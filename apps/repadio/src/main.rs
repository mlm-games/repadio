#![allow(non_snake_case, non_upper_case_globals)]

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use player_core::{AudioPlayer, PlaybackState, TrackMeta, probe_track_meta};
use repose_core::prelude::*;
use repose_material::material3 as m3;
use repose_material::{Icon, material_symbols};
use repose_ui::TextStyle;

material_symbols! {
    add            : '\u{E145}',
    image          : '\u{E3F4}',
    music_note     : '\u{E405}',
    pause          : '\u{E034}',
    play_arrow     : '\u{E037}',
    skip_next      : '\u{E044}',
    skip_previous  : '\u{E045}',
    stop           : '\u{E047}',
}
use repose_ui::{Box, Column, LazyColumn, LazyColumnConfig, Row, Spacer, Text, ViewExt};

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    meta: TrackMeta,
}

impl Entry {
    fn display_title(&self) -> String {
        self.meta.title.clone().unwrap_or_else(|| {
            self.path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Unknown".into())
        })
    }
}

/// Files picked on the dialog helper thread land here; the UI drains
/// this on each frame.
type PendingFiles = Arc<Mutex<Vec<PathBuf>>>;

#[cfg(not(target_arch = "wasm32"))]
fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    player_platform::init();

    let player = AudioPlayer::spawn()?;
    let pending: PendingFiles = Arc::new(Mutex::new(
        std::env::args().skip(1).map(PathBuf::from).collect(),
    ));

    let app_player = player.clone();
    let app_pending = pending.clone();

    repose_platform::run_desktop_app(move |_sched, _ctx| {
        App(app_player.clone(), app_pending.clone())
    })
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub async fn wasm_main() {
    console_error_panic_hook::set_once();
    console_log::init_with_level(log::Level::Info).ok();

    // Initialise OPFS for persistent storage (playlist, config).
    if let Err(e) = player_platform::wasm_persist::init().await {
        log::error!("OPFS init failed: {e}");
    }

    player_platform::init();

    let player = AudioPlayer::spawn().expect("failed to spawn audio player");
    let pending: PendingFiles = Arc::new(Mutex::new(Vec::new()));

    // Resume audio context on first user gesture.
    let resume_player = player.clone();
    let canvas = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("repadio_canvas"))
        .or_else(|| {
            // Fallback: attach to body if canvas not found
            web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.body())
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

    repose_platform::run_desktop_app(move |_sched, _ctx| App(player.clone(), pending.clone()))
        .expect("app run failed");
}

#[cfg(target_arch = "wasm32")]
fn main() {}

fn App(player: AudioPlayer, pending: PendingFiles) -> View {
    request_frame();

    let playlist = remember(|| signal(Vec::<Entry>::new()));
    let current = remember(|| signal(None::<usize>));
    let volume = remember(|| signal(1.0f32));
    // Guards auto-advance so `Ended` triggers exactly one load.
    let advance_armed = remember(|| signal(true));
    // Seek-in-progress ratio (None = not dragging).
    let scrubbing = remember(|| signal(None::<f32>));

    {
        let new_files: Vec<PathBuf> = pending.lock().unwrap().drain(..).collect();
        if !new_files.is_empty() {
            let mut list = playlist.get();
            let was_empty = list.is_empty();
            for path in new_files {
                let meta = probe_track_meta(&path);
                list.push(Entry { path, meta });
            }
            playlist.set(list.clone());
            if was_empty && !list.is_empty() {
                current.set(Some(0));
                let _ = player.load(list[0].path.clone());
                advance_armed.set(true);
            }
        }
    }

    let snap = player.snapshot();

    if snap.state == PlaybackState::Ended && advance_armed.get() {
        let list = playlist.get();
        if let Some(idx) = current.get() {
            if idx + 1 < list.len() {
                advance_armed.set(false);
                current.set(Some(idx + 1));
                let _ = player.load(list[idx + 1].path.clone());
            }
        }
    }
    if matches!(snap.state, PlaybackState::Playing | PlaybackState::Loading) {
        advance_armed.set(true);
    }

    let status_text = match snap.state {
        PlaybackState::Idle => "Idle",
        PlaybackState::Loading => "Loading",
        PlaybackState::Playing => "Playing",
        PlaybackState::Paused => "Paused",
        PlaybackState::Stopped => "Stopped",
        PlaybackState::Ended => "Ended",
        PlaybackState::Error => "Error",
    };

    let title = snap
        .title
        .clone()
        .unwrap_or_else(|| "No track loaded".into());
    let sub_line = match (&snap.artist, &snap.album) {
        (Some(ar), Some(al)) => format!("{ar} — {al}"),
        (Some(ar), None) => ar.clone(),
        (None, Some(al)) => al.clone(),
        (None, None) => String::new(),
    };

    let pos = format_duration(snap.position);
    let dur = snap
        .duration
        .map(format_duration)
        .unwrap_or_else(|| "--:--".into());

    // While the user drags the slider, show the drag position, not the clock.
    let slider_value = scrubbing
        .get()
        .unwrap_or_else(|| progress_ratio(snap.position, snap.duration));

    let top_bar = m3::TopAppBar(
        Text("Repadio"),
        Some(Text("Symphonia + CPAL — pure Rust")),
        None,
        vec![],
        m3::TopAppBarConfig::default(),
    );

    m3::Scaffold(
        move |padding| {
            Column(
                Modifier::new()
                    .fill_max_size()
                    .padding_values(padding)
                    .padding(20.0)
                    .gap(14.0),
            )
            .child((
                Row(Modifier::new().fill_max_width().gap(16.0)).child((
                    if snap.art.is_some() {
                        // TODO: load art via RenderContext::set_image_encoded
                        Box(Modifier::new()
                            .width(96.0)
                            .height(96.0)
                            .clip_rounded(12.0)
                            .background(theme().surface_variant))
                        .child(
                            Icon(Symbols::image)
                                .size(40.0)
                                .color(theme().on_surface.with_alpha(120)),
                        )
                    } else {
                        // Placeholder square when no embedded art.
                        Box(Modifier::new()
                            .width(96.0)
                            .height(96.0)
                            .clip_rounded(12.0)
                            .background(theme().surface_variant))
                        .child(
                            Icon(Symbols::music_note)
                                .size(40.0)
                                .color(theme().on_surface.with_alpha(120)),
                        )
                    },
                    Column(Modifier::new().weight(1.0).gap(4.0)).child((
                        Text(title.clone())
                            .size(24.0)
                            .single_line()
                            .overflow_ellipsize(),
                        Text(sub_line.clone())
                            .size(14.0)
                            .color(theme().on_surface.with_alpha(170))
                            .single_line()
                            .overflow_ellipsize(),
                        Text(format!("Status: {status_text}"))
                            .size(13.0)
                            .color(match snap.state {
                                PlaybackState::Error => theme().error,
                                PlaybackState::Playing => theme().primary,
                                _ => theme().on_surface.with_alpha(150),
                            }),
                    )),
                )),
                if let Some(err) = &snap.error {
                    Text(err.clone()).size(13.0).color(theme().error)
                } else {
                    Box(Modifier::new())
                },
                Row(Modifier::new().fill_max_width().gap(8.0)).child((
                    Text(pos.clone()).size(13.0),
                    Spacer(),
                    Text(dur.clone()).size(13.0),
                )),
                // ENABLED seek slider: track drag locally, commit on release.
                m3::Slider(
                    slider_value,
                    (0.0, 1.0),
                    None,
                    // on_change (while dragging) → preview only
                    {
                        let scrubbing = scrubbing.clone();
                        move |ratio: f32| scrubbing.set(Some(ratio))
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
                                }
                            }))
                        },
                        ..Default::default()
                    },
                ),
                Row(Modifier::new().fill_max_width().gap(12.0)).child((
                    m3::OutlinedButton(
                        Modifier::new(),
                        {
                            let player = player.clone();
                            let playlist = playlist.clone();
                            let current = current.clone();
                            move || {
                                let list = playlist.get();
                                if let Some(idx) = current.get() {
                                    if idx > 0 {
                                        current.set(Some(idx - 1));
                                        let _ = player.load(list[idx - 1].path.clone());
                                    }
                                }
                            }
                        },
                        m3::ButtonConfig::default(),
                        || Row(Modifier::new().gap(8.0)).child((
                            Icon(Symbols::skip_previous),
                            Text("Prev"),
                        )),
                    ),
                    m3::Button(
                        Modifier::new(),
                        {
                            let player = player.clone();
                            move || {
                                if let Err(e) = player.toggle() {
                                    log::error!("toggle failed: {e}");
                                }
                            }
                        },
                        m3::ButtonConfig::default(),
                        move || {
                            let (icon, label) = if snap.state == PlaybackState::Playing {
                                (Icon(Symbols::pause), Text("Pause"))
                            } else {
                                (Icon(Symbols::play_arrow), Text("Play"))
                            };
                            Row(Modifier::new().gap(8.0)).child((icon, label))
                        },
                    ),
                    m3::OutlinedButton(
                        Modifier::new(),
                        {
                            let player = player.clone();
                            let playlist = playlist.clone();
                            let current = current.clone();
                            move || {
                                let list = playlist.get();
                                if let Some(idx) = current.get() {
                                    if idx + 1 < list.len() {
                                        current.set(Some(idx + 1));
                                        let _ = player.load(list[idx + 1].path.clone());
                                    }
                                }
                            }
                        },
                        m3::ButtonConfig::default(),
                        || Row(Modifier::new().gap(8.0)).child((
                            Text("Next"),
                            Icon(Symbols::skip_next),
                        )),
                    ),
                    m3::OutlinedButton(
                        Modifier::new(),
                        {
                            let player = player.clone();
                            move || {
                                if let Err(e) = player.stop() {
                                    log::error!("stop failed: {e}");
                                }
                            }
                        },
                        m3::ButtonConfig::default(),
                        || Row(Modifier::new().gap(8.0)).child((
                            Icon(Symbols::stop),
                            Text("Stop"),
                        )),
                    ),
                    Spacer(),
                    m3::FilledTonalButton(
                        Modifier::new(),
                        {
                            let pending = pending.clone();
                            move || {
                                let pending = pending.clone();
                                player_platform::pick_audio_files_async(move |files| {
                                    if !files.is_empty() {
                                        pending.lock().unwrap().extend(files);
                                    }
                                });
                            }
                        },
                        m3::ButtonConfig::default(),
                        || Row(Modifier::new().gap(8.0)).child((
                            Icon(Symbols::add),
                            Text("Add files"),
                        )),
                    ),
                )),
                Row(Modifier::new().fill_max_width().gap(12.0)).child((
                    Text(format!("Vol {:.0}%", volume.get() * 100.0)).size(13.0),
                    m3::Slider(
                        volume.get(),
                        (0.0, 1.5),
                        None,
                        {
                            let volume = volume.clone();
                            let player = player.clone();
                            move |v| {
                                volume.set(v);
                                let _ = player.set_volume(v);
                            }
                        },
                        m3::SliderConfig {
                            modifier: Modifier::new().weight(1.0),
                            ..Default::default()
                        },
                    ),
                )),
                Text(format!("Playlist — {} tracks", playlist.get().len()))
                    .size(15.0)
                    .color(theme().on_surface.with_alpha(200)),
                {
                    let list = playlist.get();
                    LazyColumn(
                        list,
                        64.0f32,
                        |entry: &Entry| {
                            // Use a hash of the path as a stable key.
                            use std::hash::{Hash, Hasher};
                            let mut s = std::collections::hash_map::DefaultHasher::new();
                            entry.path.hash(&mut s);
                            s.finish()
                        },
                        {
                            let current = current.clone();
                            let player = player.clone();
                            move |entry: Entry, idx: usize| {
                                let is_current = current.get() == Some(idx);
                                let row_player = player.clone();
                                let row_current = current.clone();
                                let row_path = entry.path.clone();

                                Row(Modifier::new()
                                    .fill_max_width()
                                    .padding(10.0)
                                    .clip_rounded(8.0)
                                    .background(if is_current {
                                        theme().surface_variant
                                    } else {
                                        theme().surface
                                    })
                                    .on_click(move || {
                                        row_current.set(Some(idx));
                                        let _ = row_player.load(row_path.clone());
                                    })
                                    .gap(10.0))
                                .child((
                                    Text(format!("{}.", idx + 1))
                                        .size(13.0)
                                        .color(theme().on_surface.with_alpha(140)),
                                    Column(Modifier::new().weight(1.0)).child((
                                        Text(entry.display_title())
                                            .size(14.0)
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
                                ))
                            }
                        },
                        LazyColumnConfig {
                            modifier: Modifier::new().fill_max_width().weight(1.0),
                            ..Default::default()
                        },
                    )
                },
            ))
        },
        m3::ScaffoldConfig {
            top_bar: Some(top_bar),
            ..Default::default()
        },
    )
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
    format!("{:02}:{:02}", total / 60, total % 60)
}
