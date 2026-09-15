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

fn write_sine_wav(dir: &std::path::Path, name: &str) -> PathBuf {
    // 1s 440Hz sine, 16-bit mono 44100Hz. Enough for symphonia's
    // riff reader + pcm decoder to probe a real duration.
    let rate: u32 = 44100;
    let samples: Vec<i16> = (0..rate)
        .map(|i| {
            (f32::sin(i as f32 * 440.0 * std::f32::consts::TAU / rate as f32) * 30000.0) as i16
        })
        .collect();
    let data_len = samples.len() * 2;
    let mut wav = Vec::with_capacity(44 + data_len);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_len as u32).to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    let p = dir.join(name);
    std::fs::write(&p, wav).unwrap();
    p
}

#[test]
fn playlist_expands_with_titles_and_missing_kept() {
    let dir = std::env::temp_dir().join("repadio-pl-e2e");
    std::fs::create_dir_all(&dir).unwrap();

    let real = write_sine_wav(&dir, "tone.wav");

    let body = format!(
        "#EXTM3U\n#EXTINF:-1,Playlist Given Title\n{}\n/does/not/exist.mp3\n",
        real.display()
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
