#![allow(non_snake_case, non_upper_case_globals)]

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use web_time::Instant;

use player_core::video::DecodedVideoFrame;
use player_core::{AudioPlayer, MediaSource, PlaybackState, TrackMeta, probe_media_source};
use repose_core::modifier::PaddingValues;
use repose_core::prelude::*;
use repose_core::text::FontWeight;
use repose_material::material3 as m3;
use repose_material::{Icon, material_symbols};
use repose_platform::render::RenderContext;
use repose_ui::TextStyle;

#[cfg(target_arch = "wasm32")]
use web_workers as thread;

#[cfg(not(target_arch = "wasm32"))]
use std::thread;

material_symbols! {
    add            : '\u{E145}',
    image          : '\u{E3F4}',
    music_note     : '\u{E405}',
    pause          : '\u{E034}',
    play_arrow     : '\u{E037}',
    skip_next      : '\u{E044}',
    skip_previous  : '\u{E045}',
    volume_up      : '\u{E050}',
    volume_down    : '\u{E04D}',
    volume_off     : '\u{E04F}',
    library_music  : '\u{E030}',
    error_icon     : '\u{E000}',
    close          : '\u{E5CD}',
    graphic_eq     : '\u{E1B8}',
    fullscreen     : '\u{E5D0}',
    fullscreen_exit: '\u{E5D1}',
    replay_10      : '\u{E059}',
    forward_10     : '\u{E056}',
    settings       : '\u{E8B8}',
    speed          : '\u{E9E4}',
    movie          : '\u{E02C}',
}
use repose_ui::lazy_states::LazyColumnState;
use repose_ui::{
    Box, Column, Image, LazyColumn, LazyColumnConfig, Row, Spacer, Text, ViewExt, ZStack,
};

#[derive(Clone)]
struct Entry {
    id: u64,
    source: MediaSource,
    meta: TrackMeta,
}

impl Entry {
    fn display_title(&self) -> String {
        self.meta
            .title
            .clone()
            .unwrap_or_else(|| match &self.source {
                MediaSource::Path(p) => p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Unknown".into()),
                MediaSource::Bytes { name, .. } => std::path::Path::new(name)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| name.clone()),
            })
    }
}

struct PendingState {
    files: Mutex<Vec<MediaSource>>,
    probed_meta: Mutex<Vec<(MediaSource, TrackMeta)>>,
    needs_wake: AtomicBool,
    next_id: AtomicU64,
    /// When true, the app enters fullscreen as soon as the current item is
    /// video (by extension OR `player.has_video()`). Cleared on explicit exit.
    auto_fullscreen: AtomicBool,
}

type PendingFiles = Arc<PendingState>;

fn is_video_name(name: &str) -> bool {
    const VID: &[&str] = &["mp4", "m4v", "mkv", "webm", "mov", "avi", "ts", "m2ts"];
    std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| VID.iter().any(|v| e.eq_ignore_ascii_case(v)))
        .unwrap_or(false)
}

fn is_video_source(src: &MediaSource) -> bool {
    match src {
        MediaSource::Path(p) => is_video_name(&p.to_string_lossy()),
        MediaSource::Bytes { name, .. } => is_video_name(name),
    }
}

#[derive(Clone, Debug)]
struct PlayerSettings {
    auto_fullscreen_on_open: bool,
    hide_controls_ms: u64,
    seek_small_s: f64,
    seek_medium_s: f64,
    seek_large_s: f64,
    volume_step: f32,
    default_speed: f32,
    remember_speed: bool,
    show_playlist_thumbs: bool,
}

impl Default for PlayerSettings {
    fn default() -> Self {
        Self {
            auto_fullscreen_on_open: true,
            hide_controls_ms: 2500,
            seek_small_s: 1.0,
            seek_medium_s: 5.0,
            seek_large_s: 60.0,
            volume_step: 0.05,
            default_speed: 1.0,
            remember_speed: true,
            show_playlist_thumbs: true,
        }
    }
}

impl PlayerSettings {
    fn to_json(&self) -> String {
        format!(
            r#"{{"auto_fullscreen_on_open":{},"hide_controls_ms":{},"seek_small_s":{},"seek_medium_s":{},"seek_large_s":{},"volume_step":{},"default_speed":{},"remember_speed":{},"show_playlist_thumbs":{}}}"#,
            self.auto_fullscreen_on_open,
            self.hide_controls_ms,
            self.seek_small_s,
            self.seek_medium_s,
            self.seek_large_s,
            self.volume_step,
            self.default_speed,
            self.remember_speed,
            self.show_playlist_thumbs,
        )
    }

    fn from_json(s: &str) -> Self {
        let mut out = Self::default();
        let get_bool = |k: &str| s.contains(&format!("\"{k}\":true"));
        let get_f = |k: &str, default: f64| {
            s.split(&format!("\"{k}\":"))
                .nth(1)
                .and_then(|rest| {
                    rest.split([',', '}'])
                        .next()
                        .and_then(|n| n.trim().parse().ok())
                })
                .unwrap_or(default)
        };
        out.auto_fullscreen_on_open = get_bool("auto_fullscreen_on_open");
        out.hide_controls_ms = get_f("hide_controls_ms", 2500.0) as u64;
        out.seek_small_s = get_f("seek_small_s", 1.0);
        out.seek_medium_s = get_f("seek_medium_s", 5.0);
        out.seek_large_s = get_f("seek_large_s", 60.0);
        out.volume_step = get_f("volume_step", 0.05) as f32;
        out.default_speed = get_f("default_speed", 1.0) as f32;
        out.remember_speed = !s.contains("\"remember_speed\":false");
        out.show_playlist_thumbs = !s.contains("\"show_playlist_thumbs\":false");
        out
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn settings_path() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from) {
        return Some(dir.join("repadio").join("settings.json"));
    }
    std::env::var_os("HOME").map(|h| {
        std::path::PathBuf::from(h)
            .join(".config")
            .join("repadio")
            .join("settings.json")
    })
}

fn load_settings_sync() -> PlayerSettings {
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(p) = settings_path()
            && let Ok(s) = std::fs::read_to_string(p)
        {
            return PlayerSettings::from_json(&s);
        }
    }
    PlayerSettings::default()
}

fn save_settings_sync(s: &PlayerSettings) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(p) = settings_path() {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(p, s.to_json());
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let json = s.to_json();
        wasm_bindgen_futures::spawn_local(async move {
            let _ = player_platform::wasm_persist::write("config/settings.json", &json).await;
        });
    }
}

// when the encoded bytes change (fingerprint), to avoid handle leaks.
struct ArtCache {
    map: std::collections::HashMap<u64, ImageHandle>,
    fingerprints: std::collections::HashMap<u64, u64>,
}

impl ArtCache {
    fn new() -> Self {
        Self {
            map: Default::default(),
            fingerprints: Default::default(),
        }
    }

    fn fingerprint(bytes: &[u8]) -> u64 {
        let mut h = 0xcbf29ce484222325u64;
        for &b in bytes.iter().take(4096) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^ (bytes.len() as u64)
    }

    fn ensure(
        &mut self,
        ctx: &RenderContext,
        id: u64,
        art: &Option<Arc<Vec<u8>>>,
    ) -> Option<ImageHandle> {
        let bytes = art.as_ref()?;
        let fp = Self::fingerprint(bytes);
        if self.fingerprints.get(&id) == Some(&fp) {
            return self.map.get(&id).copied();
        }
        let handle = match self.map.get(&id) {
            Some(&h) => h,
            None => {
                let h = ctx.alloc_image_handle();
                self.map.insert(id, h);
                h
            }
        };
        ctx.set_image_encoded(handle, bytes.as_ref().clone(), true);
        self.fingerprints.insert(id, fp);
        Some(handle)
    }
}

struct VideoSink {
    rx: crossbeam_channel::Receiver<DecodedVideoFrame>,
    handles: [Option<ImageHandle>; 2],
    active_idx: usize,
    clock: AudioPlayer,
    frame_timer: Option<Instant>,
    frame_duration: f64,
    buffered: Vec<DecodedVideoFrame>,
    last_seek_serial: u64,

    visible: bool,
    last_load_serial: u64,
    aspect: f32,
}

impl VideoSink {
    fn new(rx: crossbeam_channel::Receiver<DecodedVideoFrame>, clock: AudioPlayer) -> Self {
        let serial = clock.seek_serial();
        let load_serial = clock.load_serial();
        Self {
            rx,
            handles: [None, None],
            active_idx: 0,
            clock,
            frame_timer: None,
            frame_duration: 1.0 / 30.0,
            buffered: Vec::new(),
            last_seek_serial: serial,

            visible: false,
            last_load_serial: load_serial,
            aspect: 16.0 / 9.0,
        }
    }

    fn poll(&mut self, ctx: &RenderContext) {
        let current_load_serial = self.clock.load_serial();
        let current_serial = self.clock.seek_serial();

        // New file: hard reset, blank the video
        if current_load_serial != self.last_load_serial {
            self.last_load_serial = current_load_serial;
            self.last_seek_serial = current_serial;
            self.buffered.clear();
            self.frame_timer = None;
            self.frame_duration = 1.0 / 30.0;
            self.visible = false;
            while self.rx.try_recv().is_ok() {}
        // Seek within same file: drop stale queued frames, keep the last frame visible
        } else if current_serial != self.last_seek_serial {
            self.last_seek_serial = current_serial;
            self.buffered.clear();
            self.frame_timer = None;
            self.frame_duration = 1.0 / 30.0;
            while self.rx.try_recv().is_ok() {}
        }

        // Drain a limited batch per poll so the channel provides natural
        // back-pressure against the decoder.
        for _ in 0..16 {
            match self.rx.try_recv() {
                Ok(frame) if frame.load_serial == current_load_serial => {
                    self.aspect = frame.width as f32 / frame.height.max(1) as f32;
                    self.buffered.push(frame);
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }

        // Sort by PTS for proper display order (H.264 B-frames etc.)
        self.buffered.sort_by_key(|f| f.pts);

        if self.buffered.is_empty() {
            // Still request a frame in case more are coming on the channel.
            request_frame();
            return;
        }

        // Allocate two ping-pong ImageHandles on first frame
        if self.handles[0].is_none() {
            self.handles[0] = Some(ctx.alloc_image_handle());
            self.handles[1] = Some(ctx.alloc_image_handle());
        }

        let now = Instant::now();

        // Initialize frame timer on first frame
        if self.frame_timer.is_none() {
            self.frame_timer = Some(now);
        }

        // Compare frame PTS against the audio clock for A/V sync.
        // The audio clock always advances (via real audio or injected silence
        // for video-only files).
        let now_clock = self.clock.position();

        // Process frames: find one to display, drop late ones, wait for early ones
        while !self.buffered.is_empty() {
            let pts = self.buffered[0].pts;

            // Update frame_duration from PTS gaps if we have >1 frame buffered
            if self.buffered.len() > 1 {
                let next_pts = self.buffered[1].pts;
                let gap = if next_pts > pts && next_pts < pts + Duration::from_secs(1) {
                    next_pts - pts
                } else {
                    Duration::from_secs_f64(self.frame_duration)
                };
                self.frame_duration = gap.as_secs_f64();
            }

            let gap = now_clock.saturating_sub(pts);
            let drop_threshold = Duration::from_secs_f64(self.frame_duration * 1.5);
            let is_last = self.buffered.len() == 1;
            let action = if gap > drop_threshold && !is_last {
                player_sync::FrameAction::Drop
            } else if pts <= now_clock {
                player_sync::FrameAction::PresentNow
            } else {
                player_sync::FrameAction::WaitFor(pts - now_clock)
            };

            let wall_ok = self.frame_timer.map(|ft| now >= ft).unwrap_or(true);
            let catch_up = pts + Duration::from_secs_f64(self.frame_duration) < now_clock;

            match action {
                player_sync::FrameAction::Drop => {
                    log::trace!("[vs] drop frame pts={:?} gap={:?}", pts, gap);
                    self.buffered.remove(0);
                    request_frame();
                    continue;
                }
                // Wait for the next poll unless we're >1 frame behind the audio
                // clock (catch-up).  This prevents presenting frames in the
                // future during pause when the audio clock has stopped.
                player_sync::FrameAction::WaitFor(_) if !catch_up => {
                    request_frame();
                    break;
                }
                player_sync::FrameAction::WaitFor(_) | player_sync::FrameAction::PresentNow => {
                    // Soft wall-clock pacing: skip if not enough time has elapsed
                    // since the last presented frame.
                    if !wall_ok {
                        request_frame();
                        break;
                    }

                    let frame = self.buffered.remove(0);

                    // Upload to inactive handle
                    let inactive = 1 - self.active_idx;
                    let handle = self.handles[inactive].unwrap();
                    ctx.set_image_nv12(
                        handle,
                        frame.width,
                        frame.height,
                        frame.y_plane,
                        frame.uv_plane,
                        frame.color_info,
                    );

                    // Swap active handle
                    self.active_idx = inactive;
                    self.visible = true;

                    // Advance frame timer
                    self.frame_timer = Some(now + Duration::from_secs_f64(self.frame_duration));

                    request_frame();
                    break;
                }
            }
        }
    }

    fn active_handle(&self) -> Option<ImageHandle> {
        if self.visible {
            self.handles[self.active_idx]
        } else {
            None
        }
    }

    fn aspect(&self) -> f32 {
        self.aspect.clamp(0.25, 4.0)
    }
}

#[cfg(not(any(target_os = "android", target_arch = "wasm32")))]
pub fn run_desktop() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    player_platform::init();

    let player = AudioPlayer::spawn()?;
    let video_sink = Rc::new(RefCell::new(VideoSink::new(
        player.video_rx(),
        player.clone(),
    )));
    let args: Vec<MediaSource> = std::env::args()
        .skip(1)
        .map(|p| MediaSource::Path(std::path::PathBuf::from(p)))
        .collect();
    let auto_fs = args.first().map(is_video_source).unwrap_or(false);
    let pending: PendingFiles = Arc::new(PendingState {
        files: Mutex::new(args),
        probed_meta: Mutex::new(Vec::new()),
        needs_wake: AtomicBool::new(false),
        next_id: AtomicU64::new(0),
        auto_fullscreen: AtomicBool::new(auto_fs),
    });

    repose_platform::run_desktop_app(move |_sched, ctx| {
        video_sink.borrow_mut().poll(ctx);
        App(player.clone(), pending.clone(), &video_sink, ctx)
    })
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub async fn wasm_main() {
    console_error_panic_hook::set_once();
    console_log::init_with_level(log::Level::Info).ok();

    if let Err(e) = player_platform::wasm_persist::init().await {
        log::error!("OPFS init failed: {e}");
    }

    player_platform::init();

    let player = AudioPlayer::spawn().expect("failed to spawn audio player");
    let video_sink = Rc::new(RefCell::new(VideoSink::new(
        player.video_rx(),
        player.clone(),
    )));
    let pending: PendingFiles = Arc::new(PendingState {
        files: Mutex::new(Vec::new()),
        probed_meta: Mutex::new(Vec::new()),
        needs_wake: AtomicBool::new(false),
        next_id: AtomicU64::new(0),
        auto_fullscreen: AtomicBool::new(false),
    });

    let canvas = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("repadio_canvas"))
        .or_else(|| {
            web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.body().map(web_sys::Element::from))
        });
    if let Some(el) = canvas {
        use wasm_bindgen::JsCast;
        let mut resumed = false;
        let closure = wasm_bindgen::closure::Closure::wrap(Box::new(move || {
            if !resumed {
                resumed = true;
                AudioPlayer::resume_audio();
            }
        }) as Box<dyn FnMut()>);
        let cb: &js_sys::Function = closure.as_ref().unchecked_ref();
        let _ = el.add_event_listener_with_callback("click", cb);
        let _ = el.add_event_listener_with_callback("touchstart", cb);
        closure.forget();
    }

    {
        let vs = video_sink.clone();
        repose_platform::web::run_web_app(
            move |_sched, ctx| {
                vs.borrow_mut().poll(ctx);
                App(player.clone(), pending.clone(), &vs, ctx)
            },
            repose_platform::web::WebOptions::new(None),
        )
        .expect("app run failed");
    }
}

#[cfg(target_os = "android")]
fn intent_to_media_source(dir: &std::path::Path) -> Option<MediaSource> {
    let intent = rlobkit_app_events::take_pending_intent(dir)?;
    Some(MediaSource::Bytes {
        name: intent.name,
        bytes: Arc::from(intent.data),
    })
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "C" fn android_main(android_app: winit::platform::android::activity::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );
    rlobkit_dialogs::init_shared_pending_state();
    rlobkit_dialogs::init_with_android_context(
        android_app.vm_as_ptr().cast(),
        android_app.activity_as_ptr().cast(),
    );
    player_platform::init();

    log::info!(
        "rlobkit helper activity available: {}",
        rlobkit_dialogs::helper_activity_available_for_host()
    );

    rlobkit_app_events::insets::set_on_insets(Box::new(|insets| {
        let r = repose_core::locals::WindowInsets {
            top: insets.top,
            bottom: insets.bottom,
            left: insets.left,
            right: insets.right,
            ime_bottom: insets.ime_bottom,
        };
        repose_core::locals::set_window_insets_default(r);
    }));

    let data_dir = android_app.internal_data_path();

    let mut initial = Vec::new();
    let mut auto_fs = false;
    if let Some(ref dir) = data_dir {
        if let Some(src) = intent_to_media_source(dir) {
            log::info!("loaded pending intent file");
            auto_fs = is_video_source(&src);
            initial.push(src);
        }
    }

    let player = AudioPlayer::spawn().expect("failed to spawn audio player");
    let video_sink = Rc::new(RefCell::new(VideoSink::new(
        player.video_rx(),
        player.clone(),
    )));
    let pending: PendingFiles = Arc::new(PendingState {
        files: Mutex::new(initial),
        probed_meta: Mutex::new(Vec::new()),
        needs_wake: AtomicBool::new(false),
        next_id: AtomicU64::new(0),
        auto_fullscreen: AtomicBool::new(auto_fs),
    });

    {
        let vs = video_sink.clone();
        if let Err(err) =
            repose_platform::android::run_android_app(android_app, move |_sched, ctx| {
                // Poll for onNewIntent imports while the app is already running.
                if let Some(ref dir) = data_dir {
                    if let Some(src) = intent_to_media_source(dir) {
                        log::info!("loaded late pending intent");
                        if is_video_source(&src) {
                            pending.auto_fullscreen.store(true, Ordering::Release);
                        }
                        pending.files.lock().unwrap().push(src);
                        request_frame();
                    }
                }
                vs.borrow_mut().poll(ctx);
                App(player.clone(), pending.clone(), &vs, ctx)
            })
        {
            log::error!("Repadio failed: {err:?}");
        }
    }
}

fn App(
    player: AudioPlayer,
    pending: PendingFiles,
    video_sink: &Rc<RefCell<VideoSink>>,
    ctx: &RenderContext,
) -> View {
    let playlist = remember(|| signal(Vec::<Entry>::new()));
    let current = remember(|| signal(None::<usize>));
    let volume = remember(|| signal(1.0f32));
    let advance_armed = remember(|| signal(true));
    let pending_advance = remember(|| signal(false));
    let ended_index = remember(|| signal(None::<usize>));
    let scrubbing = remember(|| signal(None::<f32>));
    let dismissed_error = remember(|| signal(None::<String>));
    let ui_tick = remember(|| signal(Instant::now()));
    let settings = remember(|| signal(load_settings_sync()));
    let show_settings = remember(|| signal(false));
    let is_fullscreen = remember(|| signal(false));
    let speed = remember(|| signal(player.playback_rate()));
    let art_cache = remember(|| Rc::new(RefCell::new(ArtCache::new())));
    {
        let needs_wake = pending.needs_wake.swap(false, Ordering::AcqRel);

        // Drain background metadata probes and update matching entries.
        let probed = pending
            .probed_meta
            .lock()
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>();
        if !probed.is_empty() {
            let mut list = playlist.get();
            for (source, meta) in probed {
                if let Some(entry) = list.iter_mut().find(|e| e.source == source) {
                    entry.meta = meta;
                }
            }
            playlist.set(list);
        }

        let new_files: Vec<MediaSource> = pending.files.lock().unwrap().drain(..).collect();
        if !new_files.is_empty() {
            let mut list = playlist.get();
            let was_empty = list.is_empty();
            for source in &new_files {
                let id = pending.next_id.fetch_add(1, Ordering::Relaxed);
                list.push(Entry {
                    id,
                    source: source.clone(),
                    meta: TrackMeta::default(),
                });
            }
            playlist.set(list.clone());
            if was_empty && !list.is_empty() {
                current.set(Some(0));
                if settings.get().auto_fullscreen_on_open && is_video_source(&list[0].source) {
                    pending.auto_fullscreen.store(true, Ordering::Release);
                }
                if let Err(e) = player.load(list[0].source.clone()) {
                    log::error!("load first track failed: {e}");
                }
                if settings.get().remember_speed {
                    let s = settings.get().default_speed;
                    let _ = player.set_speed(s);
                    speed.set(s);
                }
                advance_armed.set(true);
            }

            // Probe metadata in background (blocking I/O).
            let pending = pending.clone();
            let to_probe = new_files.clone();
            thread::spawn(move || {
                for source in to_probe {
                    let meta = probe_media_source(source.clone());
                    pending.probed_meta.lock().unwrap().push((source, meta));
                    pending.needs_wake.store(true, Ordering::Release);
                }
            });
        }
        if needs_wake || !new_files.is_empty() {
            request_frame();
        }
    }

    let snap = player.snapshot();
    let has_video_frame = video_sink.borrow().active_handle().is_some();
    let has_video = has_video_frame || snap.has_video || player.has_video();

    // Upload art thumbnails (long-lived handles, only re-uploaded on change).
    if settings.get().show_playlist_thumbs {
        let mut cache = art_cache.borrow_mut();
        let list = playlist.get();
        for e in list.iter() {
            let _ = cache.ensure(ctx, e.id, &e.meta.art);
        }
        if let Some(idx) = current.get()
            && let Some(e) = list.get(idx)
        {
            let art = snap.art.clone().or_else(|| e.meta.art.clone());
            let _ = cache.ensure(ctx, e.id, &art);
        }
    }
    let thumbs: Rc<std::collections::HashMap<u64, ImageHandle>> =
        Rc::new(art_cache.borrow().map.clone());
    let list = playlist.get();
    let now_playing_thumb: Option<ImageHandle> = current
        .get()
        .and_then(|idx| list.get(idx))
        .and_then(|e| thumbs.get(&e.id).copied());

    // Enter fullscreen as soon as we know the item is video (or the first
    // frame arrived). The flag stays set until the user explicitly exits so a
    // transient Loading→Playing transition doesn't flicker the UI.
    if pending.auto_fullscreen.load(Ordering::Acquire) && has_video && !is_fullscreen.get() {
        is_fullscreen.set(true);
    }

    if show_settings.get() {
        return SettingsScreen(
            settings.clone(),
            show_settings.clone(),
            player.clone(),
            speed.clone(),
        );
    }

    if is_fullscreen.get() && has_video {
        return FullscreenVideo(
            player.clone(),
            snap.clone(),
            video_sink,
            is_fullscreen.clone(),
            pending.clone(),
            settings.clone(),
            speed.clone(),
        );
    }

    if matches!(
        snap.state,
        PlaybackState::Playing | PlaybackState::Loading | PlaybackState::Buffering
    ) || scrubbing.get().is_some()
    {
        if ui_tick.get().elapsed() >= Duration::from_millis(125) {
            ui_tick.set(Instant::now());
            request_frame();
        }
    }

    // Keep frame alive if probe thread posted results
    // between frames.
    if pending.needs_wake.load(Ordering::Relaxed) || !pending.files.lock().unwrap().is_empty() {
        request_frame();
    }

    if pending_advance.get() {
        pending_advance.set(false);
        if let Some(prev_idx) = ended_index.get() {
            ended_index.set(None);
            if snap.state == PlaybackState::Ended {
                let list = playlist.get();
                let target = match current.get() {
                    Some(c) if c != prev_idx => c,
                    _ => prev_idx + 1,
                };
                if target < list.len() {
                    current.set(Some(target));
                    if let Err(e) = player.load(list[target].source.clone()) {
                        log::error!("advance load failed: {e}");
                    }
                }
            }
        }
    }

    // Schedule advance for NEXT frame if track just ended
    if snap.state == PlaybackState::Ended && advance_armed.get() {
        advance_armed.set(false);
        ended_index.set(current.get());
        pending_advance.set(true);
    }

    if matches!(
        snap.state,
        PlaybackState::Playing | PlaybackState::Loading | PlaybackState::Buffering
    ) {
        advance_armed.set(true);
    }

    let playlist_len = playlist.get().len();

    let top_bar = m3::TopAppBar(
        Row(Modifier::new().gap(10.0)).child((
            Icon(Symbols::graphic_eq).size(22.0).color(theme().primary),
            Text("Repadio").size(20.0),
        )),
        None,
        None,
        vec![m3::IconButton(
            Icon(Symbols::settings).size(22.0),
            {
                let show_settings = show_settings.clone();
                move || show_settings.set(true)
            },
            m3::IconButtonConfig::default(),
        )],
        m3::TopAppBarConfig::default(),
    );

    let fab = m3::FAB(
        Icon(Symbols::add).size(24.0),
        {
            let pending = pending.clone();
            move || {
                let pending = pending.clone();
                player_platform::pick_audio_files_async(move |picked| {
                    if !picked.is_empty() {
                        let sources: Vec<MediaSource> = picked
                            .into_iter()
                            .map(|f| match f {
                                player_platform::PickedFile::Path(p) => MediaSource::Path(p),
                                player_platform::PickedFile::Bytes { name, data } => {
                                    MediaSource::Bytes {
                                        name,
                                        bytes: Arc::from(data),
                                    }
                                }
                            })
                            .collect();
                        pending.files.lock().unwrap().extend(sources);
                        pending.needs_wake.store(true, Ordering::Release);
                        request_frame();
                        #[cfg(not(target_arch = "wasm32"))]
                        repose_platform::wake_event_loop();
                    }
                });
            }
        },
        m3::FABConfig::default(),
    );

    let key_handler = make_player_key_handler(
        player.clone(),
        snap.clone(),
        is_fullscreen.clone(),
        has_video,
        settings.clone(),
        speed.clone(),
        volume.clone(),
        show_settings.clone(),
    );

    m3::Scaffold(
        move |_padding| {
            Column(
                Modifier::new()
                    .fill_max_size()
                    .padding_values(PaddingValues {
                        top: 12.0,
                        bottom: 12.0,
                        left: 12.0,
                        right: 12.0,
                    })
                    .gap(16.0),
            )
            .child((
                ErrorBanner(snap.error.clone(), dismissed_error.clone()),
                NowPlayingCard(
                    player.clone(),
                    snap.clone(),
                    scrubbing.clone(),
                    video_sink,
                    is_fullscreen.clone(),
                    now_playing_thumb,
                ),
                TransportBar(
                    player.clone(),
                    playlist.clone(),
                    current.clone(),
                    snap.state,
                    speed.clone(),
                    settings.clone(),
                ),
                VolumeRow(player.clone(), volume.clone(), snap.muted),
                PlaylistHeader(playlist_len),
                if playlist_len == 0 {
                    EmptyPlaylist(pending.clone())
                } else {
                    PlaylistList(
                        playlist.clone(),
                        current.clone(),
                        player.clone(),
                        thumbs.clone(),
                    )
                },
            ))
        },
        m3::ScaffoldConfig {
            top_bar: Some(top_bar),
            floating_action_button: Some(fab),
            ..Default::default()
        },
    )
    .modifier(
        Modifier::new()
            .focusable(true)
            .focus_target()
            .on_key_event(key_handler),
    )
}

fn ErrorBanner(error: Option<String>, dismissed: Rc<Signal<Option<String>>>) -> View {
    let should_show = match (&error, &dismissed.get()) {
        (Some(e), Some(d)) => e != d,
        (Some(_), None) => true,
        (None, _) => false,
    };

    if !should_show {
        return Box(Modifier::new().height(0.0));
    }

    let msg = error.clone().unwrap_or_default();

    Row(Modifier::new()
        .fill_max_width()
        .padding(14.0)
        .clip_rounded(16.0)
        .background(theme().error_container)
        .gap(12.0)
        .align_items(AlignItems::CENTER))
    .child((
        Icon(Symbols::error_icon)
            .size(20.0)
            .color(theme().on_error_container),
        Text(msg)
            .size(13.0)
            .color(theme().on_error_container)
            .modifier(Modifier::new().weight(1.0)),
        m3::IconButton(
            Icon(Symbols::close)
                .size(18.0)
                .color(theme().on_error_container),
            {
                let dismissed = dismissed.clone();
                move || dismissed.set(error.clone())
            },
            m3::IconButtonConfig {
                container_size: Some(28.0),
                ..Default::default()
            },
        ),
    ))
}

fn NowPlayingCard(
    player: AudioPlayer,
    snap: player_core::PlayerSnapshot,
    scrubbing: Rc<Signal<Option<f32>>>,
    video_sink: &Rc<RefCell<VideoSink>>,
    is_fullscreen: Rc<Signal<bool>>,
    art_thumb: Option<ImageHandle>,
) -> View {
    let title = snap
        .title
        .clone()
        .unwrap_or_else(|| "No track loaded".into());
    let sub_line = match (&snap.artist, &snap.album) {
        (Some(ar), Some(al)) => format!("{ar} - {al}"),
        (Some(ar), None) => ar.clone(),
        (None, Some(al)) => al.clone(),
        (None, None) => "Add a track to get started".into(),
    };

    let pos = format_duration(snap.position);
    let dur = snap
        .duration
        .map(format_duration)
        .unwrap_or_else(|| "--:--".into());

    let slider_value = scrubbing
        .get()
        .unwrap_or_else(|| progress_ratio(snap.position, snap.duration));

    let is_playing = snap.state == PlaybackState::Playing;

    let art_bg = if is_playing {
        theme().primary_container
    } else {
        theme().surface_variant
    };
    let art_fg = if is_playing {
        theme().on_primary_container
    } else {
        theme().on_surface.with_alpha(120)
    };

    let video_handle = video_sink.borrow().active_handle();
    let has_video = video_handle.is_some();

    Column(
        Modifier::new()
            .fill_max_width()
            .padding(20.0)
            .clip_rounded(24.0)
            .background(theme().surface_variant.with_alpha(70))
            .gap(16.0),
    )
    .child((
        if has_video {
            let aspect = video_sink.borrow().aspect();
            Column(Modifier::new().fill_max_width().gap(12.0)).child((
                Box(Modifier::new()
                    .fill_max_width()
                    .aspect_ratio(aspect)
                    .clip_rounded(20.0)
                    .background(art_bg))
                .child(
                    ZStack(Modifier::new().fill_max_size().on_double_click({
                        let is_fullscreen = is_fullscreen.clone();
                        move || is_fullscreen.set(true)
                    }))
                    .child((
                        Image(Modifier::new().fill_max_size(), video_handle.unwrap()),
                        if snap.state == PlaybackState::Buffering {
                            Box(Modifier::new()
                                .fill_max_size()
                                .background(theme().surface.with_alpha(100))
                                .align_items(AlignItems::CENTER)
                                .justify_content(JustifyContent::CENTER))
                            .child(m3::CircularProgressIndicator(
                                None,
                                m3::CircularProgressIndicatorConfig {
                                    color: theme().primary,
                                    ..Default::default()
                                },
                            ))
                        } else {
                            Box(Modifier::new().height(0.0))
                        },
                        Box(Modifier::new()
                            .fill_max_size()
                            .align_items(AlignItems::FLEX_END)
                            .justify_content(JustifyContent::FLEX_END)
                            .padding(10.0)
                            .hit_passthrough())
                        .child(
                            Box(Modifier::new()
                                .background(Color::BLACK.with_alpha(120))
                                .clip_rounded(20.0)
                                .padding(4.0))
                            .child(m3::IconButton(
                                Icon(Symbols::fullscreen).size(20.0).color(Color::WHITE),
                                {
                                    let is_fullscreen = is_fullscreen.clone();
                                    move || is_fullscreen.set(true)
                                },
                                m3::IconButtonConfig {
                                    container_size: Some(36.0),
                                    ..Default::default()
                                },
                            )),
                        ),
                    )),
                ),
                Row(Modifier::new()
                    .fill_max_width()
                    .gap(16.0)
                    .align_items(AlignItems::CENTER))
                .child((Column(Modifier::new().weight(1.0).gap(6.0)).child((
                    Text(title).size(20.0).single_line().overflow_ellipsize(),
                    Text(sub_line)
                        .size(13.0)
                        .color(theme().on_surface.with_alpha(170))
                        .single_line()
                        .overflow_ellipsize(),
                    StatusChip(snap.state),
                )),)),
            ))
        } else {
            Row(Modifier::new()
                .fill_max_width()
                .gap(16.0)
                .align_items(AlignItems::CENTER))
            .child((
                Box(Modifier::new()
                    .width(88.0)
                    .height(88.0)
                    .clip_rounded(20.0)
                    .background(art_bg)
                    .align_items(AlignItems::CENTER)
                    .justify_content(JustifyContent::CENTER))
                .child(if let Some(h) = art_thumb {
                    Image(Modifier::new().fill_max_size(), h)
                } else {
                    Icon(if snap.art.is_some() {
                        Symbols::image
                    } else {
                        Symbols::music_note
                    })
                    .size(36.0)
                    .color(art_fg)
                }),
                Column(Modifier::new().weight(1.0).gap(6.0)).child((
                    Text(title).size(20.0).single_line().overflow_ellipsize(),
                    Text(sub_line)
                        .size(13.0)
                        .color(theme().on_surface.with_alpha(170))
                        .single_line()
                        .overflow_ellipsize(),
                    StatusChip(snap.state),
                )),
            ))
        },
        Column(Modifier::new().fill_max_width().gap(4.0)).child((
            m3::Slider(
                slider_value,
                (0.0, 1.0),
                None,
                {
                    let player = player.clone();
                    let scrubbing = scrubbing.clone();
                    let duration = snap.duration;
                    move |ratio: f32| {
                        let was_scrubbing = scrubbing.get().is_some();
                        scrubbing.set(Some(ratio));
                        if !was_scrubbing {
                            // Pause audio while scrubbing (prevents glitchy sounds)
                            let _ = player.pause();
                        }
                        // Show target time during scrub.
                        let _target_time = duration.map(|d| {
                            format_duration(Duration::from_secs_f64(d.as_secs_f64() * ratio as f64))
                        });
                    }
                },
                m3::SliderConfig {
                    enabled: snap.duration.is_some(),
                    modifier: Modifier::new().fill_max_width(),
                    on_value_change_finished: {
                        let player = player.clone();
                        let scrubbing = scrubbing.clone();
                        let duration = snap.duration;
                        Some(std::rc::Rc::new(move || {
                            let ratio = scrubbing.get().unwrap_or(0.0);
                            scrubbing.set(None);
                            if let Some(d) = duration {
                                let target =
                                    Duration::from_secs_f64(d.as_secs_f64() * ratio as f64);
                                if let Err(e) = player.seek(target) {
                                    log::error!("seek failed: {e}");
                                }
                                let _ = player.play();
                            }
                        }))
                    },
                    ..Default::default()
                },
            ),
            Row(Modifier::new().fill_max_width()).child((
                Text(pos)
                    .size(12.0)
                    .color(theme().on_surface.with_alpha(150)),
                Spacer(),
                Text(dur)
                    .size(12.0)
                    .color(theme().on_surface.with_alpha(150)),
            )),
        )),
    ))
}

fn FullscreenVideo(
    player: AudioPlayer,
    snap: player_core::PlayerSnapshot,
    video_sink: &Rc<RefCell<VideoSink>>,
    is_fullscreen: Rc<Signal<bool>>,
    pending: PendingFiles,
    settings: Rc<Signal<PlayerSettings>>,
    speed: Rc<Signal<f32>>,
) -> View {
    let handle = video_sink.borrow().active_handle();
    let aspect = video_sink.borrow().aspect();
    let slider_value = progress_ratio(snap.position, snap.duration);

    let controls_visible = remember(|| signal(true));
    let last_activity = remember(|| signal(Instant::now()));
    let scrubbing = remember(|| signal(false));
    let osd = remember(|| signal(Option::<(String, Instant)>::None));

    let exit_fs = {
        let is_fullscreen = is_fullscreen.clone();
        let pending = pending.clone();
        Rc::new(move || {
            is_fullscreen.set(false);
            pending.auto_fullscreen.store(false, Ordering::Release);
        })
    };

    let hide_after = Duration::from_millis(settings.get().hide_controls_ms.max(500));

    let bump_activity = {
        let controls_visible = controls_visible.clone();
        let last_activity = last_activity.clone();
        Rc::new(move || {
            last_activity.set(Instant::now());
            controls_visible.set(true);
            request_frame();
        })
    };

    let show_osd = {
        let osd = osd.clone();
        let bump = bump_activity.clone();
        Rc::new(move |msg: String| {
            osd.set(Some((msg, Instant::now())));
            bump();
            request_frame();
        })
    };

    {
        let playing = snap.state == PlaybackState::Playing;
        let scrub = scrubbing.get();
        if controls_visible.get() && playing && !scrub {
            if last_activity.get().elapsed() >= hide_after {
                controls_visible.set(false);
            } else {
                request_frame();
            }
        }
        if let Some((_, t)) = osd.get() {
            if t.elapsed() >= Duration::from_millis(OSD_MS) {
                osd.set(None);
            } else {
                request_frame();
            }
        }
    }

    let show_chrome =
        controls_visible.get() || scrubbing.get() || snap.state != PlaybackState::Playing;

    let title = snap
        .title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Now playing".into());

    let pos_label = format_position(snap.position);
    let dur_label = snap
        .duration
        .map(format_position)
        .unwrap_or_else(|| "--:--".into());

    let osd_pos = pos_label.clone();
    let osd_dur = dur_label.clone();

    let key_handler = {
        let player = player.clone();
        let snap = snap.clone();
        let bump = bump_activity.clone();
        let show_osd = show_osd.clone();
        let exit_fs = exit_fs.clone();
        let speed = speed.clone();
        move |ev: KeyEvent| -> bool {
            if ev.event_type != KeyEventType::Down {
                return false;
            }
            bump();
            match &ev.key {
                Key::Escape | Key::Character('f') | Key::Character('F') | Key::F(11) => {
                    exit_fs();
                    true
                }
                Key::Space | Key::Character('p') | Key::Character('P') => {
                    let _ = player.toggle();
                    true
                }
                Key::ArrowLeft if !ev.modifiers.shift => {
                    relative_seek(&player, &snap, if ev.modifiers.ctrl { -1.0 } else { -5.0 });
                    show_osd(format!(
                        "Seek {}",
                        if ev.modifiers.ctrl { "-1s" } else { "-5s" }
                    ));
                    true
                }
                Key::ArrowRight if !ev.modifiers.shift => {
                    relative_seek(&player, &snap, if ev.modifiers.ctrl { 1.0 } else { 5.0 });
                    show_osd(format!(
                        "Seek {}",
                        if ev.modifiers.ctrl { "+1s" } else { "+5s" }
                    ));
                    true
                }
                Key::ArrowLeft if ev.modifiers.shift => {
                    relative_seek(&player, &snap, -1.0);
                    show_osd("Seek -1s".into());
                    true
                }
                Key::ArrowRight if ev.modifiers.shift => {
                    relative_seek(&player, &snap, 1.0);
                    show_osd("Seek +1s".into());
                    true
                }
                Key::ArrowUp => {
                    relative_seek(&player, &snap, 60.0);
                    show_osd("Seek +1m".into());
                    true
                }
                Key::ArrowDown => {
                    relative_seek(&player, &snap, -60.0);
                    show_osd("Seek -1m".into());
                    true
                }
                Key::Character('m') | Key::Character('M') => {
                    let _ = player.toggle_mute();
                    show_osd(if snap.muted {
                        "Unmuted".into()
                    } else {
                        "Muted".into()
                    });
                    true
                }
                Key::Character('[') => {
                    let next = step_speed(speed.get(), false);
                    speed.set(next);
                    let _ = player.set_speed(next);
                    show_osd(format!("Speed {next:.2}x"));
                    true
                }
                Key::Character(']') => {
                    let next = step_speed(speed.get(), true);
                    speed.set(next);
                    let _ = player.set_speed(next);
                    show_osd(format!("Speed {next:.2}x"));
                    true
                }
                Key::Home => {
                    let _ = player.seek(Duration::ZERO);
                    show_osd("Start".into());
                    true
                }
                Key::Character('o') | Key::Character('O') => {
                    bump();
                    show_osd(format!("{osd_pos} / {osd_dur}"));
                    true
                }
                _ => false,
            }
        }
    };

    ZStack(
        Modifier::new()
            .fill_max_size()
            .background(Color::BLACK)
            .focusable(true)
            .focus_target()
            .on_key_event(key_handler)
            .on_pointer_move({
                let bump = bump_activity.clone();
                move |_| bump()
            })
            .on_click({
                let player = player.clone();
                let bump = bump_activity.clone();
                move || {
                    bump();
                    let _ = player.toggle();
                }
            })
            .on_double_click({
                let exit_fs = exit_fs.clone();
                move || exit_fs()
            }),
    )
    .child((
        Box(Modifier::new()
            .fill_max_size()
            .align_items(AlignItems::CENTER)
            .justify_content(JustifyContent::CENTER)
            .hit_passthrough())
        .child(
            Box(Modifier::new()
                .fill_max_width()
                .aspect_ratio(aspect)
                .hit_passthrough())
            .child(
                handle
                    .map(|h| Image(Modifier::new().fill_max_size().hit_passthrough(), h))
                    .unwrap_or_else(|| Box(Modifier::new().fill_max_size())),
            ),
        ),
        if snap.state == PlaybackState::Buffering || snap.state == PlaybackState::Loading {
            Box(Modifier::new()
                .fill_max_size()
                .background(theme().surface.with_alpha(80))
                .align_items(AlignItems::CENTER)
                .justify_content(JustifyContent::CENTER))
            .child(m3::CircularProgressIndicator(
                None,
                m3::CircularProgressIndicatorConfig {
                    color: theme().primary,
                    ..Default::default()
                },
            ))
        } else {
            Box(Modifier::new().height(0.0))
        },
        if let Some((ref msg, _)) = osd.get() {
            Box(Modifier::new()
                .fill_max_size()
                .align_items(AlignItems::CENTER)
                .justify_content(JustifyContent::CENTER)
                .hit_passthrough())
            .child(
                Box(Modifier::new()
                    .padding_values(PaddingValues {
                        left: 16.0,
                        right: 16.0,
                        top: 10.0,
                        bottom: 10.0,
                    })
                    .background(Color::BLACK.with_alpha(180))
                    .clip_rounded(8.0))
                .child(
                    Text(msg.clone())
                        .size(18.0)
                        .color(Color::WHITE)
                        .font_weight(FontWeight::MEDIUM),
                ),
            )
        } else {
            Box(Modifier::new().height(0.0))
        },
        if show_chrome {
            ZStack(Modifier::new().fill_max_size()).child((
                Column(
                    Modifier::new()
                        .fill_max_width()
                        .align_self(AlignSelf::START)
                        .background(Color::BLACK.with_alpha(160))
                        .padding_values(PaddingValues {
                            left: 16.0,
                            right: 8.0,
                            top: 10.0,
                            bottom: 12.0,
                        }),
                )
                .child(
                    Row(Modifier::new()
                        .fill_max_width()
                        .align_items(AlignItems::CENTER)
                        .gap(12.0))
                    .child((
                        Text(title)
                            .size(16.0)
                            .color(Color::WHITE)
                            .font_weight(FontWeight::MEDIUM)
                            .max_lines(1)
                            .overflow_ellipsize(),
                        Spacer(),
                        StatusChip(snap.state),
                        m3::IconButton(
                            Icon(Symbols::fullscreen_exit)
                                .size(22.0)
                                .color(Color::WHITE),
                            {
                                let exit_fs = exit_fs.clone();
                                move || exit_fs()
                            },
                            m3::IconButtonConfig {
                                container_size: Some(40.0),
                                ..Default::default()
                            },
                        ),
                    )),
                ),
                Column(
                    Modifier::new()
                        .fill_max_width()
                        .align_self(AlignSelf::END)
                        .background(Color::BLACK.with_alpha(170))
                        .padding_values(PaddingValues {
                            left: 16.0,
                            right: 16.0,
                            top: 10.0,
                            bottom: 14.0,
                        })
                        .gap(6.0)
                        .on_pointer_enter({
                            let bump = bump_activity.clone();
                            move |_| bump()
                        })
                        .on_pointer_move({
                            let bump = bump_activity.clone();
                            move |_| bump()
                        })
                        .on_pointer_up({
                            let scrubbing = scrubbing.clone();
                            move |_| scrubbing.set(false)
                        }),
                )
                .child((
                    Row(Modifier::new()
                        .fill_max_width()
                        .align_items(AlignItems::CENTER)
                        .gap(10.0))
                    .child((
                        Text(pos_label)
                            .size(13.0)
                            .color(Color::WHITE.with_alpha(220))
                            .font_family("monospace")
                            .single_line(),
                        m3::Slider(
                            slider_value,
                            (0.0, 1.0),
                            None,
                            {
                                let player = player.clone();
                                let duration = snap.duration;
                                let scrubbing = scrubbing.clone();
                                let bump = bump_activity.clone();
                                move |ratio: f32| {
                                    scrubbing.set(true);
                                    bump();
                                    if let Some(d) = duration {
                                        let target =
                                            Duration::from_secs_f64(d.as_secs_f64() * ratio as f64);
                                        let _ = player.seek(target);
                                    }
                                }
                            },
                            m3::SliderConfig {
                                enabled: snap.duration.is_some(),
                                modifier: Modifier::new().fill_max_width().flex_grow(1.0),
                                ..Default::default()
                            },
                        ),
                        Text(dur_label)
                            .size(13.0)
                            .color(Color::WHITE.with_alpha(220))
                            .font_family("monospace")
                            .single_line(),
                    )),
                    Row(Modifier::new()
                        .fill_max_width()
                        .align_items(AlignItems::CENTER)
                        .justify_content(JustifyContent::CENTER)
                        .gap(8.0))
                    .child((
                        m3::IconButton(
                            Icon(Symbols::replay_10).size(24.0).color(Color::WHITE),
                            {
                                let player = player.clone();
                                let snap = snap.clone();
                                let show_osd = show_osd.clone();
                                let bump = bump_activity.clone();
                                move || {
                                    bump();
                                    relative_seek(&player, &snap, -10.0);
                                    show_osd("−10s".into());
                                }
                            },
                            m3::IconButtonConfig {
                                container_size: Some(44.0),
                                ..Default::default()
                            },
                        ),
                        m3::FilledIconButton(
                            Icon(if snap.state == PlaybackState::Playing {
                                Symbols::pause
                            } else {
                                Symbols::play_arrow
                            })
                            .size(28.0),
                            {
                                let player = player.clone();
                                let bump = bump_activity.clone();
                                move || {
                                    bump();
                                    let _ = player.toggle();
                                }
                            },
                            m3::IconButtonConfig {
                                container_size: Some(56.0),
                                ..Default::default()
                            },
                        ),
                        m3::IconButton(
                            Icon(Symbols::forward_10).size(24.0).color(Color::WHITE),
                            {
                                let player = player.clone();
                                let snap = snap.clone();
                                let show_osd = show_osd.clone();
                                let bump = bump_activity.clone();
                                move || {
                                    bump();
                                    relative_seek(&player, &snap, 10.0);
                                    show_osd("+10s".into());
                                }
                            },
                            m3::IconButtonConfig {
                                container_size: Some(44.0),
                                ..Default::default()
                            },
                        ),
                        m3::AssistChip(
                            {
                                let player = player.clone();
                                let speed = speed.clone();
                                let show_osd = show_osd.clone();
                                let bump = bump_activity.clone();
                                move || {
                                    bump();
                                    let cur = speed.get();
                                    let next = if (cur - *SPEED_STEPS.last().unwrap()).abs() < 0.01
                                    {
                                        SPEED_STEPS[0]
                                    } else {
                                        step_speed(cur, true)
                                    };
                                    speed.set(next);
                                    let _ = player.set_speed(next);
                                    show_osd(format!("Speed {next:.2}x"));
                                }
                            },
                            Text(format!("{:.2}x", speed.get()))
                                .size(13.0)
                                .color(Color::WHITE),
                            None,
                            None,
                            m3::ChipConfig::default(),
                        ),
                        m3::IconButton(
                            Icon(if snap.muted {
                                Symbols::volume_off
                            } else {
                                Symbols::volume_up
                            })
                            .size(22.0)
                            .color(Color::WHITE),
                            {
                                let player = player.clone();
                                let bump = bump_activity.clone();
                                move || {
                                    bump();
                                    let _ = player.toggle_mute();
                                }
                            },
                            m3::IconButtonConfig {
                                container_size: Some(44.0),
                                ..Default::default()
                            },
                        ),
                    )),
                )),
            ))
        } else {
            Box(Modifier::new().height(0.0))
        },
    ))
}

fn StatusChip(state: PlaybackState) -> View {
    let (label, bg, fg) = match state {
        PlaybackState::Playing => (
            "Playing",
            theme().primary_container,
            theme().on_primary_container,
        ),
        PlaybackState::Paused => (
            "Paused",
            theme().secondary_container,
            theme().on_secondary_container,
        ),
        PlaybackState::Loading => (
            "Loading…",
            theme().tertiary_container,
            theme().on_tertiary_container,
        ),
        PlaybackState::Buffering => (
            "Buffering…",
            theme().tertiary_container,
            theme().on_tertiary_container,
        ),
        PlaybackState::Error => ("Error", theme().error_container, theme().on_error_container),
        PlaybackState::Ended => (
            "Ended",
            theme().surface_variant,
            theme().on_surface.with_alpha(180),
        ),
        PlaybackState::Idle => (
            "Idle",
            theme().surface_variant,
            theme().on_surface.with_alpha(180),
        ),
    };

    Box(Modifier::new()
        .padding_values(PaddingValues {
            left: 10.0,
            right: 10.0,
            top: 4.0,
            bottom: 4.0,
        })
        .clip_rounded(999.0)
        .background(bg))
    .child(Text(label).size(11.0).color(fg))
}

fn TransportBar(
    player: AudioPlayer,
    playlist: Rc<Signal<Vec<Entry>>>,
    current: Rc<Signal<Option<usize>>>,
    state: PlaybackState,
    speed: Rc<Signal<f32>>,
    settings: Rc<Signal<PlayerSettings>>,
) -> View {
    Row(Modifier::new()
        .fill_max_width()
        .gap(12.0)
        .align_items(AlignItems::CENTER)
        .justify_content(JustifyContent::CENTER))
    .child((
        m3::OutlinedIconButton(
            Icon(Symbols::skip_previous).size(22.0),
            {
                let player = player.clone();
                let playlist = playlist.clone();
                let current = current.clone();
                move || {
                    let list = playlist.get();
                    if let Some(idx) = current.get()
                        && idx > 0
                    {
                        current.set(Some(idx - 1));
                        if let Err(e) = player.load(list[idx - 1].source.clone()) {
                            log::error!("load previous failed: {e}");
                        }
                    }
                }
            },
            m3::IconButtonConfig {
                container_size: Some(48.0),
                ..Default::default()
            },
        ),
        m3::FilledIconButton(
            Icon(
                if state == PlaybackState::Playing || state == PlaybackState::Buffering {
                    Symbols::pause
                } else {
                    Symbols::play_arrow
                },
            )
            .size(30.0),
            {
                let player = player.clone();
                move || {
                    if let Err(e) = player.toggle() {
                        log::error!("toggle failed: {e}");
                    }
                }
            },
            m3::IconButtonConfig {
                container_size: Some(64.0),
                ..Default::default()
            },
        ),
        m3::OutlinedIconButton(
            Icon(Symbols::skip_next).size(22.0),
            {
                let player = player.clone();
                let playlist = playlist.clone();
                let current = current.clone();
                move || {
                    let list = playlist.get();
                    if let Some(idx) = current.get()
                        && idx + 1 < list.len()
                    {
                        current.set(Some(idx + 1));
                        if let Err(e) = player.load(list[idx + 1].source.clone()) {
                            log::error!("load next failed: {e}");
                        }
                    }
                }
            },
            m3::IconButtonConfig {
                container_size: Some(48.0),
                ..Default::default()
            },
        ),
        m3::AssistChip(
            {
                let player = player.clone();
                let speed = speed.clone();
                let settings = settings.clone();
                move || {
                    let cur = speed.get();
                    let next = if (cur - *SPEED_STEPS.last().unwrap()).abs() < 0.01 {
                        SPEED_STEPS[0]
                    } else {
                        step_speed(cur, true)
                    };
                    speed.set(next);
                    let _ = player.set_speed(next);
                    if settings.get().remember_speed {
                        let mut s = settings.get();
                        s.default_speed = next;
                        save_settings_sync(&s);
                        settings.set(s);
                    }
                }
            },
            Text(format!("{:.2}x", speed.get())).size(12.0),
            Some(Icon(Symbols::speed).size(16.0)),
            None,
            m3::ChipConfig::default(),
        ),
    ))
}

fn VolumeRow(player: AudioPlayer, volume: Rc<Signal<f32>>, muted: bool) -> View {
    let v = volume.get();
    let icon = if muted || v <= 0.001 {
        Symbols::volume_off
    } else if v < 0.6 {
        Symbols::volume_down
    } else {
        Symbols::volume_up
    };

    Row(Modifier::new()
        .fill_max_width()
        .padding_values(PaddingValues {
            left: 4.0,
            right: 4.0,
            top: 0.0,
            bottom: 0.0,
        })
        .gap(10.0)
        .align_items(AlignItems::CENTER))
    .child((
        m3::IconButton(
            Icon(icon)
                .size(18.0)
                .color(theme().on_surface.with_alpha(170)),
            {
                let player = player.clone();
                move || {
                    let _ = player.toggle_mute();
                }
            },
            m3::IconButtonConfig {
                container_size: Some(32.0),
                ..Default::default()
            },
        ),
        m3::Slider(
            v,
            (0.0, 1.5),
            None,
            {
                let volume = volume.clone();
                let player = player.clone();
                move |v| {
                    volume.set(v);
                    if let Err(e) = player.set_volume(v) {
                        log::error!("set volume failed: {e}");
                    }
                }
            },
            m3::SliderConfig {
                modifier: Modifier::new().weight(1.0),
                ..Default::default()
            },
        ),
        Text(format!("{:.0}%", v * 100.0))
            .size(12.0)
            .color(theme().on_surface.with_alpha(170))
            .modifier(Modifier::new().width(40.0)),
    ))
}

fn PlaylistHeader(count: usize) -> View {
    Row(Modifier::new()
        .fill_max_width()
        .gap(8.0)
        .align_items(AlignItems::CENTER))
    .child((
        Text("Playlist")
            .size(16.0)
            .color(theme().on_surface.with_alpha(220)),
        Box(Modifier::new()
            .padding_values(PaddingValues {
                left: 8.0,
                right: 8.0,
                top: 2.0,
                bottom: 2.0,
            })
            .clip_rounded(999.0)
            .background(theme().secondary_container))
        .child(
            Text(count.to_string())
                .size(11.0)
                .color(theme().on_secondary_container),
        ),
        Spacer(),
    ))
}

fn EmptyPlaylist(pending: PendingFiles) -> View {
    Column(
        Modifier::new()
            .fill_max_width()
            .weight(1.0)
            .padding(32.0)
            .gap(12.0),
    )
    .child((
        Spacer(),
        Row(Modifier::new().fill_max_width()).child((
            Spacer(),
            Icon(Symbols::library_music)
                .size(48.0)
                .color(theme().on_surface.with_alpha(100)),
            Spacer(),
        )),
        Row(Modifier::new().fill_max_width()).child((
            Spacer(),
            Text("No tracks yet")
                .size(16.0)
                .color(theme().on_surface.with_alpha(200)),
            Spacer(),
        )),
        Row(Modifier::new().fill_max_width()).child((
            Spacer(),
            Text("Tap + to add media files")
                .size(13.0)
                .color(theme().on_surface.with_alpha(150)),
            Spacer(),
        )),
        Row(Modifier::new().fill_max_width()).child((
            Spacer(),
            m3::FilledTonalButton(
                Modifier::new(),
                {
                    let pending = pending.clone();
                    move || {
                        let pending = pending.clone();
                        player_platform::pick_audio_files_async(move |picked| {
                            if !picked.is_empty() {
                                let sources: Vec<MediaSource> = picked
                                    .into_iter()
                                    .map(|f| match f {
                                        player_platform::PickedFile::Path(p) => {
                                            MediaSource::Path(p)
                                        }
                                        player_platform::PickedFile::Bytes { name, data } => {
                                            MediaSource::Bytes {
                                                name,
                                                bytes: Arc::from(data),
                                            }
                                        }
                                    })
                                    .collect();
                                pending.files.lock().unwrap().extend(sources);
                                pending.needs_wake.store(true, Ordering::Release);
                                request_frame();
                                #[cfg(not(target_arch = "wasm32"))]
                                repose_platform::wake_event_loop();
                            }
                        });
                    }
                },
                m3::ButtonConfig::default(),
                || Row(Modifier::new().gap(8.0)).child((Icon(Symbols::add), Text("Add files"))),
            ),
            Spacer(),
        )),
        Spacer(),
    ))
}

fn PlaylistList(
    playlist: Rc<Signal<Vec<Entry>>>,
    current: Rc<Signal<Option<usize>>>,
    player: AudioPlayer,
    thumbs: Rc<std::collections::HashMap<u64, ImageHandle>>,
) -> View {
    let list = playlist.get();
    let lazy_state: Rc<LazyColumnState> = remember(LazyColumnState::new);

    LazyColumn(
        list,
        68.0f32,
        |entry: &Entry| entry.id,
        {
            let current = current.clone();
            let player = player.clone();
            let thumbs = thumbs.clone();
            move |entry: Entry, idx: usize| {
                let thumb = thumbs.get(&entry.id).copied();
                TrackRow(entry, idx, current.clone(), player.clone(), thumb)
            }
        },
        LazyColumnConfig {
            state: lazy_state.clone(),
            modifier: Modifier::new().fill_max_width().weight(1.0),
            ..Default::default()
        },
    )
}

fn TrackRow(
    entry: Entry,
    idx: usize,
    current: Rc<Signal<Option<usize>>>,
    player: AudioPlayer,
    thumb: Option<ImageHandle>,
) -> View {
    let is_current = current.get() == Some(idx);
    let row_source = entry.source.clone();

    let leading_bg = if is_current {
        theme().primary_container
    } else {
        theme().surface_variant.with_alpha(80)
    };
    let leading_fg = if is_current {
        theme().on_primary_container
    } else {
        theme().on_surface.with_alpha(140)
    };

    Column(Modifier::new().fill_max_width()).child((
        Row(Modifier::new()
            .fill_max_width()
            .padding(10.0)
            .clip_rounded(14.0)
            .background(if is_current {
                theme().primary_container.with_alpha(60)
            } else {
                theme().surface.with_alpha(0)
            })
            .on_click({
                let player = player.clone();
                let current = current.clone();
                move || {
                    current.set(Some(idx));
                    if let Err(e) = player.load(row_source.clone()) {
                        log::error!("load track failed: {e}");
                    }
                }
            })
            .gap(12.0)
            .align_items(AlignItems::CENTER))
        .child((
            Box(Modifier::new()
                .width(40.0)
                .height(40.0)
                .clip_rounded(10.0)
                .background(leading_bg)
                .align_items(AlignItems::CENTER)
                .justify_content(JustifyContent::CENTER))
            .child(if let Some(h) = thumb {
                Image(Modifier::new().fill_max_size(), h)
            } else if is_current {
                Icon(Symbols::graphic_eq).size(18.0).color(leading_fg)
            } else if is_video_source(&entry.source) {
                Icon(Symbols::movie).size(18.0).color(leading_fg)
            } else {
                Text(format!("{}", idx + 1)).size(13.0).color(leading_fg)
            }),
            Column(Modifier::new().weight(1.0).gap(2.0)).child((
                Text(entry.display_title())
                    .size(14.0)
                    .color(if is_current {
                        theme().primary
                    } else {
                        theme().on_surface
                    })
                    .single_line()
                    .overflow_ellipsize(),
                Text(
                    entry
                        .meta
                        .artist
                        .clone()
                        .unwrap_or_else(|| "Unknown artist".into()),
                )
                .size(12.0)
                .color(theme().on_surface.with_alpha(150))
                .single_line()
                .overflow_ellipsize(),
            )),
            Text(
                entry
                    .meta
                    .duration
                    .map(format_duration)
                    .unwrap_or_else(|| "--:--".into()),
            )
            .size(12.0)
            .color(theme().on_surface.with_alpha(150)),
        )),
        m3::HorizontalDivider(m3::DividerConfig {
            modifier: Modifier::new()
                .fill_max_width()
                .padding_values(PaddingValues {
                    left: 62.0,
                    right: 0.0,
                    top: 0.0,
                    bottom: 0.0,
                }),
            ..Default::default()
        }),
    ))
}

fn progress_ratio(position: Duration, duration: Option<Duration>) -> f32 {
    match duration {
        Some(d) if d.as_secs_f32() > 0.0 => {
            (position.as_secs_f32() / d.as_secs_f32()).clamp(0.0, 1.0)
        }
        _ => 0.0,
    }
}

fn format_duration(d: Duration) -> String {
    let total = d.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{:02}:{:02}", m, s)
    } else {
        format!("{:02}:{:02}", m, s)
    }
}

fn format_position(d: Duration) -> String {
    let secs = d.as_secs();
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn relative_seek(player: &AudioPlayer, snap: &player_core::PlayerSnapshot, delta: f64) {
    let Some(dur) = snap.duration else { return };
    let pos = snap.position.as_secs_f64();
    let target = (pos + delta).clamp(0.0, dur.as_secs_f64());
    let _ = player.seek(Duration::from_secs_f64(target));
}

const OSD_MS: u64 = 1200;

const SPEED_STEPS: &[f32] = &[0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0];

fn step_speed(cur: f32, up: bool) -> f32 {
    if up {
        SPEED_STEPS
            .iter()
            .copied()
            .find(|&s| s > cur + 0.01)
            .unwrap_or(*SPEED_STEPS.last().unwrap())
    } else {
        SPEED_STEPS
            .iter()
            .copied()
            .rev()
            .find(|&s| s < cur - 0.01)
            .unwrap_or(SPEED_STEPS[0])
    }
}

#[allow(clippy::too_many_arguments)]
fn make_player_key_handler(
    player: AudioPlayer,
    snap: player_core::PlayerSnapshot,
    is_fullscreen: Rc<Signal<bool>>,
    has_video: bool,
    settings: Rc<Signal<PlayerSettings>>,
    speed: Rc<Signal<f32>>,
    volume: Rc<Signal<f32>>,
    show_settings: Rc<Signal<bool>>,
) -> impl Fn(KeyEvent) -> bool {
    move |ev: KeyEvent| -> bool {
        if ev.event_type != KeyEventType::Down {
            return false;
        }
        let cfg = settings.get();
        match &ev.key {
            Key::Escape if is_fullscreen.get() => {
                is_fullscreen.set(false);
                true
            }
            Key::Character('f') | Key::Character('F') | Key::F(11) if has_video => {
                is_fullscreen.set(!is_fullscreen.get());
                true
            }
            Key::Space | Key::Character('p') | Key::Character('P') => {
                let _ = player.toggle();
                true
            }
            Key::ArrowLeft if ev.modifiers.shift => {
                relative_seek(&player, &snap, -cfg.seek_small_s);
                true
            }
            Key::ArrowRight if ev.modifiers.shift => {
                relative_seek(&player, &snap, cfg.seek_small_s);
                true
            }
            Key::ArrowLeft => {
                let d = if ev.modifiers.ctrl {
                    cfg.seek_small_s
                } else {
                    cfg.seek_medium_s
                };
                relative_seek(&player, &snap, -d);
                true
            }
            Key::ArrowRight => {
                let d = if ev.modifiers.ctrl {
                    cfg.seek_small_s
                } else {
                    cfg.seek_medium_s
                };
                relative_seek(&player, &snap, d);
                true
            }
            Key::ArrowUp => {
                relative_seek(&player, &snap, cfg.seek_large_s);
                true
            }
            Key::ArrowDown => {
                relative_seek(&player, &snap, -cfg.seek_large_s);
                true
            }
            Key::Character('m') | Key::Character('M') => {
                let _ = player.toggle_mute();
                true
            }
            Key::Character('0') | Key::Home => {
                let _ = player.seek(Duration::ZERO);
                true
            }
            Key::Character('[') => {
                let next = step_speed(speed.get(), false);
                speed.set(next);
                let _ = player.set_speed(next);
                true
            }
            Key::Character(']') => {
                let next = step_speed(speed.get(), true);
                speed.set(next);
                let _ = player.set_speed(next);
                true
            }
            Key::Character(',') | Key::Character('<') => {
                let v = (volume.get() - cfg.volume_step).clamp(0.0, 1.5);
                volume.set(v);
                let _ = player.set_volume(v);
                true
            }
            Key::Character('.') | Key::Character('>') => {
                let v = (volume.get() + cfg.volume_step).clamp(0.0, 1.5);
                volume.set(v);
                let _ = player.set_volume(v);
                true
            }
            Key::Character('s') | Key::Character('S') if ev.modifiers.ctrl => {
                show_settings.set(true);
                true
            }
            _ => false,
        }
    }
}

fn SettingsScreen(
    settings: Rc<Signal<PlayerSettings>>,
    show: Rc<Signal<bool>>,
    player: AudioPlayer,
    speed: Rc<Signal<f32>>,
) -> View {
    let s = settings.get();
    m3::Scaffold(
        move |_| {
            Column(Modifier::new().fill_max_size().padding(16.0).gap(16.0)).child((
                Row(
                    Modifier::new()
                        .fill_max_width()
                        .align_items(AlignItems::CENTER)
                        .gap(8.0),
                )
                .child((
                    m3::IconButton(
                        Icon(Symbols::close).size(22.0),
                        {
                            let show = show.clone();
                            move || show.set(false)
                        },
                        m3::IconButtonConfig::default(),
                    ),
                    Text("Settings").size(20.0),
                )),
                Column(Modifier::new().fill_max_width().gap(10.0)).child((
                    Text("Playback").size(14.0).color(theme().primary),
                    Row(
                        Modifier::new()
                            .fill_max_width()
                            .align_items(AlignItems::CENTER),
                    )
                    .child((
                        Text("Auto full-window for video")
                            .modifier(Modifier::new().weight(1.0)),
                        m3::Switch(
                            s.auto_fullscreen_on_open,
                            {
                                let settings = settings.clone();
                                move |v| {
                                    let mut s = settings.get();
                                    s.auto_fullscreen_on_open = v;
                                    save_settings_sync(&s);
                                    settings.set(s);
                                }
                            },
                            m3::SwitchConfig::default(),
                        ),
                    )),
                    Row(
                        Modifier::new()
                            .fill_max_width()
                            .align_items(AlignItems::CENTER),
                    )
                    .child((
                        Text("Playlist thumbnails").modifier(Modifier::new().weight(1.0)),
                        m3::Switch(
                            s.show_playlist_thumbs,
                            {
                                let settings = settings.clone();
                                move |v| {
                                    let mut s = settings.get();
                                    s.show_playlist_thumbs = v;
                                    save_settings_sync(&s);
                                    settings.set(s);
                                }
                            },
                            m3::SwitchConfig::default(),
                        ),
                    )),
                    Row(
                        Modifier::new()
                            .fill_max_width()
                            .align_items(AlignItems::CENTER),
                    )
                    .child((
                        Text("Remember speed").modifier(Modifier::new().weight(1.0)),
                        m3::Switch(
                            s.remember_speed,
                            {
                                let settings = settings.clone();
                                move |v| {
                                    let mut s = settings.get();
                                    s.remember_speed = v;
                                    save_settings_sync(&s);
                                    settings.set(s);
                                }
                            },
                            m3::SwitchConfig::default(),
                        ),
                    )),
                )),
                Column(Modifier::new().fill_max_width().gap(6.0)).child((
                    Text("Controls").size(14.0).color(theme().primary),
                    Text(format!("Controls hide after: {} ms", s.hide_controls_ms)).size(13.0),
                    m3::Slider(
                        s.hide_controls_ms as f32,
                        (800.0, 8000.0),
                        None,
                        {
                            let settings = settings.clone();
                            move |v| {
                                let mut s = settings.get();
                                s.hide_controls_ms = v as u64;
                                settings.set(s);
                            }
                        },
                        m3::SliderConfig {
                            on_value_change_finished: Some(Rc::new({
                                let settings = settings.clone();
                                move || save_settings_sync(&settings.get())
                            })),
                            modifier: Modifier::new().fill_max_width(),
                            ..Default::default()
                        },
                    ),
                    Text(format!("Seek (arrows), seconds: {:.0}", s.seek_medium_s)).size(13.0),
                    m3::Slider(
                        s.seek_medium_s as f32,
                        (1.0, 30.0),
                        None,
                        {
                            let settings = settings.clone();
                            move |v| {
                                let mut s = settings.get();
                                s.seek_medium_s = v as f64;
                                settings.set(s);
                            }
                        },
                        m3::SliderConfig {
                            on_value_change_finished: Some(Rc::new({
                                let settings = settings.clone();
                                move || save_settings_sync(&settings.get())
                            })),
                            modifier: Modifier::new().fill_max_width(),
                            ..Default::default()
                        },
                    ),
                    Text(format!("Seek (up/down), seconds: {:.0}", s.seek_large_s)).size(13.0),
                    m3::Slider(
                        s.seek_large_s as f32,
                        (10.0, 180.0),
                        None,
                        {
                            let settings = settings.clone();
                            move |v| {
                                let mut s = settings.get();
                                s.seek_large_s = v as f64;
                                settings.set(s);
                            }
                        },
                        m3::SliderConfig {
                            on_value_change_finished: Some(Rc::new({
                                let settings = settings.clone();
                                move || save_settings_sync(&settings.get())
                            })),
                            modifier: Modifier::new().fill_max_width(),
                            ..Default::default()
                        },
                    ),
                )),
                Column(Modifier::new().fill_max_width().gap(8.0)).child((
                    Text(format!("Default speed: {:.2}x", s.default_speed)).size(13.0),
                    Row(Modifier::new().gap(8.0)).child(
                        SPEED_STEPS
                            .iter()
                            .map(|&step| {
                                let selected = (s.default_speed - step).abs() < 0.01;
                                let settings = settings.clone();
                                let player = player.clone();
                                let speed = speed.clone();
                                m3::FilterChip(
                                    selected,
                                    move || {
                                        let mut s = settings.get();
                                        s.default_speed = step;
                                        save_settings_sync(&s);
                                        settings.set(s);
                                        let _ = player.set_speed(step);
                                        speed.set(step);
                                    },
                                    Text(format!("{step:.2}x")).size(12.0),
                                    None,
                                    None,
                                    m3::ChipConfig::default(),
                                )
                            })
                            .collect::<Vec<_>>(),
                    ),
                )),
                Column(Modifier::new().fill_max_width().gap(8.0)).child((
                    Text("Hotkeys").size(14.0).color(theme().primary),
                    Text(
                        "Space/P play·pause · F fullscreen · M mute · [/] speed · ←/→ seek · ↑/↓ ±1m · Ctrl+S settings · ,/. volume",
                    )
                    .size(12.0)
                    .color(theme().on_surface.with_alpha(160)),
                )),
            ))
        },
        m3::ScaffoldConfig::default(),
    )
}
