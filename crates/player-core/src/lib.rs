use std::{
    fs::File,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

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
    Shutdown,
}

#[derive(Clone)]
pub struct AudioPlayer {
    inner: Arc<AudioPlayerInner>,
}

struct AudioPlayerInner {
    tx: Sender<Command>,
    shared: Arc<Shared>,
}

impl Drop for AudioPlayerInner {
    fn drop(&mut self) {
        self.tx.send(Command::Shutdown).ok();
    }
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
    /// Incremented on every load/seek/stop. Callbacks discard samples from
    /// an old generation to prevent stale audio leaking after a discontinuity.
    generation: AtomicU64,
    /// Monotonically increasing serial for seek ordering.  Bumped before
    /// every `Command::Seek` is sent.  `perform_seek` checks this to avoid
    /// overwriting a newer seek's optimistic clock rebase.
    seek_serial: AtomicU64,
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
            base_frames: AtomicU64::new(0),
            output_sample_rate: AtomicU32::new(48_000),
            volume_bits: AtomicU32::new(1.0f32.to_bits()),
            generation: AtomicU64::new(0),
            seek_serial: AtomicU64::new(0),
        });

        let (tx, rx) = unbounded();
        let thread_shared = shared.clone();

        #[cfg(not(target_arch = "wasm32"))]
        {
            let (stream, sample_tx, flush_rx, out_channels, out_sample_rate) =
                create_cpal_stream(&shared)?;

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
        audio_thread_wasm(rx, thread_shared).context("failed to start WASM audio")?;

        Ok(Self {
            inner: Arc::new(AudioPlayerInner { tx, shared }),
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
        shared.base_frames.store(base, Ordering::Release);
        shared.played_frames.store(0, Ordering::Release);
        let serial = shared.seek_serial.fetch_add(1, Ordering::AcqRel) + 1;
        self.send(Command::Seek(position, serial))
    }
    pub fn set_volume(&self, volume: f32) -> Result<()> {
        let v = volume.clamp(0.0, 2.0);
        let shared = &self.inner.shared;
        shared.volume_bits.store(v.to_bits(), Ordering::Release);
        {
            let mut s = lock_status(shared);
            s.volume = v;
        }
        self.send(Command::SetVolume(v))
    }

    /// Current playback position derived from output frames.
    /// This is the master clock -> video sync will read this.
    pub fn position(&self) -> Duration {
        let shared = &self.inner.shared;

        let rate = shared.output_sample_rate.load(Ordering::Acquire).max(1);

        let frames = shared.base_frames.load(Ordering::Acquire)
            + shared.played_frames.load(Ordering::Acquire);

        Duration::from_secs_f64(frames as f64 / rate as f64)
    }

    pub fn is_playing(&self) -> bool {
        self.inner.shared.is_playing.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        let shared = &self.inner.shared;

        let mut snap = lock_status(shared).clone();
        snap.position = self.position();
        snap.volume = f32::from_bits(shared.volume_bits.load(Ordering::Acquire));
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
            let cursor = Cursor::new(bytes.to_vec());
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

/// WASM entry point: sets up CPAL synchronously, stores the stream in a
/// static `OnceLock`, and spawns the decode/command loop onto a real
/// Web Worker via `web_thread` so that blocking I/O and channel waits
/// actually sleep instead of spinning the main thread.
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

    // Real thread (Web Worker) via web_thread — blocking I/O, channel
    // sends, and thread::sleep all work correctly here.
    web_thread::spawn(move || {
        let result = run_command_loop(
            &cmd_rx,
            &sample_tx,
            &flush_rx,
            &shared,
            out_channels,
            out_sample_rate,
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
) -> Result<()> {
    loop {
        match cmd_rx.recv() {
            Ok(Command::Load(path)) => {
                log::info!("audio thread processing load: {}", path.display_name());
                let mut next = Some(path);
                'decode: while let Some(path) = next.take() {
                    match decode_file_to_queue(
                        path,
                        cmd_rx,
                        sample_tx,
                        flush_rx,
                        shared.clone(),
                        out_channels,
                        out_sample_rate,
                    ) {
                        Ok(DecodeOutcome::Idle) => next = None,
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
            Ok(cmd) => match apply_command(cmd, shared, flush_rx) {
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
    Load(MediaSource),
    Shutdown,
}

fn decode_file_to_queue(
    source: MediaSource,
    cmd_rx: &Receiver<Command>,
    sample_tx: &Sender<f32>,
    flush_rx: &Receiver<f32>,
    shared: Arc<Shared>,
    out_channels: usize,
    out_rate: u32,
) -> Result<DecodeOutcome> {
    reset_audio_queue_and_clock(&shared, flush_rx);

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

    shared.is_playing.store(true, Ordering::Release);

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

    let track = format
        .default_track(TrackType::Audio)
        .or_else(|| format.first_track_known_codec(TrackType::Audio))
        .context("no supported audio track")?;

    let mut track_id = track.id;
    let mut codec_params = match &track.codec_params {
        Some(CodecParameters::Audio(p)) => p.clone(),
        _ => anyhow::bail!("track has no audio codec parameters"),
    };

    let mut duration = match (track.num_frames, codec_params.sample_rate) {
        (Some(frames), Some(rate)) if rate > 0 => {
            Some(Duration::from_secs_f64(frames as f64 / rate as f64))
        }
        _ => None,
    };

    {
        let mut s = lock_status(&shared);
        s.state = PlaybackState::Playing;
        s.duration = duration;
    }

    let mut decoder = get_codecs()
        .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
        .context("failed to create decoder")?;

    let mut resampler = RubatoResampler::new(
        codec_params.sample_rate.unwrap_or(out_rate),
        out_rate,
        out_channels,
    )?;

    loop {
        match drain_commands(cmd_rx, &shared, flush_rx) {
            CommandAction::Continue => {}
            CommandAction::Load(path) => return Ok(DecodeOutcome::Load(path)),
            CommandAction::Shutdown => return Ok(DecodeOutcome::Shutdown),
            CommandAction::Seek(target, serial) => {
                perform_seek(
                    &mut *format,
                    &mut *decoder,
                    track_id,
                    target,
                    serial,
                    duration,
                    &shared,
                    flush_rx,
                    out_rate,
                );
                resampler.reset();
            }
        }

        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => {
                let flushed = resampler.flush()?;
                for mut sample in flushed {
                    loop {
                        match sample_tx.try_send(sample) {
                            Ok(()) => break,
                            Err(TrySendError::Full(s)) => {
                                sample = s;
                                match drain_commands(cmd_rx, &shared, flush_rx) {
                                    CommandAction::Continue => {}
                                    CommandAction::Load(p) => return Ok(DecodeOutcome::Load(p)),
                                    CommandAction::Shutdown => return Ok(DecodeOutcome::Shutdown),
                                    _ => return Ok(DecodeOutcome::Idle),
                                }
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(TrySendError::Disconnected(_)) => {
                                return Ok(DecodeOutcome::Shutdown);
                            }
                        }
                    }
                }
                return Ok(wait_until_queue_drained(cmd_rx, &shared, flush_rx));
            }
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                let flushed = resampler.flush()?;
                for mut sample in flushed {
                    loop {
                        match sample_tx.try_send(sample) {
                            Ok(()) => break,
                            Err(TrySendError::Full(s)) => {
                                sample = s;
                                match drain_commands(cmd_rx, &shared, flush_rx) {
                                    CommandAction::Continue => {}
                                    CommandAction::Load(p) => return Ok(DecodeOutcome::Load(p)),
                                    CommandAction::Shutdown => return Ok(DecodeOutcome::Shutdown),
                                    _ => return Ok(DecodeOutcome::Idle),
                                }
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(TrySendError::Disconnected(_)) => {
                                return Ok(DecodeOutcome::Shutdown);
                            }
                        }
                    }
                }
                return Ok(wait_until_queue_drained(cmd_rx, &shared, flush_rx));
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
                // Chained container (e.g. concatenated OGG streams).
                // Re-select the audio track and recreate the decoder.
                let track = format
                    .default_track(TrackType::Audio)
                    .or_else(|| format.first_track_known_codec(TrackType::Audio))
                    .context("no supported audio track after reset")?;
                track_id = track.id;
                codec_params = match &track.codec_params {
                    Some(CodecParameters::Audio(p)) => p.clone(),
                    _ => anyhow::bail!("track has no audio codec parameters after reset"),
                };
                duration = match (track.num_frames, codec_params.sample_rate) {
                    (Some(frames), Some(rate)) if rate > 0 => {
                        Some(Duration::from_secs_f64(frames as f64 / rate as f64))
                    }
                    _ => None,
                };
                decoder = get_codecs()
                    .make_audio_decoder(&codec_params, &AudioDecoderOptions::default())
                    .context("failed to create decoder after reset")?;
                resampler = RubatoResampler::new(
                    codec_params.sample_rate.unwrap_or(out_rate),
                    out_rate,
                    out_channels,
                )?;
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

        let converted = resampler.process(&f32_samples, in_channels)?;

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
                            CommandAction::Seek(target, serial) => {
                                perform_seek(
                                    &mut *format,
                                    &mut *decoder,
                                    track_id,
                                    target,
                                    serial,
                                    duration,
                                    &shared,
                                    flush_rx,
                                    out_rate,
                                );
                                resampler.reset();
                                // Drop the in-flight sample; it's pre-seek audio.
                                break;
                            }
                        }

                        // Yield so the audio callback can drain the queue.
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(TrySendError::Disconnected(_)) => return Ok(DecodeOutcome::Shutdown),
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn perform_seek(
    format: &mut dyn symphonia::core::formats::FormatReader,
    decoder: &mut dyn AudioDecoder,
    track_id: u32,
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

    match format.seek(
        SeekMode::Accurate,
        SeekTo::Time {
            time,
            track_id: Some(track_id),
        },
    ) {
        Ok(_seeked_to) => {
            decoder.reset();
            reset_audio_queue_and_clock(shared, flush_rx);
            shared.is_playing.store(was_playing, Ordering::Release);
            // Don't overwrite a newer seek's optimistic clock rebase.
            if shared.seek_serial.load(Ordering::Acquire) == serial {
                let base = (clamped.as_secs_f64() * out_rate as f64) as u64;
                shared.base_frames.store(base, Ordering::Release);
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
        Command::Seek(pos, serial) => CommandAction::Seek(pos, serial),

        Command::Play => {
            let mut s = lock_status(shared);
            match s.state {
                PlaybackState::Paused => {
                    shared.is_playing.store(true, Ordering::Release);
                    s.state = PlaybackState::Playing;
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
            if s.state == PlaybackState::Playing {
                s.state = PlaybackState::Paused;
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
            }
            CommandAction::Continue
        }

        Command::SetVolume(v) => {
            let v = v.clamp(0.0, 2.0);
            shared.volume_bits.store(v.to_bits(), Ordering::Release);
            let mut s = lock_status(shared);
            s.volume = v;
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
    shared.base_frames.store(0, Ordering::Release);
    shared.generation.fetch_add(1, Ordering::Release);
}

fn wait_until_queue_drained(
    cmd_rx: &Receiver<Command>,
    shared: &Arc<Shared>,
    flush_rx: &Receiver<f32>,
) -> DecodeOutcome {
    loop {
        match drain_commands(cmd_rx, shared, flush_rx) {
            CommandAction::Continue => {}
            CommandAction::Load(path) => return DecodeOutcome::Load(path),
            CommandAction::Seek(..) => return DecodeOutcome::Idle,
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
