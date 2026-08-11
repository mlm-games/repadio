use std::{
    fs::File,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

use web_time::Instant;

#[cfg(not(target_arch = "wasm32"))]
use std::thread;
#[cfg(target_arch = "wasm32")]
use web_thread as thread;

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};

use symphonia::{
    core::{
        codecs::{
            CodecParameters,
            audio::{AudioDecoder, AudioDecoderOptions},
            registry::CodecRegistry,
            video::well_known as video_codec_ids,
        },
        errors::Error as SymphoniaError,
        formats::{FormatOptions, SeekMode, SeekTo, TrackType, probe::Hint},
        io::{MediaSourceStream, MediaSourceStreamOptions},
        meta::{MetadataOptions, MetadataRevision, StandardTag},
        packet::Packet as SymphoniaPacket,
        units::{Time, TimeBase},
    },
    default::get_probe,
};

fn get_codecs() -> &'static CodecRegistry {
    static REGISTRY: OnceLock<CodecRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = CodecRegistry::new();
        symphonia::default::register_enabled_codecs(&mut registry);
        registry.register_audio_decoder::<symphonia_adapter_oporus::OpusDecoder>();
        registry
    })
}

const MIN_VIDEO_FRAMES_PREROLL: u64 = 8;
const PREROLL_MS: u32 = 500;

pub mod video;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Idle,
    Loading,
    Buffering,
    Playing,
    Paused,
    Ended,
    Error,
}

/// A media source that the decoder can open.
/// Abstracts over native filesystem paths and in-memory byte buffers so
/// the same pipeline works on desktop, WASM (browser blobs / OPFS), and
/// Android (content:// URIs).
#[derive(Debug, Clone, PartialEq, Hash)]
pub enum MediaSource {
    Path(PathBuf),
    Bytes { name: String, bytes: Arc<[u8]> },
}

impl From<PathBuf> for MediaSource {
    fn from(p: PathBuf) -> Self {
        MediaSource::Path(p)
    }
}

impl From<&Path> for MediaSource {
    fn from(p: &Path) -> Self {
        MediaSource::Path(p.to_path_buf())
    }
}

impl MediaSource {
    /// A human-readable unique identifier for the source file
    /// (file name for paths, the `name` field for byte blobs).
    pub fn display_name(&self) -> String {
        match self {
            MediaSource::Path(p) => p.to_string_lossy().to_string(),
            MediaSource::Bytes { name, .. } => name.clone(),
        }
    }
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
    pub source: Option<MediaSource>,
    pub position: Duration,
    pub duration: Option<Duration>,
    pub volume: f32,
    pub muted: bool,
    pub playback_rate: f32,
    pub has_video: bool,
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
            source: None,
            position: Duration::ZERO,
            duration: None,
            volume: 1.0,
            muted: false,
            playback_rate: 1.0,
            has_video: false,
            error: None,
        }
    }
}

#[derive(Debug)]
enum Command {
    Load(MediaSource),
    Play,
    Pause,
    Toggle,
    Seek(Duration, u64),
    SetVolume(f32),
    SetMuted(bool),
    ToggleMute,
    SetSpeed(f32),
    Shutdown,
}

#[derive(Clone)]
pub struct AudioPlayer {
    inner: Arc<AudioPlayerInner>,
    video_rx: crossbeam_channel::Receiver<video::DecodedVideoFrame>,
}

struct AudioPlayerInner {
    tx: Sender<Command>,
    shared: Arc<Shared>,
    #[allow(dead_code)]
    video_tx: crossbeam_channel::Sender<video::DecodedVideoFrame>,
}

impl Drop for AudioPlayerInner {
    fn drop(&mut self) {
        self.tx.send(Command::Shutdown).ok();
    }
}

impl AudioPlayer {
    pub fn video_rx(&self) -> crossbeam_channel::Receiver<video::DecodedVideoFrame> {
        self.video_rx.clone()
    }

    pub fn seek_serial(&self) -> u64 {
        self.inner.shared.seek_serial.load(Ordering::Acquire)
    }

    pub fn load_serial(&self) -> u64 {
        self.inner.shared.load_serial.load(Ordering::Acquire)
    }

    /// Deprecated: use `load_serial()` instead.
    pub fn generation(&self) -> u64 {
        self.load_serial()
    }
}

struct Shared {
    status: Mutex<PlayerSnapshot>,
    is_playing: AtomicBool,
    /// Frames played at the OUTPUT sample rate since the last position base.
    played_frames: AtomicU64,
    /// Media-time position base in output frames (set on seek), added to
    /// `played_frames * playback_rate` to derive the current position.
    base_media_frames: AtomicU64,
    output_sample_rate: AtomicU32,
    /// Current output gain applied in the CPAL callback. Zeroed while muted.
    volume_bits: AtomicU32,
    /// Logical volume to restore on unmute (never zeroed by mute).
    unmute_volume_bits: AtomicU32,
    muted: AtomicBool,
    /// Playback rate in (0.25 ..= 4.0), 1.0 = normal.
    playback_rate_bits: AtomicU32,
    /// Bumped on every `set_speed` so the decode loop rebuilds the resampler.
    speed_serial: AtomicU64,
    /// Incremented on every `decode_file_to_queue` call to signal a new
    /// file load. VideoSink uses this to distinguish new-file from seek.
    load_serial: AtomicU64,
    /// Monotonically increasing serial for seek ordering.  Bumped before
    /// every `Command::Seek` is sent.  `perform_seek` checks this to avoid
    /// overwriting a newer seek's optimistic clock rebase.
    seek_serial: AtomicU64,
    /// Whether playback should resume after buffering completes.
    /// Can be changed by Play/Pause during Buffering state.
    resume_intent: AtomicBool,

    /// Whether a video track was found in the current file.
    has_video: AtomicBool,
    /// Incremented on every video frame successfully sent to the video channel.
    /// The audio thread can check this without waiting for the UI thread.
    video_frames_sent: AtomicU64,
}

// CPAL streams are not `Send`/`Sync`, so need thread-local storage for wasm.
#[cfg(target_arch = "wasm32")]
thread_local! {
    static AUDIO_STREAM: std::cell::OnceCell<cpal::Stream> = const { std::cell::OnceCell::new() };
}

impl AudioPlayer {
    pub fn spawn() -> Result<Self> {
        let shared = Arc::new(Shared {
            status: Mutex::new(PlayerSnapshot::default()),
            is_playing: AtomicBool::new(false),
            played_frames: AtomicU64::new(0),
            base_media_frames: AtomicU64::new(0),
            output_sample_rate: AtomicU32::new(48_000),
            volume_bits: AtomicU32::new(1.0f32.to_bits()),
            unmute_volume_bits: AtomicU32::new(1.0f32.to_bits()),
            muted: AtomicBool::new(false),
            playback_rate_bits: AtomicU32::new(1.0f32.to_bits()),
            speed_serial: AtomicU64::new(0),
            load_serial: AtomicU64::new(0),
            seek_serial: AtomicU64::new(0),
            resume_intent: AtomicBool::new(true),

            has_video: AtomicBool::new(false),
            video_frames_sent: AtomicU64::new(0),
        });

        let (tx, rx) = unbounded();
        let (video_tx, video_rx) = crossbeam_channel::bounded(256);
        let thread_shared = shared.clone();

        #[cfg(not(target_arch = "wasm32"))]
        {
            let (stream, sample_tx, flush_rx, out_channels, out_sample_rate) =
                create_cpal_stream(&shared)?;

            let thread_video_tx = video_tx.clone();

            std::thread::Builder::new()
                .name("repadio-audio".into())
                .spawn({
                    let err_shared = thread_shared.clone();

                    move || {
                        let stream = stream;

                        let result = run_command_loop(
                            &rx,
                            &sample_tx,
                            &flush_rx,
                            &thread_shared,
                            out_channels,
                            out_sample_rate,
                            &thread_video_tx,
                        );

                        drop(stream);

                        if let Err(err) = result {
                            set_error(err_shared.as_ref(), &err);
                            log::error!("audio thread: {err}");
                        }
                    }
                })
                .context("failed to spawn audio thread")?;
        }

        #[cfg(target_arch = "wasm32")]
        audio_thread_wasm(rx, thread_shared, &video_tx).context("failed to start WASM audio")?;

        Ok(Self {
            inner: Arc::new(AudioPlayerInner {
                tx,
                shared,
                video_tx,
            }),
            video_rx,
        })
    }

    /// MUST be called from a user-gesture handler on WASM (click/touch)
    /// to satisfy browser autoplay policy. No-op on desktop.
    pub fn resume_audio() {
        #[cfg(target_arch = "wasm32")]
        AUDIO_STREAM.with(|cell| {
            if let Some(stream) = cell.get() {
                let _ = stream.play();
            }
        });
    }

    fn send(&self, cmd: Command) -> Result<()> {
        self.inner
            .tx
            .send(cmd)
            .map_err(|_| anyhow!("audio thread is not running"))
    }

    pub fn load(&self, path: impl Into<MediaSource>) -> Result<()> {
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
    pub fn seek(&self, position: Duration) -> Result<()> {
        // Optimistically update the position clock so the UI doesn't flicker
        // back to the old position while the audio thread processes the seek.
        let shared = &self.inner.shared;
        let rate = shared.output_sample_rate.load(Ordering::Acquire).max(1);
        let base = (position.as_secs_f64() * rate as f64) as u64;
        shared.base_media_frames.store(base, Ordering::Release);
        shared.played_frames.store(0, Ordering::Release);
        let serial = shared.seek_serial.fetch_add(1, Ordering::AcqRel) + 1;
        self.send(Command::Seek(position, serial))
    }
    pub fn set_volume(&self, volume: f32) -> Result<()> {
        let v = volume.clamp(0.0, 2.0);
        let shared = &self.inner.shared;
        // Keep the logical volume independent of mute so unmute restores it.
        shared.unmute_volume_bits.store(v.to_bits(), Ordering::Release);
        if !shared.muted.load(Ordering::Acquire) {
            shared.volume_bits.store(v.to_bits(), Ordering::Release);
        }
        {
            let mut s = lock_status(shared);
            s.volume = v;
        }
        self.send(Command::SetVolume(v))
    }
    pub fn set_muted(&self, muted: bool) -> Result<()> {
        self.send(Command::SetMuted(muted))
    }
    pub fn toggle_mute(&self) -> Result<()> {
        self.send(Command::ToggleMute)
    }
    /// Playback rate in `0.25 ..= 4.0`; `1.0` is normal speed.
    pub fn set_speed(&self, speed: f32) -> Result<()> {
        let s = speed.clamp(0.25, 4.0);
        // Fold wall progress into the media base so the clock doesn't jump.
        let shared = &self.inner.shared;
        let old = f32::from_bits(shared.playback_rate_bits.load(Ordering::Acquire));
        let played = shared.played_frames.load(Ordering::Acquire);
        let add = (played as f64 * old as f64).round() as u64;
        shared.base_media_frames.fetch_add(add, Ordering::AcqRel);
        shared.played_frames.store(0, Ordering::Release);
        shared.playback_rate_bits.store(s.to_bits(), Ordering::Release);
        shared.speed_serial.fetch_add(1, Ordering::Release);
        {
            let mut st = lock_status(shared);
            st.playback_rate = s;
        }
        self.send(Command::SetSpeed(s))
    }
    pub fn playback_rate(&self) -> f32 {
        f32::from_bits(
            self.inner
                .shared
                .playback_rate_bits
                .load(Ordering::Acquire),
        )
    }
    pub fn has_video(&self) -> bool {
        self.inner.shared.has_video.load(Ordering::Acquire)
    }

    /// Current playback position derived from output frames.
    /// For video-only files this advances via injected silence samples.
    /// Advances at `wall_seconds * playback_rate`.
    pub fn position(&self) -> Duration {
        let shared = &self.inner.shared;
        let rate = shared.output_sample_rate.load(Ordering::Acquire).max(1) as f64;
        let speed = f32::from_bits(shared.playback_rate_bits.load(Ordering::Acquire)) as f64;
        let frames = shared.base_media_frames.load(Ordering::Acquire) as f64
            + shared.played_frames.load(Ordering::Acquire) as f64 * speed;
        Duration::from_secs_f64(frames / rate)
    }

    pub fn is_playing(&self) -> bool {
        self.inner.shared.is_playing.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        let shared = &self.inner.shared;

        let mut snap = lock_status(shared).clone();
        snap.position = self.position();
        snap.volume = f32::from_bits(shared.unmute_volume_bits.load(Ordering::Acquire));
        snap.muted = shared.muted.load(Ordering::Acquire);
        snap.playback_rate = f32::from_bits(shared.playback_rate_bits.load(Ordering::Acquire));
        snap.has_video = shared.has_video.load(Ordering::Acquire);
        snap
    }
}

/// Convert a `MediaSource` into a Symphonia `MediaSourceStream` + `Hint`.
fn media_source_stream(src: MediaSource) -> Result<(MediaSourceStream<'static>, Hint)> {
    match src {
        MediaSource::Path(path) => {
            let file =
                File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
            let mut hint = Hint::new();
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                hint.with_extension(&ext.to_lowercase());
            }
            Ok((
                MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default()),
                hint,
            ))
        }
        MediaSource::Bytes { name, bytes } => {
            let cursor = Cursor::new(bytes);
            let mut hint = Hint::new();
            if let Some(ext) = std::path::Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
            {
                hint.with_extension(&ext.to_lowercase());
            }
            Ok((
                MediaSourceStream::new(Box::new(cursor), MediaSourceStreamOptions::default()),
                hint,
            ))
        }
    }
}

pub fn probe_track_meta(path: &Path) -> TrackMeta {
    probe_media_source(MediaSource::Path(path.to_path_buf()))
}

pub fn probe_media_source(source: MediaSource) -> TrackMeta {
    let mut meta = TrackMeta::default();

    let Ok((mss, hint)) = media_source_stream(source) else {
        return meta;
    };

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

    if let Some(track) = reader.first_track_known_codec(TrackType::Audio)
        && let Some(CodecParameters::Audio(params)) = &track.codec_params
        && let (Some(frames), Some(rate)) = (track.num_frames, params.sample_rate)
        && rate > 0
    {
        meta.duration = Some(Duration::from_secs_f64(frames as f64 / rate as f64));
    } else if let Some(track) = reader.first_track_known_codec(TrackType::Video) {
        meta.duration = track_duration_secs(track);
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
    if meta.art.is_none()
        && let Some(visual) = rev.media.visuals.first()
    {
        meta.art = Some(Arc::new(visual.data.to_vec()));
    }
}

fn track_duration_secs(track: &symphonia::core::formats::Track) -> Option<Duration> {
    let tb = track.time_base?;
    if let Some(d) = track.duration {
        let secs = d.get() as f64 * tb.numer.get() as f64 / tb.denom.get() as f64;
        if secs > 0.0 {
            return Some(Duration::from_secs_f64(secs));
        }
    }
    if let Some(nf) = track.num_frames {
        let secs = nf as f64 * tb.numer.get() as f64 / tb.denom.get() as f64;
        if secs > 0.0 {
            return Some(Duration::from_secs_f64(secs));
        }
    }
    None
}

/// WASM entry point: sets up CPAL synchronously, stores the stream in a
/// static `OnceLock`, and spawns the decode/command loop onto a real
/// Web Worker via `web_thread` so that blocking I/O and channel waits

#[cfg(target_arch = "wasm32")]
fn audio_thread_wasm(
    cmd_rx: Receiver<Command>,
    shared: Arc<Shared>,
    video_tx: &crossbeam_channel::Sender<video::DecodedVideoFrame>,
) -> Result<()> {
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
            config,
            make_f32_callback(sample_rx, shared.clone(), out_channels),
            err_fn,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_output_stream(
            config,
            make_i16_callback(sample_rx, shared.clone(), out_channels),
            err_fn,
            None,
        )?,
        cpal::SampleFormat::U16 => device.build_output_stream(
            config,
            make_u16_callback(sample_rx, shared.clone(), out_channels),
            err_fn,
            None,
        )?,
        other => return Err(anyhow!("unsupported output sample format: {other:?}")),
    };

    AUDIO_STREAM.with(|cell| {
        cell.set(stream)
            .map_err(|_| anyhow!("audio stream already initialized"))
    })?;

    // Real thread (Web Worker) via web_thread. blocking I/O, channel
    // sends, and thread::sleep all work correctly here.
    let thread_video_tx = video_tx.clone();
    web_thread::spawn(move || {
        let result = run_command_loop(
            &cmd_rx,
            &sample_tx,
            &flush_rx,
            &shared,
            out_channels,
            out_sample_rate,
            &thread_video_tx,
        );
        if let Err(err) = result {
            set_error(shared.as_ref(), &err);
            web_sys::console::log_1(&format!("audio thread error: {err}").into());
        }
    });

    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::type_complexity)]
fn create_cpal_stream(
    shared: &Arc<Shared>,
) -> Result<(cpal::Stream, Sender<f32>, Receiver<f32>, usize, u32)> {
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
            config,
            make_f32_callback(sample_rx, shared.clone(), out_channels),
            err_fn,
            None,
        )?,
        cpal::SampleFormat::I16 => device.build_output_stream(
            config,
            make_i16_callback(sample_rx, shared.clone(), out_channels),
            err_fn,
            None,
        )?,
        cpal::SampleFormat::U16 => device.build_output_stream(
            config,
            make_u16_callback(sample_rx, shared.clone(), out_channels),
            err_fn,
            None,
        )?,
        other => return Err(anyhow!("unsupported output sample format: {other:?}")),
    };

    stream.play().context("failed to start CPAL stream")?;

    Ok((stream, sample_tx, flush_rx, out_channels, out_sample_rate))
}

fn lock_status(shared: &Shared) -> std::sync::MutexGuard<'_, PlayerSnapshot> {
    shared.status.lock().unwrap_or_else(|e| e.into_inner())
}

fn set_error(shared: &Shared, err: &(impl std::fmt::Display + ?Sized)) {
    shared.is_playing.store(false, Ordering::Release);
    let mut s = lock_status(shared);
    s.state = PlaybackState::Error;
    s.error = Some(err.to_string());
}

fn run_command_loop(
    cmd_rx: &Receiver<Command>,
    sample_tx: &Sender<f32>,
    flush_rx: &Receiver<f32>,
    shared: &Arc<Shared>,
    out_channels: usize,
    out_sample_rate: u32,
    video_tx: &crossbeam_channel::Sender<video::DecodedVideoFrame>,
) -> Result<()> {
    loop {
        match cmd_rx.recv() {
            Ok(Command::Load(path)) => {
                log::info!("audio thread processing load: {}", path.display_name());
                let mut next = Some(path);
                let mut pending_seek: Option<(Duration, u64)> = None;
                'decode: while let Some(path) = next.take() {
                    match decode_file_to_queue(
                        path,
                        cmd_rx,
                        sample_tx,
                        flush_rx,
                        shared.clone(),
                        out_channels,
                        out_sample_rate,
                        video_tx,
                        pending_seek.take(),
                    ) {
                        Ok(DecodeOutcome::Idle) => next = None,
                        Ok(DecodeOutcome::Seek(target, serial)) => {
                            pending_seek = Some((target, serial));
                            next = lock_status(shared).source.clone();
                        }
                        Ok(DecodeOutcome::Load(path)) => next = Some(path),
                        Ok(DecodeOutcome::Shutdown) => return Ok(()),
                        Err(err) => {
                            log::error!("decode error: {err}");
                            set_error(shared.as_ref(), &err);
                            break 'decode;
                        }
                    }
                }
            }
            Ok(cmd) => match apply_command(cmd, shared) {
                CommandAction::Continue => {}
                CommandAction::Seek(..) => {}
                CommandAction::Load(path) => {
                    if let Err(err) = decode_file_to_queue(
                        path,
                        cmd_rx,
                        sample_tx,
                        flush_rx,
                        shared.clone(),
                        out_channels,
                        out_sample_rate,
                        video_tx,
                        None,
                    ) {
                        log::error!("decode error: {err}");
                        set_error(shared.as_ref(), &err);
                    }
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

        let mut consumed = 0usize;
        if playing {
            while consumed + channels <= data.len() && rx.len() >= channels {
                for sample in data[consumed..consumed + channels].iter_mut() {
                    *sample = rx.recv().unwrap() * volume;
                }
                consumed += channels;
            }
        }
        for sample in data[consumed..].iter_mut() {
            *sample = 0.0;
        }

        shared
            .played_frames
            .fetch_add((consumed / channels.max(1)) as u64, Ordering::AcqRel);
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

        let mut consumed = 0usize;
        if playing {
            while consumed + channels <= data.len() && rx.len() >= channels {
                for sample in data[consumed..consumed + channels].iter_mut() {
                    let s = (rx.recv().unwrap() * volume).clamp(-1.0, 1.0);
                    *sample = (s * i16::MAX as f32) as i16;
                }
                consumed += channels;
            }
        }
        for sample in data[consumed..].iter_mut() {
            *sample = 0;
        }

        shared
            .played_frames
            .fetch_add((consumed / channels.max(1)) as u64, Ordering::AcqRel);
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

        let mut consumed = 0usize;
        if playing {
            while consumed + channels <= data.len() && rx.len() >= channels {
                for sample in data[consumed..consumed + channels].iter_mut() {
                    let s = (rx.recv().unwrap() * volume).clamp(-1.0, 1.0) * 0.5 + 0.5;
                    *sample = (s * u16::MAX as f32) as u16;
                }
                consumed += channels;
            }
        }
        for sample in data[consumed..].iter_mut() {
            *sample = u16::MAX / 2;
        }

        shared
            .played_frames
            .fetch_add((consumed / channels.max(1)) as u64, Ordering::AcqRel);
    }
}

enum DecodeOutcome {
    Idle,
    Seek(Duration, u64),
    Load(MediaSource),
    Shutdown,
}

enum VideoCodecKind {
    H264 { nal_len_size: usize },
    Hevc { nal_len_size: usize },
    Av1,
}

struct VideoDecodeState {
    track_id: u32,
    decoder: video::VideoDecoder,
    time_base: TimeBase,
    codec: VideoCodecKind,
    need_keyframe: bool,
    /// GCD of all non-zero packet PTS ticks seen so far.
    /// Used to derive the true frame duration in µs.
    gcd_pts_ticks: u64,
    /// Number of non-zero PTS ticks seen so far.  Once >= 2, the GCD has
    /// converged to the true frame duration and POC correction is activated.
    non_zero_pts_seen: u64,
    /// Cached frame_duration_us computed from gcd_pts_ticks + time_base.
    /// Only meaningful when `non_zero_pts_seen >= 2`.
    frame_duration_us: u64,
}

/// State for the two-phase accurate seek / buffering.
/// Decoded output before `target` is discarded; playback starts once the
/// audio stream reaches `target` AND at least `min_samples` are queued
/// AND video has produced a few presentable frames (or a timeout fires).
struct SeekPhase {
    target: Duration,
    min_samples: usize,
    audio_reached: bool,
    started: Instant,
}

impl SeekPhase {
    fn new(target: Duration, out_rate: u32, out_channels: usize) -> Self {
        Self {
            target,
            min_samples: (out_rate as usize * out_channels * PREROLL_MS as usize) / 1000,
            audio_reached: target.is_zero(),
            started: Instant::now(),
        }
    }
}

fn video_preroll_ok(shared: &Shared) -> bool {
    if !shared.has_video.load(Ordering::Acquire) {
        return true;
    }
    shared.video_frames_sent.load(Ordering::Acquire) >= MIN_VIDEO_FRAMES_PREROLL
}

fn packet_pts_us(packet: &SymphoniaPacket, tb: &TimeBase) -> i64 {
    let time = tb.calc_time_saturating(packet.pts);
    (time.as_secs_f64() * 1_000_000.0) as i64
}

fn h264_avcc_has_idr(data: &[u8], nal_len_size: usize) -> bool {
    let mut i = 0usize;
    while i + nal_len_size <= data.len() {
        let mut n = 0usize;
        for &b in &data[i..i + nal_len_size] {
            n = (n << 8) | b as usize;
        }
        i += nal_len_size;
        if n == 0 || i + n > data.len() {
            return false;
        }
        let nal_type = data[i] & 0x1f;
        if nal_type == 5 {
            return true;
        }
        i += n;
    }
    false
}

fn hevc_hvcc_has_keyframe(data: &[u8], nal_len_size: usize) -> bool {
    let mut i = 0usize;
    while i + nal_len_size <= data.len() {
        let mut n = 0usize;
        for &b in &data[i..i + nal_len_size] {
            n = (n << 8) | b as usize;
        }
        i += nal_len_size;
        if n == 0 || i + n > data.len() {
            return false;
        }
        let nal_type = (data[i] >> 1) & 0x3f;
        if matches!(nal_type, 19 | 20 | 21) {
            return true;
        }
        i += n;
    }
    false
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

fn handle_video_packet(
    state: &mut VideoDecodeState,
    packet: &SymphoniaPacket,
    video_tx: &crossbeam_channel::Sender<video::DecodedVideoFrame>,
    load_serial: u64,
    min_pts: Option<Duration>,
    shared: &Shared,
) {
    let is_sync = match state.codec {
        VideoCodecKind::H264 { nal_len_size } => h264_avcc_has_idr(&packet.data, nal_len_size),
        VideoCodecKind::Hevc { nal_len_size } => hevc_hvcc_has_keyframe(&packet.data, nal_len_size),
        VideoCodecKind::Av1 => true,
    };

    if state.need_keyframe {
        if !is_sync {
            return;
        }
        state.need_keyframe = false;
    }

    let pts_us = packet_pts_us(packet, &state.time_base);
    let fallback = Duration::from_micros(pts_us.max(0) as u64);

    // Track GCD of packet PTS ticks to derive correct frame duration.
    // Container PTS may follow B-frame decode order even for no-B-frame
    // bitstreams, and the GCD gives the true per-frame tick increment.
    // We defer POC-based PTS correction until we've seen 2+ non-zero PTS
    // ticks (at which point the GCD has converged to the true frame duration).
    let pts_ticks = packet.pts.get();
    if pts_ticks > 0 {
        if state.gcd_pts_ticks == 0 {
            state.gcd_pts_ticks = pts_ticks as u64;
        } else {
            state.gcd_pts_ticks = gcd(state.gcd_pts_ticks, pts_ticks as u64);
        }
        state.non_zero_pts_seen += 1;
    }
    if state.non_zero_pts_seen >= 2 {
        let new_fd = (state.gcd_pts_ticks * state.time_base.numer.get() as u64 * 1_000_000)
            / state.time_base.denom.get() as u64;
        if new_fd != state.frame_duration_us {
            state.frame_duration_us = new_fd;
            state.decoder.set_frame_duration_micros(new_fd);
        }
    }

    if let Err(e) = state.decoder.send_packet(&packet.data, pts_us, is_sync) {
        log::warn!("video decode error: {e}, resetting decoder");
        state.decoder.reset();
        state.need_keyframe = true;
        state.gcd_pts_ticks = 0;
        state.non_zero_pts_seen = 0;
        state.frame_duration_us = 0;
        return;
    }
    let fd = if state.non_zero_pts_seen >= 2 {
        state.frame_duration_us
    } else {
        0
    };
    let frames = state.decoder.drain_frames(fallback, load_serial, fd);
    for (i, frame) in frames.into_iter().enumerate() {
        if let Some(min) = min_pts {
            if frame.pts + Duration::from_millis(500) < min {
                log::trace!("[hvp] skip frame {} pts={:?} (min={:?})", i, frame.pts, min);
                continue;
            }
        }
        log::trace!(
            "[hvp] sending frame {} pts={:?} load_serial={}",
            i,
            frame.pts,
            load_serial
        );
        shared.video_frames_sent.fetch_add(1, Ordering::Release);
        if video_tx.try_send(frame).is_err() {
            log::trace!("[hvp] video_tx full, dropping frame (non-blocking)");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_file_to_queue(
    source: MediaSource,
    cmd_rx: &Receiver<Command>,
    sample_tx: &Sender<f32>,
    flush_rx: &Receiver<f32>,
    shared: Arc<Shared>,
    out_channels: usize,
    out_rate: u32,
    video_tx: &crossbeam_channel::Sender<video::DecodedVideoFrame>,
    initial_seek: Option<(Duration, u64)>,
) -> Result<DecodeOutcome> {
    reset_audio_queue_and_clock(&shared, flush_rx);
    shared.load_serial.fetch_add(1, Ordering::Release);
    shared.video_frames_sent.store(0, Ordering::Release);

    let display_name = match &source {
        MediaSource::Path(p) => p.file_name().map(|v| v.to_string_lossy().to_string()),
        MediaSource::Bytes { name, .. } => Some(name.clone()),
    };

    {
        let mut s = lock_status(&shared);
        s.state = PlaybackState::Loading;
        s.source = Some(source.clone());
        s.title = display_name;
        s.artist = None;
        s.album = None;
        s.art = None;
        s.position = Duration::ZERO;
        s.duration = None;
        s.error = None;
    }

    shared.resume_intent.store(true, Ordering::Release);

    let (mss, hint) = media_source_stream(source).context("failed to open media source")?;

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

        let mut s = lock_status(&shared);
        if let Some(t) = meta.title {
            s.title = Some(t);
        }
        s.artist = meta.artist;
        s.album = meta.album;
        s.art = meta.art;
    }

    // Audio track is optional. video-only files are valid.
    let audio_track = format
        .default_track(TrackType::Audio)
        .or_else(|| format.first_track_known_codec(TrackType::Audio));

    let mut video_state: Option<VideoDecodeState> = None;
    let mut video_duration: Option<Duration> = None;
    'video_init: {
        let vtrack = format
            .default_track(TrackType::Video)
            .or_else(|| format.first_track_known_codec(TrackType::Video));
        let Some(vtrack) = vtrack else {
            break 'video_init;
        };
        let Some(CodecParameters::Video(vp)) = &vtrack.codec_params else {
            break 'video_init;
        };
        use symphonia::core::codecs::video::well_known::extra_data as ed_ids;
        let w = vp.width.unwrap_or(0) as u32;
        let h = vp.height.unwrap_or(0) as u32;
        let wanted = if vp.codec == video_codec_ids::CODEC_ID_HEVC {
            ed_ids::VIDEO_EXTRA_DATA_ID_HEVC_DECODER_CONFIG
        } else if vp.codec == video_codec_ids::CODEC_ID_AV1 {
            ed_ids::VIDEO_EXTRA_DATA_ID_AV1_DECODER_CONFIG
        } else {
            ed_ids::VIDEO_EXTRA_DATA_ID_AVC_DECODER_CONFIG
        };
        let extradata = vp
            .extra_data
            .iter()
            .find(|ed| ed.id == wanted)
            .or_else(|| vp.extra_data.first())
            .map(|ed| &ed.data[..])
            .unwrap_or(&[]);
        let (name, dec) = if vp.codec == video_codec_ids::CODEC_ID_H264 {
            match video::VideoDecoder::new_h264(w, h, extradata) {
                Ok(d) => ("H.264", d),
                Err(e) => {
                    log::warn!("failed to init H.264 decoder: {e}");
                    break 'video_init;
                }
            }
        } else if vp.codec == video_codec_ids::CODEC_ID_HEVC {
            match video::VideoDecoder::new_hevc(w, h, extradata) {
                Ok(d) => ("H.265", d),
                Err(e) => {
                    log::warn!("failed to init H.265 decoder: {e}");
                    break 'video_init;
                }
            }
        } else if vp.codec == video_codec_ids::CODEC_ID_AV1 {
            match video::VideoDecoder::new_av1(w, h, extradata) {
                Ok(d) => ("AV1", d),
                Err(e) => {
                    log::warn!("failed to init AV1 decoder: {e}");
                    break 'video_init;
                }
            }
        } else {
            log::info!("unsupported video codec {:?}, skipping", vp.codec);
            break 'video_init;
        };
        video_duration = track_duration_secs(vtrack);
        log::info!("video: {name} {}x{}", w, h);
        let nal_len_size = if vp.codec == video_codec_ids::CODEC_ID_HEVC {
            video::parse_nal_length_size_hevc(extradata) as usize
        } else {
            video::parse_nal_length_size(extradata) as usize
        };
        video_state = Some(VideoDecodeState {
            track_id: vtrack.id,
            decoder: dec,
            time_base: vtrack.time_base.unwrap_or_default(),
            codec: if vp.codec == video_codec_ids::CODEC_ID_H264 {
                VideoCodecKind::H264 { nal_len_size }
            } else if vp.codec == video_codec_ids::CODEC_ID_HEVC {
                VideoCodecKind::Hevc { nal_len_size }
            } else {
                VideoCodecKind::Av1
            },
            need_keyframe: false,
            gcd_pts_ticks: 0,
            non_zero_pts_seen: 0,
            frame_duration_us: 0,
        });
    }

    shared
        .has_video
        .store(video_state.is_some(), Ordering::Release);
    if audio_track.is_none() && video_state.is_none() {
        anyhow::bail!("no supported audio or video track in this file");
    }

    // Duration: prefer audio, fall back to video
    let mut duration: Option<Duration> = None;
    let mut track_id = u32::MAX;
    let mut decoder: Option<Box<dyn AudioDecoder>> = None;
    let mut resampler: Option<RubatoResampler> = None;
    let mut audio_in_rate: Option<u32> = None;
    let mut audio_tb = TimeBase::default();

    if let Some(track) = audio_track {
        track_id = track.id;
        audio_tb = track.time_base.unwrap_or_default();
        let params = match &track.codec_params {
            Some(CodecParameters::Audio(p)) => p.clone(),
            _ => anyhow::bail!("audio track has no audio codec parameters"),
        };
        duration = match (track.num_frames, params.sample_rate) {
            (Some(frames), Some(rate)) if rate > 0 => {
                Some(Duration::from_secs_f64(frames as f64 / rate as f64))
            }
            _ => None,
        };
        decoder = Some(
            get_codecs()
                .make_audio_decoder(&params, &AudioDecoderOptions::default())
                .context("failed to create decoder")?,
        );
        audio_in_rate = Some(params.sample_rate.unwrap_or(out_rate));
        let speed = f32::from_bits(shared.playback_rate_bits.load(Ordering::Acquire));
        resampler = Some(rebuild_resampler(
            params.sample_rate.unwrap_or(out_rate),
            speed,
            out_rate,
            out_channels,
        )?);
    } else if let Some(vdur) = video_duration {
        duration = Some(vdur);
    }

    {
        let mut s = lock_status(&shared);
        s.state = PlaybackState::Buffering;
        s.duration = duration;
    }

    let mut seek_phase: Option<SeekPhase> = if let Some((target, serial)) = initial_seek {
        if let Some(ref mut vs) = video_state {
            vs.decoder.reset();
            vs.need_keyframe = true;
            vs.gcd_pts_ticks = 0;
            vs.non_zero_pts_seen = 0;
            vs.frame_duration_us = 0;
        }
        let video_track_id = video_state.as_ref().map(|vs| vs.track_id);
        perform_seek(
            &mut *format,
            &mut decoder,
            video_track_id,
            target,
            serial,
            duration,
            &shared,
            flush_rx,
            out_rate,
        );
        if let Some(r) = &mut resampler {
            r.reset();
        }
        Some(SeekPhase::new(target, out_rate, out_channels))
    } else {
        Some(SeekPhase::new(Duration::ZERO, out_rate, out_channels))
    };

    // Push silence samples up to a target position (for video-only clock).
    // Returns None when done, or Some(action) if a command interrupted.
    let push_silence_to = |target: Duration,
                           seek_base: Duration,
                           silence_pushed: &mut u64|
     -> Option<CommandAction> {
        let relative = target.saturating_sub(seek_base);
        let target_samples =
            (relative.as_secs_f64() * out_rate as f64) as u64 * out_channels as u64;
        while *silence_pushed < target_samples {
            match sample_tx.try_send(0.0f32) {
                Ok(()) => {
                    *silence_pushed += 1;
                }
                Err(TrySendError::Full(_)) => {
                    match drain_commands(cmd_rx, &shared) {
                        CommandAction::Continue => {}
                        other => return Some(other),
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                Err(TrySendError::Disconnected(_)) => return Some(CommandAction::Shutdown),
            }
        }
        None
    };

    let video_only = decoder.is_none();
    let mut silence_pushed: u64 = 0;
    let has_audio = decoder.is_some();

    let mut seek_base = initial_seek.map(|(t, _)| t).unwrap_or(Duration::ZERO);

    // Speed-change tracking: rebuild the resampler when the target rate changes
    // so wall clock advances at `speed` relative to media time.
    let mut last_speed_serial = shared.speed_serial.load(Ordering::Acquire);

    loop {
        let ss = shared.speed_serial.load(Ordering::Acquire);
        if ss != last_speed_serial {
            last_speed_serial = ss;
            if let (Some(in_rate), Some(r)) = (audio_in_rate, resampler.as_mut()) {
                let speed = f32::from_bits(shared.playback_rate_bits.load(Ordering::Acquire));
                *r = rebuild_resampler(in_rate, speed, out_rate, out_channels)?;
            }
        }

        match drain_commands(cmd_rx, &shared) {
            CommandAction::Continue => {}
            CommandAction::Load(path) => return Ok(DecodeOutcome::Load(path)),
            CommandAction::Shutdown => return Ok(DecodeOutcome::Shutdown),
            CommandAction::Seek(target, serial) => {
                if let Some(vs) = &mut video_state {
                    vs.decoder.reset();
                    vs.need_keyframe = true;
                    vs.gcd_pts_ticks = 0;
                    vs.non_zero_pts_seen = 0;
                    vs.frame_duration_us = 0;
                }
                shared.video_frames_sent.store(0, Ordering::Release);
                let video_track_id = video_state.as_ref().map(|vs| vs.track_id);
                perform_seek(
                    &mut *format,
                    &mut decoder,
                    video_track_id,
                    target,
                    serial,
                    duration,
                    &shared,
                    flush_rx,
                    out_rate,
                );
                if let Some(r) = &mut resampler {
                    r.reset();
                }
                seek_phase = Some(SeekPhase::new(target, out_rate, out_channels));
                silence_pushed = 0;
                seek_base = target;
            }
        }

        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => {
                // End of stream: flush remaining audio or push silence (video-only).
                if has_audio {
                    if let Some(ref mut res) = resampler {
                        let flushed = res.flush()?;
                        for mut sample in flushed {
                            loop {
                                match sample_tx.try_send(sample) {
                                    Ok(()) => break,
                                    Err(TrySendError::Full(s)) => {
                                        sample = s;
                                    }
                                    Err(TrySendError::Disconnected(_)) => {
                                        return Ok(DecodeOutcome::Shutdown);
                                    }
                                }
                            }
                        }
                    }
                } else if let Some(dur) = duration {
                    if let Some(action) = (push_silence_to)(dur, seek_base, &mut silence_pushed) {
                        match action {
                            CommandAction::Load(p) => return Ok(DecodeOutcome::Load(p)),
                            CommandAction::Seek(target, serial) => {
                                return Ok(DecodeOutcome::Seek(target, serial));
                            }
                            CommandAction::Shutdown => return Ok(DecodeOutcome::Shutdown),
                            CommandAction::Continue => {}
                        }
                    }
                }
                if seek_phase.is_some() && shared.resume_intent.load(Ordering::Acquire) {
                    seek_phase.take();
                    shared.is_playing.store(true, Ordering::Release);
                    let mut s = lock_status(&shared);
                    s.state = PlaybackState::Playing;
                }
                match wait_until_queue_drained(cmd_rx, &shared, flush_rx) {
                    DecodeOutcome::Seek(target, serial) => {
                        return Ok(DecodeOutcome::Seek(target, serial));
                    }
                    other => return Ok(other),
                }
            }
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                if has_audio {
                    if let Some(ref mut res) = resampler {
                        let flushed = res.flush()?;
                        for mut sample in flushed {
                            loop {
                                match sample_tx.try_send(sample) {
                                    Ok(()) => break,
                                    Err(TrySendError::Full(s)) => {
                                        sample = s;
                                    }
                                    Err(TrySendError::Disconnected(_)) => {
                                        return Ok(DecodeOutcome::Shutdown);
                                    }
                                }
                            }
                        }
                    }
                } else if let Some(dur) = duration {
                    if let Some(action) = (push_silence_to)(dur, seek_base, &mut silence_pushed) {
                        match action {
                            CommandAction::Load(p) => return Ok(DecodeOutcome::Load(p)),
                            CommandAction::Seek(target, serial) => {
                                return Ok(DecodeOutcome::Seek(target, serial));
                            }
                            CommandAction::Shutdown => return Ok(DecodeOutcome::Shutdown),
                            CommandAction::Continue => {}
                        }
                    }
                }
                if seek_phase.is_some() && shared.resume_intent.load(Ordering::Acquire) {
                    seek_phase.take();
                    shared.is_playing.store(true, Ordering::Release);
                    let mut s = lock_status(&shared);
                    s.state = PlaybackState::Playing;
                }
                match wait_until_queue_drained(cmd_rx, &shared, flush_rx) {
                    DecodeOutcome::Seek(target, serial) => {
                        return Ok(DecodeOutcome::Seek(target, serial));
                    }
                    other => return Ok(other),
                }
            }
            Err(SymphoniaError::IoError(_)) => {
                // Real I/O error, not end-of-stream.
                let mut s = lock_status(&shared);
                s.state = PlaybackState::Error;
                s.error = Some("I/O error reading media".into());
                shared.is_playing.store(false, Ordering::Release);
                return Ok(DecodeOutcome::Idle);
            }
            Err(SymphoniaError::ResetRequired) => {
                if has_audio {
                    // Chained container (e.g. concatenated OGG streams).
                    // Re-select the audio track and recreate the decoder.
                    let track = format
                        .default_track(TrackType::Audio)
                        .or_else(|| format.first_track_known_codec(TrackType::Audio))
                        .context("no supported audio track after reset")?;
                    track_id = track.id;
                    audio_tb = track.time_base.unwrap_or_default();
                    let params = match &track.codec_params {
                        Some(CodecParameters::Audio(p)) => p.clone(),
                        _ => anyhow::bail!("track has no audio codec parameters after reset"),
                    };
                    duration = match (track.num_frames, params.sample_rate) {
                        (Some(frames), Some(rate)) if rate > 0 => {
                            Some(Duration::from_secs_f64(frames as f64 / rate as f64))
                        }
                        _ => None,
                    };
                    decoder = Some(
                        get_codecs()
                            .make_audio_decoder(&params, &AudioDecoderOptions::default())
                            .context("failed to create decoder after reset")?,
                    );
                    audio_in_rate = Some(params.sample_rate.unwrap_or(out_rate));
                    let speed = f32::from_bits(shared.playback_rate_bits.load(Ordering::Acquire));
                    resampler = Some(rebuild_resampler(
                        params.sample_rate.unwrap_or(out_rate),
                        speed,
                        out_rate,
                        out_channels,
                    )?);
                }
                continue;
            }
            Err(err) => {
                let mut s = lock_status(&shared);
                s.state = PlaybackState::Error;
                s.error = Some(err.to_string());
                shared.is_playing.store(false, Ordering::Release);
                return Ok(DecodeOutcome::Idle);
            }
        };

        // Handle video packets
        if let Some(ref mut vs) = video_state {
            if packet.track_id == vs.track_id {
                let serial = shared.load_serial.load(Ordering::Acquire);
                let min_pts = seek_phase.as_ref().map(|p| p.target);
                handle_video_packet(vs, &packet, video_tx, serial, min_pts, &shared);

                // Video-only: push silence up to this packet's PTS
                // so the audio clock reflects the video position.
                if video_only {
                    let pts_us = packet_pts_us(&packet, &vs.time_base);
                    let pkt_pts = Duration::from_micros(pts_us.max(0) as u64);
                    if let Some(action) = (push_silence_to)(pkt_pts, seek_base, &mut silence_pushed)
                    {
                        match action {
                            CommandAction::Load(p) => return Ok(DecodeOutcome::Load(p)),
                            CommandAction::Seek(target, serial) => {
                                if let Some(ref mut vs2) = video_state {
                                    vs2.decoder.reset();
                                    vs2.need_keyframe = true;
                                    vs2.gcd_pts_ticks = 0;
                                    vs2.non_zero_pts_seen = 0;
                                    vs2.frame_duration_us = 0;
                                }
                                shared.video_frames_sent.store(0, Ordering::Release);
                                let video_track_id = video_state.as_ref().map(|vs| vs.track_id);
                                perform_seek(
                                    &mut *format,
                                    &mut decoder,
                                    video_track_id,
                                    target,
                                    serial,
                                    duration,
                                    &shared,
                                    flush_rx,
                                    out_rate,
                                );
                                if let Some(r) = &mut resampler {
                                    r.reset();
                                }
                                seek_phase = Some(SeekPhase::new(target, out_rate, out_channels));
                                silence_pushed = 0;
                                seek_base = target;
                            }
                            CommandAction::Shutdown => return Ok(DecodeOutcome::Shutdown),
                            CommandAction::Continue => {}
                        }
                    }
                }

            // Preroll gate (checked after every video packet)
            if video_only {
                if let Some(_phase) = &seek_phase {
                    let min_samples =
                        (out_rate as usize * out_channels * PREROLL_MS as usize) / 1000;
                    let video_ok = video_preroll_ok(&shared);
                    let audio_ok = sample_tx.len() >= min_samples;
                    let queue_capacity = out_rate as usize * out_channels * 4;
                    if audio_ok && (video_ok || sample_tx.len() >= queue_capacity * 3 / 4) {
                        let resume = shared.resume_intent.load(Ordering::Acquire);
                        shared.is_playing.store(resume, Ordering::Release);
                        {
                            let mut s = lock_status(&shared);
                            s.state = if resume {
                                PlaybackState::Playing
                            } else {
                                PlaybackState::Paused
                            };
                        }
                        seek_phase = None;
                    }
                }
            }

            continue;
        }
        }

        // Non-video packet: skip if it's not our audio track.
        if packet.track_id != track_id {
            continue;
        }
        // Reaching here means we have an audio packet.
        let Some(ref mut dec) = decoder else { continue };
        let Some(ref mut res) = resampler else {
            continue;
        };

        let decoded = match dec.decode(&packet) {
            Ok(decoded) => decoded,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(SymphoniaError::IoError(_)) => continue,
            Err(SymphoniaError::ResetRequired) => {
                dec.reset();
                continue;
            }
            Err(err) => {
                let mut s = lock_status(&shared);
                s.state = PlaybackState::Error;
                s.error = Some(err.to_string());
                shared.is_playing.store(false, Ordering::Release);
                return Ok(DecodeOutcome::Idle);
            }
        };

        let spec = decoded.spec();
        let in_channels = spec.channels().count().max(1);
        let mut f32_samples = vec![0.0; decoded.samples_interleaved()];
        decoded.copy_to_slice_interleaved(&mut f32_samples);

        let converted = res.process(&f32_samples, in_channels)?;

        let mut push: &[f32] = &converted;

        if let Some(phase) = &mut seek_phase {
            if !phase.audio_reached {
                let pkt_start = {
                    let t = audio_tb.calc_time_saturating(packet.pts);
                    Duration::from_secs_f64(t.as_secs_f64())
                };
                let out_frames = converted.len() / out_channels;
                let pkt_dur = Duration::from_secs_f64(out_frames as f64 / out_rate as f64);

                if pkt_start + pkt_dur <= phase.target {
                    push = &[];
                } else {
                    let lead = phase.target.saturating_sub(pkt_start);
                    let skip_frames = (lead.as_secs_f64() * out_rate as f64) as usize;
                    let skip = (skip_frames * out_channels).min(converted.len());
                    push = &converted[skip..];
                    phase.audio_reached = true;
                }
            }
        }

        for mut sample in push.iter().copied() {
            loop {
                match sample_tx.try_send(sample) {
                    Ok(()) => break,
                    Err(TrySendError::Full(s)) => {
                        sample = s;

                        match drain_commands(cmd_rx, &shared) {
                            CommandAction::Continue => {}
                            CommandAction::Load(path) => return Ok(DecodeOutcome::Load(path)),
                            CommandAction::Shutdown => return Ok(DecodeOutcome::Shutdown),
                            CommandAction::Seek(target, serial) => {
                                if let Some(vs) = &mut video_state {
                                    vs.decoder.reset();
                                    vs.need_keyframe = true;
                                    vs.gcd_pts_ticks = 0;
                                    vs.non_zero_pts_seen = 0;
                                    vs.frame_duration_us = 0;
                                }
                                shared.video_frames_sent.store(0, Ordering::Release);
                                let video_track_id = video_state.as_ref().map(|vs| vs.track_id);
                                perform_seek(
                                    &mut *format,
                                    &mut decoder,
                                    video_track_id,
                                    target,
                                    serial,
                                    duration,
                                    &shared,
                                    flush_rx,
                                    out_rate,
                                );
                                if let Some(r) = &mut resampler {
                                    r.reset();
                                }
                                seek_phase = Some(SeekPhase::new(target, out_rate, out_channels));
                                silence_pushed = 0;
                                seek_base = target;
                                break;
                            }
                        }

                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(TrySendError::Disconnected(_)) => return Ok(DecodeOutcome::Shutdown),
                }
            }
        }

        // Preroll gate (checked after every audio packet)
        if let Some(phase) = &seek_phase {
            let audio_ok = phase.audio_reached && sample_tx.len() >= phase.min_samples;
            let video_ok = video_preroll_ok(&shared);
            let queue_capacity = out_rate as usize * out_channels * 4;
            if audio_ok && (video_ok || sample_tx.len() >= queue_capacity * 3 / 4) {
                let resume = shared.resume_intent.load(Ordering::Acquire);
                shared.is_playing.store(resume, Ordering::Release);
                {
                    let mut s = lock_status(&shared);
                    s.state = if resume {
                        PlaybackState::Playing
                    } else {
                        PlaybackState::Paused
                    };
                }
                seek_phase = None;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn perform_seek(
    format: &mut dyn symphonia::core::formats::FormatReader,
    decoder: &mut Option<Box<dyn AudioDecoder>>,
    video_track_id: Option<u32>,
    target: Duration,
    serial: u64,
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

    let was_playing = shared.is_playing.load(Ordering::Acquire);
    shared.resume_intent.store(was_playing, Ordering::Release);

    let seek_track = video_track_id.unwrap_or(0);

    match format.seek(
        SeekMode::Accurate,
        SeekTo::Time {
            time,
            track_id: Some(seek_track),
        },
    ) {
        Ok(_) => {
            if let Some(dec) = decoder {
                dec.reset();
            }
            reset_audio_queue_and_clock(shared, flush_rx);
            {
                let mut s = lock_status(shared);
                s.state = PlaybackState::Buffering;
                s.position = clamped;
            }
            if shared.seek_serial.load(Ordering::Acquire) == serial {
                let base = (clamped.as_secs_f64() * out_rate as f64) as u64;
                shared.base_media_frames.store(base, Ordering::Release);
            }
        }
        Err(err) => {
            let mut s = lock_status(shared);
            s.error = Some(format!("seek failed: {err}"));
        }
    }
}

enum CommandAction {
    Continue,
    Load(MediaSource),
    Seek(Duration, u64),
    Shutdown,
}

fn drain_commands(cmd_rx: &Receiver<Command>, shared: &Arc<Shared>) -> CommandAction {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match apply_command(cmd, shared) {
            CommandAction::Continue => {}
            other => return other,
        }
    }
    CommandAction::Continue
}

fn apply_command(cmd: Command, shared: &Arc<Shared>) -> CommandAction {
    match cmd {
        Command::Load(path) => CommandAction::Load(path),
        Command::Seek(pos, serial) => CommandAction::Seek(pos, serial),

        Command::Play => {
            let mut s = lock_status(shared);
            match s.state {
                PlaybackState::Paused => {
                    shared.is_playing.store(true, Ordering::Release);
                    s.state = PlaybackState::Playing;
                }
                PlaybackState::Buffering => {
                    shared.resume_intent.store(true, Ordering::Release);
                }
                PlaybackState::Ended => {
                    if let Some(source) = s.source.clone() {
                        return CommandAction::Load(source);
                    }
                }
                PlaybackState::Idle
                | PlaybackState::Loading
                | PlaybackState::Playing
                | PlaybackState::Error => {}
            }
            CommandAction::Continue
        }

        Command::Pause => {
            shared.is_playing.store(false, Ordering::Release);
            let mut s = lock_status(shared);
            match s.state {
                PlaybackState::Playing => s.state = PlaybackState::Paused,
                PlaybackState::Buffering => {
                    shared.resume_intent.store(false, Ordering::Release);
                }
                _ => {}
            }
            CommandAction::Continue
        }

        Command::Toggle => {
            let mut s = lock_status(shared);
            match s.state {
                PlaybackState::Idle | PlaybackState::Loading | PlaybackState::Error => {
                    return CommandAction::Continue;
                }
                PlaybackState::Ended => {
                    if let Some(source) = s.source.clone() {
                        return CommandAction::Load(source);
                    }
                    return CommandAction::Continue;
                }
                PlaybackState::Playing => {
                    shared.is_playing.store(false, Ordering::Release);
                    s.state = PlaybackState::Paused;
                }
                PlaybackState::Paused => {
                    shared.is_playing.store(true, Ordering::Release);
                    s.state = PlaybackState::Playing;
                }
                PlaybackState::Buffering => {
                    let new_intent = !shared.resume_intent.load(Ordering::Acquire);
                    shared.resume_intent.store(new_intent, Ordering::Release);
                }
            }
            CommandAction::Continue
        }

        Command::SetVolume(v) => {
            let v = v.clamp(0.0, 2.0);
            shared.unmute_volume_bits.store(v.to_bits(), Ordering::Release);
            if !shared.muted.load(Ordering::Acquire) {
                shared.volume_bits.store(v.to_bits(), Ordering::Release);
            }
            let mut s = lock_status(shared);
            s.volume = v;
            CommandAction::Continue
        }

        Command::SetMuted(m) => {
            shared.muted.store(m, Ordering::Release);
            if m {
                shared.volume_bits.store(0.0f32.to_bits(), Ordering::Release);
            } else {
                let v = shared.unmute_volume_bits.load(Ordering::Acquire);
                shared.volume_bits.store(v, Ordering::Release);
            }
            let mut s = lock_status(shared);
            s.muted = m;
            CommandAction::Continue
        }

        Command::ToggleMute => {
            let m = !shared.muted.load(Ordering::Acquire);
            shared.muted.store(m, Ordering::Release);
            if m {
                shared.volume_bits.store(0.0f32.to_bits(), Ordering::Release);
            } else {
                let v = shared.unmute_volume_bits.load(Ordering::Acquire);
                shared.volume_bits.store(v, Ordering::Release);
            }
            let mut s = lock_status(shared);
            s.muted = m;
            CommandAction::Continue
        }

        Command::SetSpeed(s) => {
            let mut st = lock_status(shared);
            st.playback_rate = s.clamp(0.25, 4.0);
            CommandAction::Continue
        }

        Command::Shutdown => CommandAction::Shutdown,
    }
}

fn flush_audio_queue(rx: &Receiver<f32>) {
    while rx.try_recv().is_ok() {}
}

fn reset_audio_queue_and_clock(shared: &Shared, flush_rx: &Receiver<f32>) {
    shared.is_playing.store(false, Ordering::Release);
    flush_audio_queue(flush_rx);
    shared.played_frames.store(0, Ordering::Release);
    shared.base_media_frames.store(0, Ordering::Release);
}

fn wait_until_queue_drained(
    cmd_rx: &Receiver<Command>,
    shared: &Arc<Shared>,
    flush_rx: &Receiver<f32>,
) -> DecodeOutcome {
    loop {
        match drain_commands(cmd_rx, shared) {
            CommandAction::Continue => {}
            CommandAction::Load(path) => return DecodeOutcome::Load(path),
            CommandAction::Seek(target, serial) => return DecodeOutcome::Seek(target, serial),
            CommandAction::Shutdown => return DecodeOutcome::Shutdown,
        }

        if flush_rx.is_empty() {
            shared.is_playing.store(false, Ordering::Release);

            let mut s = lock_status(shared);
            s.state = PlaybackState::Ended;

            return DecodeOutcome::Idle;
        }

        thread::sleep(Duration::from_millis(10));
    }
}

/// Build a resampler whose output target is `out_rate / speed`, so the wall
/// clock consumes the media's audio in `1/speed` real seconds.
fn rebuild_resampler(
    in_rate: u32,
    speed: f32,
    out_rate: u32,
    out_channels: usize,
) -> anyhow::Result<RubatoResampler> {
    let speed = speed.clamp(0.25, 4.0);
    let target = ((out_rate as f64) / speed as f64).round().max(8000.0) as u32;
    RubatoResampler::new(in_rate, target, out_channels)
}

struct RubatoResampler {
    inner: Fft<f32>,
    channels: usize,
    input_frames_needed: usize,
    input_buf: Vec<f32>,
    input_pos: usize,
    output_buf: Vec<f32>,
    output_capacity: usize,
}

impl RubatoResampler {
    fn new(in_rate: u32, out_rate: u32, out_channels: usize) -> anyhow::Result<Self> {
        let channels = out_channels.max(1);
        let chunk_size = 512.min(in_rate.max(out_rate) as usize / 10).max(64);

        let inner = Fft::<f32>::new(
            in_rate as usize,
            out_rate as usize,
            chunk_size,
            channels,
            FixedSync::Input,
        )
        .context("failed to create FFT resampler")?;

        let input_frames_needed = inner.input_frames_next();
        let output_capacity = inner.output_frames_max();

        Ok(Self {
            inner,
            channels,
            input_frames_needed,
            input_buf: vec![0.0; input_frames_needed * channels],
            input_pos: 0,
            output_buf: vec![0.0; output_capacity * channels],
            output_capacity,
        })
    }

    fn process(&mut self, input: &[f32], in_channels: usize) -> anyhow::Result<Vec<f32>> {
        let in_channels = in_channels.max(1);
        let out_channels = self.channels;
        let in_frames = input.len() / in_channels;
        if in_frames == 0 {
            return Ok(Vec::new());
        }

        let new_pos = self.input_pos + in_frames;
        let needed = new_pos * out_channels;
        if needed > self.input_buf.len() {
            self.input_buf.resize(needed, 0.0);
        }
        for frame in 0..in_frames {
            let pos = (self.input_pos + frame) * out_channels;
            for ch in 0..out_channels {
                self.input_buf[pos + ch] =
                    read_mapped_sample(input, frame, ch, in_channels, out_channels);
            }
        }
        self.input_pos = new_pos;

        let mut output = Vec::new();
        while self.input_pos >= self.input_frames_needed {
            let total = self.input_frames_needed * out_channels;

            let input_adapter = InterleavedSlice::new(
                &self.input_buf[..total],
                out_channels,
                self.input_frames_needed,
            )
            .context("resampler input adapter")?;

            let cap = self.output_capacity * out_channels;
            let mut output_adapter = InterleavedSlice::new_mut(
                &mut self.output_buf[..cap],
                out_channels,
                self.output_capacity,
            )
            .context("resampler output adapter")?;

            let (_, written) = self
                .inner
                .process_into_buffer(&input_adapter, &mut output_adapter, None)
                .context("resampling failed")?;

            output.extend_from_slice(&self.output_buf[..written * out_channels]);

            // Shift remaining buffered data to front.
            let consumed = self.input_frames_needed;
            let remaining = self.input_pos - consumed;
            if remaining > 0 {
                self.input_buf.copy_within(consumed * out_channels.., 0);
            }
            self.input_pos = remaining;
        }

        Ok(output)
    }

    fn flush(&mut self) -> anyhow::Result<Vec<f32>> {
        if self.input_pos == 0 {
            return Ok(Vec::new());
        }

        let out_channels = self.channels;
        let total = self.input_frames_needed * out_channels;

        // Zero unused portion of input buffer.
        let used = self.input_pos * out_channels;
        self.input_buf[used..total].fill(0.0);

        let input_adapter = InterleavedSlice::new(
            &self.input_buf[..total],
            out_channels,
            self.input_frames_needed,
        )
        .context("flush input adapter")?;

        let cap = self.output_capacity * out_channels;
        let mut output_adapter = InterleavedSlice::new_mut(
            &mut self.output_buf[..cap],
            out_channels,
            self.output_capacity,
        )
        .context("flush output adapter")?;

        let indexing = rubato::Indexing::new().partial_len(self.input_pos);
        let (_, written) = self
            .inner
            .process_into_buffer(&input_adapter, &mut output_adapter, Some(&indexing))
            .context("resampling failed on flush")?;

        self.input_pos = 0;

        Ok(self.output_buf[..written * out_channels].to_vec())
    }

    fn reset(&mut self) {
        self.input_pos = 0;
        self.inner.reset();
    }
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
