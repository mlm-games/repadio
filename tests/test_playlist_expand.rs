//! Playlist expansion against real media files (headless).
//! Verifies: EXTINF titles win, relative entries resolve against the
//! playlist dir, missing entries are kept.

use std::path::PathBuf;

use player_core::{MediaSource, expand_playlist, probe_media_source};

fn write_playlist(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

#[test]
fn playlist_expands_with_titles_and_missing_kept() {
    let dir = std::env::temp_dir().join("repadio-pl-e2e");
    std::fs::create_dir_all(&dir).unwrap();

    let real: Vec<PathBuf> = ["/home/ymsr/Music/peaceful-ringtone.mp3"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();
    assert!(!real.is_empty(), "need a real mp3 on disk for this test");

    let body = format!(
        "#EXTM3U\n#EXTINF:-1,Playlist Given Title\n{}\n/does/not/exist.mp3\n",
        real[0].display()
    );
    let pl = write_playlist(&dir, "list.m3u", &body);

    let out = expand_playlist(&MediaSource::Path(pl));
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].1.as_deref(), Some("Playlist Given Title"));
    assert_eq!(out[1].1, None);
    assert!(matches!(&out[1].0, MediaSource::Path(p) if p.to_str() == Some("/does/not/exist.mp3")));

    // Real entry probes to valid metadata (title overridden later by playlist).
    let meta = probe_media_source(out[0].0.clone());
    assert!(
        meta.duration.is_some(),
        "expected duration from probing {:?}",
        out[0].0
    );
}

#[test]
fn pls_expands_with_titles() {
    let dir = std::env::temp_dir().join("repadio-pl-e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let body = "[playlist]\nFile1=/nowhere/a.mp3\nTitle1=Pls Title\nNumberOfEntries=1\n";
    let pl = write_playlist(&dir, "list.pls", body);
    let out = expand_playlist(&MediaSource::Path(pl));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].1.as_deref(), Some("Pls Title"));
}

#[test]
fn m3u8_relative_resolves_against_playlist_dir() {
    let dir = std::env::temp_dir().join("repadio-pl-e2e-sub");
    std::fs::create_dir_all(&dir).unwrap();
    let pl = write_playlist(&dir, "list.m3u8", "sub/song.ogg\n");
    let out = expand_playlist(&MediaSource::Path(pl));
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, MediaSource::Path(dir.join("sub/song.ogg")));
}
