use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

#[cfg(not(target_arch = "wasm32"))]
use std::thread;

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};

#[cfg(target_arch = "wasm32")]
use std::sync::OnceLock;
use symphonia::{
    core::{
        codecs::{
            CodecParameters,
            audio::{AudioDecoder, AudioDecoderOptions},
        },
        errors::Error as SymphoniaError,
        formats::{FormatOptions, SeekMode, SeekTo, TrackType, probe::Hint},
        io::{MediaSourceStream, MediaSourceStreamOptions},
        meta::{MetadataOptions, MetadataRevision, StandardTag},
        units::Time,
    },
    default::{get_codecs, get_probe},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Idle,
    Loading,
    Playing,
    Paused,
    Stopped,
    Ended,
    Error,
}

/// Best-effort tag metadata for a media file.
#[derive(Debug, Clone, Default)]
pub struct TrackMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Option<Duration>,
    /// Raw encoded image bytes (PNG/JPEG) from the first embedded visual.
    pub art: Option<Arc<Vec<u8>>>,
}

#[derive(Debug, Clone)]
pub struct PlayerSnapshot {
    pub state: PlaybackState,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub art: Option<Arc<Vec<u8>>>,
    pub path: Option<PathBuf>,
    pub position: Duration,
    pub duration: Option<Duration>,
    pub volume: f32,
    pub error: Option<String>,
}

impl Default for PlayerSnapshot {
    fn default() -> Self {
        Self {
            state: PlaybackState::Idle,
            title: None,
            artist: None,
            album: None,
            art: None,
            path: None,
            position: Duration::ZERO,
            duration: None,
            volume: 1.0,
            error: None,
        }
    }
}

#[derive(Debug)]
enum Command {
    Load(PathBuf),
    Play,
    Pause,
    Toggle,
    Stop,
    Seek(Duration),
    SetVolume(f32),
    Shutdown,
}

#[derive(Clone)]
pub struct AudioPlayer {
    tx: Sender<Command>,
    shared: Arc<Shared>,
}

struct Shared {
    status: Mutex<PlayerSnapshot>,
    is_playing: AtomicBool,
    /// Frames played at the OUTPUT sample rate since the last position base.
    played_frames: AtomicU64,
    /// Position base in output frames (set on seek), added to played_frames.
    base_frames: AtomicU64,
    output_sample_rate: AtomicU32,
    volume_bits: AtomicU32,
}

/// WASM: CPAL streams must outlive the event loop and are not `Send`.
/// Wrapping makes them storable in a static.
#[cfg(target_arch = "wasm32")]
struct SyncStream(cpal::Stream);
#[cfg(target_arch = "wasm32")]
unsafe impl Send for SyncStream {}
#[cfg(target_arch = "wasm32")]
unsafe impl Sync for SyncStream {}

#[cfg(target_arch = "wasm32")]
static AUDIO_STREAM: OnceLock<SyncStream> = OnceLock::new();

impl AudioPlayer {
    pub fn spawn() -> Result<Self> {
        let shared = Arc::new(Shared {
            status: Mutex::new(PlayerSnapshot::default()),
            is_playing: AtomicBool::new(false),
            played_frames: AtomicU64::new(0),
            base_frames: AtomicU64::new(0),
            output_sample_rate: AtomicU32::new(48_000),
            volume_bits: AtomicU32::new(1.0f32.to_bits()),
        });

        let (tx, rx) = unbounded();
        let thread_shared = shared.clone();

        #[cfg(not(target_arch = "wasm32"))]
        std::thread::Builder::new()
            .name("repadio".into())
            .spawn(move || {
                if let Err(err) = audio_thread(rx, thread_shared.clone()) {
                    thread_shared.is_playing.store(false, Ordering::Release);
                    let mut s = thread_shared.status.lock().unwrap();
                    s.state = PlaybackState::Error;
                    s.error = Some(err.to_string());
                }
            })
            .context("failed to spawn audio thread")?;

        #[cfg(target_arch = "wasm32")]
        audio_thread_wasm(rx, thread_shared).context("failed to start WASM audio")?;

        Ok(Self { tx, shared })
    }

    /// MUST be called from a user-gesture handler on WASM (click/touch)
    /// to satisfy browser autoplay policy. No-op on desktop.
    pub fn resume_audio() {
        #[cfg(target_arch = "wasm32")]
        if let Some(stream) = AUDIO_STREAM.get() {
            let _ = stream.0.play();
        }
    }

    fn send(&self, cmd: Command) -> Result<()> {
        self.tx
            .send(cmd)
            .map_err(|_| anyhow!("audio thread is not running"))
    }

    pub fn load(&self, path: impl Into<PathBuf>) -> Result<()> {
        self.send(Command::Load(path.into()))
    }
    pub fn play(&self) -> Result<()> {
        self.send(Command::Play)
    }
    pub fn pause(&self) -> Result<()> {
        self.send(Command::Pause)
    }
    pub fn toggle(&self) -> Result<()> {
        self.send(Command::Toggle)
    }
    pub fn stop(&self) -> Result<()> {
        self.send(Command::Stop)
    }
    /// NEW: seek to an absolute position in the current track.
    pub fn seek(&self, position: Duration) -> Result<()> {
        self.send(Command::Seek(position))
    }
    pub fn set_volume(&self, volume: f32) -> Result<()> {
        self.send(Command::SetVolume(volume.clamp(0.0, 2.0)))
    }

    /// Current playback position derived from output frames.
    /// This is the master clock — video sync will read this.
    pub fn position(&self) -> Duration {
        let rate = self
            .shared
            .output_sample_rate
            .load(Ordering::Acquire)
            .max(1);
        let frames = self.shared.base_frames.load(Ordering::Acquire)
            + self.shared.played_frames.load(Ordering::Acquire);
        Duration::from_secs_f64(frames as f64 / rate as f64)
    }

    pub fn is_playing(&self) -> bool {
        self.shared.is_playing.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        let mut snap = self.shared.status.lock().unwrap().clone();
        snap.position = self.position();
        snap.volume = f32::from_bits(self.shared.volume_bits.load(Ordering::Acquire));
        snap
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

pub fn probe_track_meta(path: &Path) -> TrackMeta {
    let mut meta = TrackMeta::default();

    let Ok(src) = File::open(path) else {
        return meta;
    };
    let mss = MediaSourceStream::new(Box::new(src), MediaSourceStreamOptions::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let Ok(mut reader) = get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    ) else {
        return meta;
    };

    if let Some(rev) = reader.metadata().current() {
        apply_revision(&mut meta, rev);
    }

    if let Some(track) = reader.first_track_known_codec(TrackType::Audio) {
        if let Some(CodecParameters::Audio(params)) = &track.codec_params {
            if let (Some(frames), Some(rate)) = (track.num_frames, params.sample_rate) {
                if rate > 0 {
                    meta.duration = Some(Duration::from_secs_f64(frames as f64 / rate as f64));
                }
            }
        }
    }

    meta
}

fn apply_revision(meta: &mut TrackMeta, rev: &MetadataRevision) {
    for tag in &rev.media.tags {
        match &tag.std {
            Some(StandardTag::TrackTitle(title)) if meta.title.is_none() => {
                meta.title = Some(title.to_string());
            }
            Some(StandardTag::Artist(artist)) if meta.artist.is_none() => {
                meta.artist = Some(artist.to_string());
            }
            Some(StandardTag::Album(album)) if meta.album.is_none() => {
                meta.album = Some(album.to_string());
            }
            _ => {}
        }
    }
    if meta.art.is_none() {
        if let Some(visual) = rev.media.visuals.first() {
            meta.art = Some(Arc::new(visual.data.to_vec()));
        }
    }
}

/// WASM entry point: sets up CPAL synchronously, stores the stream in a
/// static `OnceLock`, and drives decode/command processing through the
/// browser event loop via `wasm_bindgen_futures::spawn_local`.
///
/// Because WASM has no blocking threads, the decode loop runs as a
/// recursive async task.  The CPAL callback (AudioWorklet) pulls decoded
/// samples from the same bounded queue as on desktop.
#[cfg(target_arch = "wasm32")]
fn audio_thread_wasm(cmd_rx: Receiver<Command>, shared: Arc<Shared>) -> Result<()> {
    #[cfg(target_feature = "atomics")]
    let host = cpal::available_hosts()
        .iter()
        .find(|id| **id == cpal::HostId::AudioWorklet)
        .and_then(|id| cpal::host_from_id(*id).ok())
        .unwrap_or_else(cpal::default_host);
    #[cfg(not(target_feature = "atomics"))]
    let host = cpal::default_host();

    let device = host
        .default_output_device()
        .context("no default output audio device")?;

    let supported = device
        .default_output_config()
        .context("failed to query default output config")?;

    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let out_channels = config.channels as usize;
    let out_sample_rate = config.sample_rate;

    shared
        .output_sample_rate
        .store(out_sample_rate, Ordering::Release);

    let queue_seconds = 4usize;
    let queue_capacity = out_sample_rate as usize * out_channels * queue_seconds;
    let (sample_tx, sample_rx) = bounded::<f32>(queue_capacity);
    let flush_rx = sample_rx.clone();

    let err_fn = |err| web_sys::console::log_1(&format!("CPAL stream error: {err}").into());

    let stream = match sample_format {
        cpal::SampleFormat::F32 => device.build_output_stream(
            config.clone(),
            make_f32_callback(sample_rx, shared.clone(), out_channels),
            err_fn,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_output_stream(
            config.clone(),
            make_i16_callback(sample_rx, shared.clone(), out_channels),
            err_fn,
            None,
        )?,
        cpal::SampleFormat::U16 => device.build_output_stream(
            config.clone(),
            make_u16_callback(sample_rx, shared.clone(), out_channels),
            err_fn,
            None,
        )?,
        other => return Err(anyhow!("unsupported output sample format: {other:?}")),
    };

    AUDIO_STREAM
        .set(SyncStream(stream))
        .map_err(|_| anyhow!("audio stream already initialized"))?;

    // Drive command/decode processing on the browser event loop so
    // blocking I/O (file reads, symphonia decode) doesn't freeze the UI.
    // The decode loop must be async-aware on WASM; the pending-files
    // queue pattern in the app layer feeds file paths via the command
    // channel.
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(err) = audio_thread_wasm_loop(
            cmd_rx,
            sample_tx,
            flush_rx,
            shared,
            out_channels,
            out_sample_rate,
        )
        .await
        {
            web_sys::console::log_1(&format!("audio thread error: {err}").into());
        }
    });

    Ok(())
}

/// WASM decode loop — async counterpart of `audio_thread`.
/// `wasm_bindgen_futures::spawn_local` runs this cooperatively; we poll
/// commands with `try_recv` (non-blocking) and decode audio whenever a
/// `Load` arrives.  File reads and Symphonia decoding block the main
/// thread briefly, so keep decode runs short or offload via
/// `wasm-bindgen-rayon` for heavy files.
#[cfg(target_arch = "wasm32")]
async fn audio_thread_wasm_loop(
    cmd_rx: Receiver<Command>,
    sample_tx: Sender<f32>,
    flush_rx: Receiver<f32>,
    shared: Arc<Shared>,
    out_channels: usize,
    out_sample_rate: u32,
) -> Result<()> {
    use js_sys::Promise;
    use wasm_bindgen_futures::JsFuture;

    loop {
        // Non-blocking command poll — we must not stall the event loop.
        match cmd_rx.try_recv() {
            Ok(Command::Load(path)) => {
                flush_audio_queue(&flush_rx);
                let mut next = Some(path);
                while let Some(path) = next.take() {
                    next = match decode_file_to_queue(
                        path,
                        &cmd_rx,
                        &sample_tx,
                        &flush_rx,
                        shared.clone(),
                        out_channels,
                        out_sample_rate,
                    )? {
                        DecodeOutcome::Idle => None,
                        DecodeOutcome::Load(path) => Some(path),
                        DecodeOutcome::Shutdown => return Ok(()),
                    };
                }
            }
            Ok(cmd) => match apply_command(cmd, &shared, &flush_rx) {
                CommandAction::Continue => {}
                CommandAction::Seek(_) => {}
                CommandAction::Load(path) => {
                    flush_audio_queue(&flush_rx);
                    let _ = decode_file_to_queue(
                        path,
                        &cmd_rx,
                        &sample_tx,
                        &flush_rx,
                        shared.clone(),
                        out_channels,
                        out_sample_rate,
                    )?;
                }
                CommandAction::Shutdown => return Ok(()),
            },
            Err(_) => {}
        }

        // Yield to the browser event loop so the UI and AudioWorklet
        // can make progress.  5 ms is short enough to keep the audio
        // buffer from running dry during song changes.
        JsFuture::from(Promise::new(&mut |resolve, _| {
            web_sys::window()
                .unwrap()
                .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 5)
                .unwrap();
        }))
        .await
        .ok();
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn audio_thread(cmd_rx: Receiver<Command>, shared: Arc<Shared>) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default output audio device")?;

    let supported = device
        .default_output_config()
        .context("failed to query default output config")?;

    let sample_format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();

    let out_channels = config.channels as usize;
    let out_sample_rate = config.sample_rate;

    shared
        .output_sample_rate
        .store(out_sample_rate, Ordering::Release);

    let queue_seconds = 4usize;
    let queue_capacity = out_sample_rate as usize * out_channels * queue_seconds;
    let (sample_tx, sample_rx) = bounded::<f32>(queue_capacity);
    let flush_rx = sample_rx.clone();

    let err_fn = |err| eprintln!("CPAL stream error: {err}");

    let stream = match sample_format {
        cpal::SampleFormat::F32 => device.build_output_stream(
            config.clone(),
            make_f32_callback(sample_rx, shared.clone(), out_channels),
            err_fn,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_output_stream(
            config.clone(),
            make_i16_callback(sample_rx, shared.clone(), out_channels),
            err_fn,
            None,
        )?,
        cpal::SampleFormat::U16 => device.build_output_stream(
            config.clone(),
            make_u16_callback(sample_rx, shared.clone(), out_channels),
            err_fn,
            None,
        )?,
        other => return Err(anyhow!("unsupported output sample format: {other:?}")),
    };

    stream.play().context("failed to start CPAL stream")?;

    loop {
        match cmd_rx.recv() {
            Ok(Command::Load(path)) => {
                flush_audio_queue(&flush_rx);

                let mut next = Some(path);
                while let Some(path) = next.take() {
                    next = match decode_file_to_queue(
                        path,
                        &cmd_rx,
                        &sample_tx,
                        &flush_rx,
                        shared.clone(),
                        out_channels,
                        out_sample_rate,
                    )? {
                        DecodeOutcome::Idle => None,
                        DecodeOutcome::Load(path) => Some(path),
                        DecodeOutcome::Shutdown => return Ok(()),
                    };
                }
            }
            Ok(cmd) => match apply_command(cmd, &shared, &flush_rx) {
                CommandAction::Continue => {}
                // Seek with nothing loaded is a no-op.
                CommandAction::Seek(_) => {}
                CommandAction::Load(path) => {
                    flush_audio_queue(&flush_rx);
                    let _ = decode_file_to_queue(
                        path,
                        &cmd_rx,
                        &sample_tx,
                        &flush_rx,
                        shared.clone(),
                        out_channels,
                        out_sample_rate,
                    )?;
                }
                CommandAction::Shutdown => return Ok(()),
            },
            Err(_) => return Ok(()),
        }
    }
}

fn make_f32_callback(
    rx: Receiver<f32>,
    shared: Arc<Shared>,
    channels: usize,
) -> impl FnMut(&mut [f32], &cpal::OutputCallbackInfo) + Send + 'static {
    move |data, _| {
        let playing = shared.is_playing.load(Ordering::Acquire);
        let volume = f32::from_bits(shared.volume_bits.load(Ordering::Acquire));

        let mut consumed = 0u64;
        for sample in data.iter_mut() {
            *sample = if playing {
                match rx.try_recv() {
                    Ok(s) => {
                        consumed += 1;
                        s * volume
                    }
                    Err(_) => 0.0,
                }
            } else {
                0.0
            };
        }

        if playing {
            // Only count frames actually consumed → position doesn't
            // drift ahead during underruns.
            shared
                .played_frames
                .fetch_add(consumed / channels.max(1) as u64, Ordering::AcqRel);
        }
    }
}

fn make_i16_callback(
    rx: Receiver<f32>,
    shared: Arc<Shared>,
    channels: usize,
) -> impl FnMut(&mut [i16], &cpal::OutputCallbackInfo) + Send + 'static {
    move |data, _| {
        let playing = shared.is_playing.load(Ordering::Acquire);
        let volume = f32::from_bits(shared.volume_bits.load(Ordering::Acquire));

        let mut consumed = 0u64;
        for sample in data.iter_mut() {
            let s = if playing {
                match rx.try_recv() {
                    Ok(s) => {
                        consumed += 1;
                        s * volume
                    }
                    Err(_) => 0.0,
                }
            } else {
                0.0
            };
            *sample = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        }

        if playing {
            shared
                .played_frames
                .fetch_add(consumed / channels.max(1) as u64, Ordering::AcqRel);
        }
    }
}

fn make_u16_callback(
    rx: Receiver<f32>,
    shared: Arc<Shared>,
    channels: usize,
) -> impl FnMut(&mut [u16], &cpal::OutputCallbackInfo) + Send + 'static {
    move |data, _| {
        let playing = shared.is_playing.load(Ordering::Acquire);
        let volume = f32::from_bits(shared.volume_bits.load(Ordering::Acquire));

        let mut consumed = 0u64;
        for sample in data.iter_mut() {
            let s = if playing {
                match rx.try_recv() {
                    Ok(s) => {
                        consumed += 1;
                        s * volume
                    }
                    Err(_) => 0.0,
                }
            } else {
                0.0
            };
            let normalized = s.clamp(-1.0, 1.0) * 0.5 + 0.5;
            *sample = (normalized * u16::MAX as f32) as u16;
        }

        if playing {
            shared
                .played_frames
                .fetch_add(consumed / channels.max(1) as u64, Ordering::AcqRel);
        }
    }
}

enum DecodeOutcome {
    Idle,
    Load(PathBuf),
    Shutdown,
}

fn decode_file_to_queue(
    path: PathBuf,
    cmd_rx: &Receiver<Command>,
    sample_tx: &Sender<f32>,
    flush_rx: &Receiver<f32>,
    shared: Arc<Shared>,
    out_channels: usize,
    out_rate: u32,
) -> Result<DecodeOutcome> {
    {
        let mut s = shared.status.lock().unwrap();
        s.state = PlaybackState::Loading;
        s.path = Some(path.clone());
        s.title = path.file_name().map(|v| v.to_string_lossy().to_string());
        s.artist = None;
        s.album = None;
        s.art = None;
        s.position = Duration::ZERO;
        s.duration = None;
        s.error = None;
    }

    shared.played_frames.store(0, Ordering::Release);
    shared.base_frames.store(0, Ordering::Release);
    shared.is_playing.store(true, Ordering::Release);

    let src = File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(src), MediaSourceStreamOptions::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mut format = get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .context("unsupported or unreadable media container")?;
    {
        let mut meta = TrackMeta::default();
        if let Some(rev) = format.metadata().current() {
            apply_revision(&mut meta, rev);
        }

        let mut s = shared.status.lock().unwrap();
        if let Some(t) = meta.title {
            s.title = Some(t);
        }
        s.artist = meta.artist;
        s.album = meta.album;
        s.art = meta.art;
    }

    let track = format
        .default_track(TrackType::Audio)
        .or_else(|| format.first_track_known_codec(TrackType::Audio))
        .context("no supported audio track")?;

    let track_id = track.id;
    let codec_params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p.clone(),
        _ => anyhow::bail!("track has no audio codec parameters"),
    };

    let duration = match (track.num_frames, codec_params.sample_rate) {
        (Some(frames), Some(rate)) if rate > 0 => {
            Some(Duration::from_secs_f64(frames as f64 / rate as f64))
        }
        _ => None,
    };

    {
        let mut s = shared.status.lock().unwrap();
        s.state = PlaybackState::Playing;
        s.duration = duration;
    }

    let mut decoder = get_codecs()
        .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
        .context("failed to create decoder")?;

    loop {
        match drain_commands(cmd_rx, &shared, flush_rx) {
            CommandAction::Continue => {}
            CommandAction::Load(path) => return Ok(DecodeOutcome::Load(path)),
            CommandAction::Shutdown => return Ok(DecodeOutcome::Shutdown),
            CommandAction::Seek(target) => {
                perform_seek(
                    &mut *format,
                    &mut *decoder,
                    track_id,
                    target,
                    duration,
                    &shared,
                    flush_rx,
                    out_rate,
                );
            }
        }

        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => {
                shared.is_playing.store(false, Ordering::Release);
                let mut s = shared.status.lock().unwrap();
                if s.state != PlaybackState::Stopped {
                    s.state = PlaybackState::Ended;
                }
                return Ok(DecodeOutcome::Idle);
            }
            Err(SymphoniaError::IoError(_)) => {
                shared.is_playing.store(false, Ordering::Release);
                let mut s = shared.status.lock().unwrap();
                if s.state != PlaybackState::Stopped {
                    s.state = PlaybackState::Ended;
                }
                return Ok(DecodeOutcome::Idle);
            }
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(err) => {
                let mut s = shared.status.lock().unwrap();
                s.state = PlaybackState::Error;
                s.error = Some(err.to_string());
                shared.is_playing.store(false, Ordering::Release);
                return Ok(DecodeOutcome::Idle);
            }
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(_)) => continue,
            Err(SymphoniaError::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(err) => {
                let mut s = shared.status.lock().unwrap();
                s.state = PlaybackState::Error;
                s.error = Some(err.to_string());
                shared.is_playing.store(false, Ordering::Release);
                return Ok(DecodeOutcome::Idle);
            }
        };

        let spec = decoded.spec();
        let in_channels = spec.channels().count().max(1);
        let in_rate = spec.rate().max(1);

        let mut f32_samples = Vec::new();
        decoded.copy_to_vec_interleaved::<f32>(&mut f32_samples);

        let converted =
            convert_channels_and_rate(&f32_samples, in_channels, in_rate, out_channels, out_rate);

        for mut sample in converted {
            loop {
                match sample_tx.try_send(sample) {
                    Ok(()) => break,
                    Err(TrySendError::Full(s)) => {
                        sample = s;

                        match drain_commands(cmd_rx, &shared, flush_rx) {
                            CommandAction::Continue => {}
                            CommandAction::Load(path) => return Ok(DecodeOutcome::Load(path)),
                            CommandAction::Shutdown => return Ok(DecodeOutcome::Shutdown),
                            CommandAction::Seek(target) => {
                                perform_seek(
                                    &mut *format,
                                    &mut *decoder,
                                    track_id,
                                    target,
                                    duration,
                                    &shared,
                                    flush_rx,
                                    out_rate,
                                );
                                // Drop the in-flight sample; it's pre-seek audio.
                                break;
                            }
                        }

                        // Yield so the audio callback can drain the queue.
                        #[cfg(not(target_arch = "wasm32"))]
                        thread::sleep(Duration::from_millis(2));
                        #[cfg(target_arch = "wasm32")]
                        std::thread::yield_now();
                    }
                    Err(TrySendError::Disconnected(_)) => return Ok(DecodeOutcome::Shutdown),
                }
            }
        }
    }
}

/// NEW: execute a seek against the open demuxer.
#[allow(clippy::too_many_arguments)]
fn perform_seek(
    format: &mut dyn symphonia::core::formats::FormatReader,
    decoder: &mut dyn AudioDecoder,
    track_id: u32,
    target: Duration,
    duration: Option<Duration>,
    shared: &Arc<Shared>,
    flush_rx: &Receiver<f32>,
    out_rate: u32,
) {
    let clamped = match duration {
        Some(d) if target > d => d,
        _ => target,
    };

    let time = Time::try_from_secs_f64(clamped.as_secs_f64()).unwrap_or(Time::ZERO);

    match format.seek(
        SeekMode::Accurate,
        SeekTo::Time {
            time,
            track_id: Some(track_id),
        },
    ) {
        Ok(_seeked_to) => {
            decoder.reset();
            // Drop already-queued (pre-seek) audio.
            flush_audio_queue(flush_rx);
            // Re-base the position clock at the seek target.
            let base = (clamped.as_secs_f64() * out_rate as f64) as u64;
            shared.base_frames.store(base, Ordering::Release);
            shared.played_frames.store(0, Ordering::Release);
        }
        Err(err) => {
            let mut s = shared.status.lock().unwrap();
            s.error = Some(format!("seek failed: {err}"));
        }
    }
}

enum CommandAction {
    Continue,
    Load(PathBuf),
    Seek(Duration),
    Shutdown,
}

fn drain_commands(
    cmd_rx: &Receiver<Command>,
    shared: &Arc<Shared>,
    flush_rx: &Receiver<f32>,
) -> CommandAction {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match apply_command(cmd, shared, flush_rx) {
            CommandAction::Continue => {}
            other => return other,
        }
    }
    CommandAction::Continue
}

fn apply_command(cmd: Command, shared: &Arc<Shared>, flush_rx: &Receiver<f32>) -> CommandAction {
    match cmd {
        Command::Load(path) => CommandAction::Load(path),
        Command::Seek(pos) => CommandAction::Seek(pos),

        Command::Play => {
            shared.is_playing.store(true, Ordering::Release);
            let mut s = shared.status.lock().unwrap();
            if matches!(
                s.state,
                PlaybackState::Paused | PlaybackState::Stopped | PlaybackState::Ended
            ) {
                s.state = PlaybackState::Playing;
            }
            CommandAction::Continue
        }

        Command::Pause => {
            shared.is_playing.store(false, Ordering::Release);
            let mut s = shared.status.lock().unwrap();
            if s.state == PlaybackState::Playing {
                s.state = PlaybackState::Paused;
            }
            CommandAction::Continue
        }

        Command::Toggle => {
            let now_playing = shared.is_playing.load(Ordering::Acquire);
            shared.is_playing.store(!now_playing, Ordering::Release);
            let mut s = shared.status.lock().unwrap();
            s.state = if now_playing {
                PlaybackState::Paused
            } else {
                PlaybackState::Playing
            };
            CommandAction::Continue
        }

        Command::Stop => {
            shared.is_playing.store(false, Ordering::Release);
            shared.played_frames.store(0, Ordering::Release);
            shared.base_frames.store(0, Ordering::Release);
            flush_audio_queue(flush_rx);
            let mut s = shared.status.lock().unwrap();
            s.state = PlaybackState::Stopped;
            s.position = Duration::ZERO;
            CommandAction::Continue
        }

        Command::SetVolume(v) => {
            let v = v.clamp(0.0, 2.0);
            shared.volume_bits.store(v.to_bits(), Ordering::Release);
            let mut s = shared.status.lock().unwrap();
            s.volume = v;
            CommandAction::Continue
        }

        Command::Shutdown => CommandAction::Shutdown,
    }
}

fn flush_audio_queue(rx: &Receiver<f32>) {
    while rx.try_recv().is_ok() {}
}

fn convert_channels_and_rate(
    input: &[f32],
    in_channels: usize,
    in_rate: u32,
    out_channels: usize,
    out_rate: u32,
) -> Vec<f32> {
    let in_channels = in_channels.max(1);
    let out_channels = out_channels.max(1);

    let in_frames = input.len() / in_channels;
    if in_frames == 0 {
        return Vec::new();
    }

    let out_frames = if in_rate == out_rate {
        in_frames
    } else {
        ((in_frames as u64 * out_rate as u64 + in_rate as u64 - 1) / in_rate as u64) as usize
    };

    let mut out = Vec::with_capacity(out_frames * out_channels);

    for out_frame in 0..out_frames {
        let src_pos = if in_rate == out_rate {
            out_frame as f64
        } else {
            out_frame as f64 * in_rate as f64 / out_rate as f64
        };

        let i0 = src_pos.floor() as usize;
        let i1 = (i0 + 1).min(in_frames - 1);
        let frac = (src_pos - i0 as f64) as f32;

        for out_ch in 0..out_channels {
            let a = read_mapped_sample(
                input,
                i0.min(in_frames - 1),
                out_ch,
                in_channels,
                out_channels,
            );
            let b = read_mapped_sample(input, i1, out_ch, in_channels, out_channels);
            out.push(a + (b - a) * frac);
        }
    }

    out
}

fn read_mapped_sample(
    input: &[f32],
    frame: usize,
    out_ch: usize,
    in_channels: usize,
    out_channels: usize,
) -> f32 {
    let base = frame * in_channels;

    if out_channels == 1 && in_channels > 1 {
        let mut sum = 0.0;
        for ch in 0..in_channels {
            sum += input.get(base + ch).copied().unwrap_or(0.0);
        }
        sum / in_channels as f32
    } else if in_channels == 1 {
        input.get(base).copied().unwrap_or(0.0)
    } else {
        let ch = out_ch.min(in_channels - 1);
        input.get(base + ch).copied().unwrap_or(0.0)
    }
}
