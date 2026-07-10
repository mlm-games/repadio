//! player-platform: platform glue (file picking + WASM OPFS persistence).

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum PickedFile {
    /// Real filesystem path (desktop).
    Path(PathBuf),
    /// File name + raw bytes (WASM / Android). Could maybe add a Uri type when Android adds sharing.
    Bytes { name: String, data: Vec<u8> },
}

/// Initialise platform backends. Must be called once at app startup.
pub fn init() {
    rlobkit_dialogs::init();
}

pub const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "wav", "ogg", "oga", "m4a", "aac", "mp4", "mka", "webm", "aiff", "caf",
];

/// Open a native multi-file picker filtered to audio formats.
/// Returns an empty Vec if the user cancels.
///
/// NOTE: blocks the calling thread on desktop. On WASM the synchronous
/// API is unavailable -> use `pick_audio_files_async` instead.
pub fn pick_audio_files() -> Vec<PickedFile> {
    #[cfg(not(any(target_os = "android", target_arch = "wasm32")))]
    {
        rlobkit_dialogs::blocking_pick_files("Add audio files", AUDIO_EXTENSIONS)
            .into_iter()
            .map(PickedFile::Path)
            .collect()
    }
    #[cfg(any(target_os = "android", target_arch = "wasm32"))]
    {
        log::warn!("blocking picker unavailable on this platform; use async variant");
        Vec::new()
    }
}

/// Non-blocking file picker. On WASM the dialog is launched via the
/// browser's native file picker; results arrive on `on_done` when the
/// user finishes selecting.
///
/// On desktop this spawns a helper thread (same as before).
pub fn pick_audio_files_async(on_done: impl FnOnce(Vec<PickedFile>) + Send + 'static) {
    #[cfg(target_os = "android")]
    {
        std::thread::spawn(move || {
            use rlobkit_dialogs::picker::OpenFileOptions;
            use rlobkit_dialogs::{RlobKit, RlobKitMode, RlobKitType};

            let exts: Vec<String> = AUDIO_EXTENSIONS.iter().map(|s| s.to_string()).collect();
            let result =
                futures_lite::future::block_on(RlobKit::open_file_picker(OpenFileOptions {
                    file_type: RlobKitType::Custom {
                        extensions: exts,
                        mime_types: vec!["audio/*".to_string()],
                    },
                    mode: RlobKitMode::Multiple { limit: None },
                    title: Some("Add audio files".to_string()),
                    initial_directory: None,
                }));

            let files = match result {
                Ok(Some(platform_files)) => platform_files
                    .into_iter()
                    .filter_map(|file| {
                        let name = file.name().to_string();

                        match file.read_bytes() {
                            Ok(data) => Some(PickedFile::Bytes {
                                name,
                                data: data.to_vec(),
                            }),
                            Err(err) => {
                                log::error!(
                                    "failed to read Android picker file {name:?}: {err}"
                                );
                                None
                            }
                        }
                    })
                    .collect(),

                Ok(None) => {
                    log::info!("Android audio picker cancelled");
                    Vec::new()
                }

                Err(err) => {
                    log::error!("Android audio picker failed: {err}");
                    Vec::new()
                }
            };
            on_done(files);
        });
    }

    #[cfg(not(any(target_os = "android", target_arch = "wasm32")))]
    {
        std::thread::spawn(move || {
            let files = pick_audio_files();
            on_done(files);
        });
    }

    #[cfg(target_arch = "wasm32")]
    {
        let exts: Vec<String> = AUDIO_EXTENSIONS.iter().map(|s| s.to_string()).collect();
        wasm_bindgen_futures::spawn_local(async move {
            use rlobkit_dialogs::picker::OpenFileOptions;
            use rlobkit_dialogs::{RlobKit, RlobKitMode, RlobKitType};

            let result = RlobKit::open_file_picker(OpenFileOptions {
                file_type: RlobKitType::Custom {
                    extensions: exts,
                    mime_types: vec!["audio/*".to_string()],
                },
                mode: RlobKitMode::Multiple { limit: None },
                title: Some("Add audio files".to_string()),
                initial_directory: None,
            })
            .await;

            let files = match result {
                Ok(Some(platform_files)) => platform_files
                    .into_iter()
                    .filter_map(|f| {
                        let name = f.name().to_string();
                        // On WASM the bytes are already in memory.
                        match f.data() {
                            Some(data) => Some(PickedFile::Bytes {
                                name,
                                data: data.to_vec(),
                            }),
                            None => f.read_bytes().ok().map(|data| PickedFile::Bytes {
                                name,
                                data: data.to_vec(),
                            }),
                        }
                    })
                    .collect(),
                _ => Vec::new(),
            };
            on_done(files);
        });
    }
}

/// OPFS-backed persistent key-value store for WASM.
/// No-op on desktop/Android.
pub mod wasm_persist {
    /// Initialise OPFS directories. Must be called once at startup on WASM.
    /// Safe to call on all platforms (no-op outside WASM).
    pub async fn init() -> Result<(), String> {
        #[cfg(target_arch = "wasm32")]
        {
            let dirs = ["config", "cache", "library"];
            for dir in &dirs {
                opfs::ensure_dir(dir)
                    .await
                    .map_err(|e| format!("OPFS init: {e}"))?;
            }
            log::info!("OPFS initialised");
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = ();
        Ok(())
    }

    /// Read a string from OPFS. Returns `None` if the key doesn't exist.
    /// No-op outside WASM.
    pub async fn read(key: &str) -> Option<String> {
        #[cfg(target_arch = "wasm32")]
        {
            opfs::read(key)
                .await
                .ok()
                .and_then(|d| String::from_utf8(d.to_vec()).ok())
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = key;
            None
        }
    }

    /// Write a string to OPFS.
    pub async fn write(key: &str, data: &str) -> Result<(), String> {
        #[cfg(target_arch = "wasm32")]
        {
            opfs::write(key, data.as_bytes())
                .await
                .map_err(|e| e.to_string())
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (key, data);
            Ok(())
        }
    }
}
