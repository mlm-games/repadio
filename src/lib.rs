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
use repose_material::material3 as m3;
use repose_material::{Icon, material_symbols};
use repose_platform::render::RenderContext;
use repose_ui::TextStyle;

#[cfg(target_arch = "wasm32")]
use web_thread as thread;

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
}

type PendingFiles = Arc<PendingState>;

// ---------------------------------------------------------------------------
// Video sink: receives decoded frames from the audio thread, applies A/V sync,
// uploads to the GPU via ping-pong NV12 textures.
// ---------------------------------------------------------------------------

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

        // Drain available frames into buffer, filtering by load_serial
        while let Ok(frame) = self.rx.try_recv() {
            if frame.load_serial == current_load_serial {
                self.aspect = frame.width as f32 / frame.height.max(1) as f32;
                self.buffered.push(frame);
            }
        }

        // Sort by PTS for proper display order (H.264 B-frames etc.)
        self.buffered.sort_by_key(|f| f.pts);

        if self.buffered.is_empty() {
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
            let action = if gap > Duration::from_millis(50) {
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
                    self.buffered.remove(0);
                    continue;
                }
                // Wait for the next poll unless we're >1 frame behind the audio
                // clock (catch-up).  This prevents presenting frames in the
                // future during pause when the audio clock has stopped.
                player_sync::FrameAction::WaitFor(_) if !catch_up => {
                    break;
                }
                player_sync::FrameAction::WaitFor(_) | player_sync::FrameAction::PresentNow => {
                    // Wall-clock pacing: skip if not enough time has elapsed
                    // since the last presented frame (smooths over audio clock
                    // granularity and prevents frame-doubling).
                    if !wall_ok {
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
    let pending: PendingFiles = Arc::new(PendingState {
        files: Mutex::new(
            std::env::args()
                .skip(1)
                .map(|p| MediaSource::Path(std::path::PathBuf::from(p)))
                .collect(),
        ),
        probed_meta: Mutex::new(Vec::new()),
        needs_wake: AtomicBool::new(false),
        next_id: AtomicU64::new(0),
    });

    repose_platform::run_desktop_app(move |_sched, ctx| {
        video_sink.borrow_mut().poll(ctx);
        App(player.clone(), pending.clone(), &video_sink)
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
                App(player.clone(), pending.clone(), &vs)
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
    if let Some(ref dir) = data_dir {
        if let Some(src) = intent_to_media_source(dir) {
            log::info!("loaded pending intent file");
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
    });

    {
        let vs = video_sink.clone();
        if let Err(err) =
            repose_platform::android::run_android_app(android_app, move |_sched, ctx| {
                // Poll for onNewIntent imports while the app is already running.
                if let Some(ref dir) = data_dir {
                    if let Some(src) = intent_to_media_source(dir) {
                        log::info!("loaded late pending intent");
                        pending.files.lock().unwrap().push(src);
                        request_frame();
                    }
                }
                vs.borrow_mut().poll(ctx);
                App(player.clone(), pending.clone(), &vs)
            })
        {
            log::error!("Repadio failed: {err:?}");
        }
    }
}

fn App(player: AudioPlayer, pending: PendingFiles, video_sink: &Rc<RefCell<VideoSink>>) -> View {
    let playlist = remember(|| signal(Vec::<Entry>::new()));
    let current = remember(|| signal(None::<usize>));
    let volume = remember(|| signal(1.0f32));
    let advance_armed = remember(|| signal(true));
    let pending_advance = remember(|| signal(false));
    let ended_index = remember(|| signal(None::<usize>));
    let scrubbing = remember(|| signal(None::<f32>));
    let dismissed_error = remember(|| signal(None::<String>));
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
                if let Err(e) = player.load(list[0].source.clone()) {
                    log::error!("load first track failed: {e}");
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

    if matches!(
        snap.state,
        PlaybackState::Playing | PlaybackState::Loading | PlaybackState::Buffering
    ) || scrubbing.get().is_some()
    {
        request_frame();
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
        vec![],
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
                NowPlayingCard(player.clone(), snap.clone(), scrubbing.clone(), video_sink),
                TransportBar(
                    player.clone(),
                    playlist.clone(),
                    current.clone(),
                    snap.state,
                ),
                VolumeRow(player.clone(), volume.clone()),
                PlaylistHeader(playlist_len),
                if playlist_len == 0 {
                    EmptyPlaylist(pending.clone())
                } else {
                    PlaylistList(playlist.clone(), current.clone(), player.clone())
                },
            ))
        },
        m3::ScaffoldConfig {
            top_bar: Some(top_bar),
            floating_action_button: Some(fab),
            ..Default::default()
        },
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
                .child(ZStack(Modifier::new().fill_max_size()).child((
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
                ))),
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
                .child(
                    Icon(if snap.art.is_some() {
                        Symbols::image
                    } else {
                        Symbols::music_note
                    })
                    .size(36.0)
                    .color(art_fg),
                ),
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
    ))
}

fn VolumeRow(player: AudioPlayer, volume: Rc<Signal<f32>>) -> View {
    let v = volume.get();
    let icon = if v <= 0.001 {
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
        Icon(icon)
            .size(18.0)
            .color(theme().on_surface.with_alpha(170)),
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
            Text("Tap + to add audio files")
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
            move |entry: Entry, idx: usize| TrackRow(entry, idx, current.clone(), player.clone())
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
            .child(if is_current {
                Icon(Symbols::graphic_eq).size(18.0).color(leading_fg)
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
