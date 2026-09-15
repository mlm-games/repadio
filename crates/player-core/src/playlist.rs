//! Playlist file support (M3U/M3U8/PLS).
//!
//! Playlist files are expanded into their entries before probing: each entry
//! resolves to a [`MediaSource`] plus an optional title from the playlist
//! (EXTINF titles / PLS Title fields win over probed media tags, mpv-style).
//! Entries pointing at missing files are kept — the error surfaces when the
//! entry is played, like mpv.

use std::path::{Path, PathBuf};

use super::MediaSource;

const PLAYLIST_EXTENSIONS: &[&str] = &["m3u", "m3u8", "pls"];

pub fn is_playlist_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            PLAYLIST_EXTENSIONS
                .iter()
                .any(|v| e.eq_ignore_ascii_case(v))
        })
        .unwrap_or(false)
}

pub fn is_playlist_source(src: &MediaSource) -> bool {
    match src {
        MediaSource::Path(p) => p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                PLAYLIST_EXTENSIONS
                    .iter()
                    .any(|v| e.eq_ignore_ascii_case(v))
            })
            .unwrap_or(false),
        MediaSource::Bytes { name, .. } => is_playlist_name(name),
    }
}

/// Expand a playlist source into `(entry source, playlist title)` pairs.
/// Non-playlist sources pass through unchanged with no title.
pub fn expand_playlist(src: &MediaSource) -> Vec<(MediaSource, Option<String>)> {
    if !is_playlist_source(src) {
        return vec![(src.clone(), None)];
    }
    match read_playlist_text(src) {
        Ok(text) => parse_playlist(src, &text),
        Err(e) => {
            log::warn!("playlist {}: {e:#}", src.display_name());
            vec![(src.clone(), None)]
        }
    }
}

fn read_playlist_text(src: &MediaSource) -> anyhow::Result<String> {
    match src {
        MediaSource::Path(p) => Ok(std::fs::read_to_string(p)?),
        MediaSource::Bytes { bytes, .. } => Ok(String::from_utf8(bytes.to_vec())?),
    }
}

fn playlist_base_dir(src: &MediaSource) -> Option<PathBuf> {
    match src {
        MediaSource::Path(p) => p.parent().map(|d| d.to_path_buf()),
        MediaSource::Bytes { .. } => None,
    }
}

fn parse_playlist(src: &MediaSource, text: &str) -> Vec<(MediaSource, Option<String>)> {
    let ext = match src {
        MediaSource::Path(p) => p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase(),
        MediaSource::Bytes { name, .. } => Path::new(name)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase(),
    };
    if ext == "pls" {
        parse_pls(src, text)
    } else {
        parse_m3u(src, text)
    }
}

fn resolve_entry(src: &MediaSource, raw: &str) -> Option<MediaSource> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        // Remote entries: not downloadable yet, keep as path so the
        // player reports a clear error instead of silently dropping.
        return Some(MediaSource::Path(PathBuf::from(raw)));
    }
    let base = playlist_base_dir(src);
    let candidate = Path::new(raw);
    if candidate.is_absolute() {
        return Some(MediaSource::Path(candidate.to_path_buf()));
    }
    // Entries live relative to the playlist file when on disk, else
    // relative to the current directory.
    match base {
        Some(dir) => Some(MediaSource::Path(dir.join(candidate))),
        None => Some(MediaSource::Path(candidate.to_path_buf())),
    }
}

fn parse_m3u(src: &MediaSource, text: &str) -> Vec<(MediaSource, Option<String>)> {
    let mut out = Vec::new();
    let mut pending_title: Option<String> = None;
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let title = rest.splitn(2, ',').nth(1).map(|t| t.trim().to_string());
            pending_title = title.filter(|t| !t.is_empty());
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some(entry) = resolve_entry(src, line) {
            out.push((entry, pending_title.take()));
        }
    }
    out
}

fn parse_pls(src: &MediaSource, text: &str) -> Vec<(MediaSource, Option<String>)> {
    match pls::parse(&mut text.as_bytes()) {
        Ok(elements) => elements
            .into_iter()
            .filter_map(|el| resolve_entry(src, &el.path).map(|s| (s, el.title)))
            .collect(),
        Err(e) => {
            log::warn!("playlist {}: PLS parse failed: {e}", src.display_name());
            vec![(src.clone(), None)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m3u_extinf_titles() {
        let src = MediaSource::Path(PathBuf::from("/music/list.m3u"));
        let text = "#EXTM3U\n#EXTINF:123,Artist - Title\nsong.mp3\nplain.ogg\n";
        let out = parse_playlist(&src, text);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].1.as_deref(), Some("Artist - Title"));
        assert_eq!(
            out[0].0,
            MediaSource::Path(PathBuf::from("/music/song.mp3"))
        );
        assert_eq!(out[1].1, None);
    }

    #[test]
    fn m3u_absolute_and_url_kept() {
        let src = MediaSource::Path(PathBuf::from("/music/list.m3u"));
        let text = "/abs/a.flac\nhttps://example.com/b.mp3\n";
        let out = parse_playlist(&src, text);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn pls_titles() {
        let src = MediaSource::Path(PathBuf::from("/music/list.pls"));
        let text = "[playlist]\nFile1=a.mp3\nTitle1=First\nNumberOfEntries=1\n";
        let out = parse_playlist(&src, text);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.as_deref(), Some("First"));
    }

    #[test]
    fn non_playlist_passes_through() {
        let src = MediaSource::Path(PathBuf::from("/music/a.mp3"));
        let out = expand_playlist(&src);
        assert_eq!(out, vec![(src, None)]);
    }
}
