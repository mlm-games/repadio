//! A decoded frame must always carry a real picture.
//!
//! Two things can put a blank on screen: a decoder handing back a surface it
//! never rendered into, and the preroll gate resuming before the decoder has
//! caught up to the seek target. Both surface as an emitted frame whose luma
//! plane holds no picture, and neither is observable from frame counts alone.
//!
//! Black is *not* on its own a defect — a clip may legitimately end in black,
//! and godot's final 72 frames genuinely are. So the invariant is not "never
//! black" but "never a frame the source does not contain": every frame must
//! be structurally complete, and the number of black frames emitted may never
//! exceed the number the source actually contains. A fabricated blank pushes
//! that count past the source's; a legitimately black clip does not.

use std::time::{Duration, Instant};

use player_core::video::DecodedVideoFrame;
use player_core::{AudioPlayer, PlaybackState};

struct Expect {
    path: &'static str,
    width: u32,
    height: u32,
    /// Frames the source contains that are entirely black. Verified against
    /// `ffmpeg -pix_fmt yuv420p -f rawvideo`.
    source_black: usize,
}

const FIXTURES: &[Expect] = &[
    Expect {
        path: "/home/ymsr/Videos/godot_intro_animation_hd.mp4",
        width: 1920,
        height: 1080,
        source_black: 72,
    },
    Expect {
        path: "/home/ymsr/Videos/hid.mp4",
        width: 1920,
        height: 1080,
        source_black: 35,
    },
    Expect {
        path: "/home/ymsr/Videos/github-duck-vid.mp4",
        width: 1280,
        height: 720,
        source_black: 3,
    },
    Expect {
        path: "/home/ymsr/Videos/Big_Buck_Bunny_720_10s_1MB-h265.mp4",
        width: 1280,
        height: 720,
        source_black: 0,
    },
];

fn is_black(f: &DecodedVideoFrame) -> bool {
    f.y_plane.iter().all(|&v| v <= 16)
}

fn wait_playing(p: &AudioPlayer, budget: Duration) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < budget {
        if p.snapshot().state == PlaybackState::Playing {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

fn play_through(expect: &Expect) -> (usize, usize) {
    let p = AudioPlayer::spawn().unwrap();
    p.load(std::path::PathBuf::from(expect.path)).unwrap();
    assert!(
        wait_playing(&p, Duration::from_secs(25)),
        "{} never started",
        expect.path
    );
    let rx = p.video_rx();

    let mut frames = 0usize;
    let mut black = 0usize;
    let mut structural: Vec<String> = Vec::new();

    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(60) {
        if matches!(p.snapshot().state, PlaybackState::Ended) {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(f) => {
                let y_expected = f.width as usize * f.height as usize;
                let uv_expected =
                    ((f.width as usize + 1) / 2 * 2) * ((f.height as usize + 1) / 2);
                if f.width == 0 || f.height == 0 {
                    structural.push(format!("frame {frames}: zero dimensions"));
                } else if f.y_plane.len() != y_expected {
                    structural.push(format!(
                        "frame {frames}: luma {} bytes, expected {y_expected}",
                        f.y_plane.len()
                    ));
                } else if f.uv_plane.len() != uv_expected {
                    structural.push(format!(
                        "frame {frames}: chroma {} bytes, expected {uv_expected}",
                        f.uv_plane.len()
                    ));
                } else if f.y_plane.iter().all(|&v| v == 0) {
                    structural.push(format!("frame {frames}: luma all zero"));
                }
                if is_black(&f) {
                    black += 1;
                }
                frames += 1;
            }
            Err(_) => continue,
        }
    }
    let _ = p.pause();

    assert!(
        frames > 0,
        "{} produced no frames",
        expect.path
    );
    assert_eq!(
        structural,
        Vec::<String>::new(),
        "{} emitted structurally incomplete frames: {structural:?}",
        expect.path
    );
    assert!(
        black <= expect.source_black,
        "{} emitted {black} black frames but the source only contains {}; \
         {} blank frames were fabricated",
        expect.path,
        expect.source_black,
        black.saturating_sub(expect.source_black)
    );

    (frames, black)
}

#[test]
fn never_emits_an_empty_frame() {
    for expect in FIXTURES {
        if !std::path::Path::new(expect.path).exists() {
            eprintln!("skipping missing {}", expect.path);
            continue;
        }
        let (frames, black) = play_through(expect);
        println!(
            "{:<44} {}x{} frames={frames} black={black}/{}",
            expect.path.rsplit('/').next().unwrap(),
            expect.width,
            expect.height,
            expect.source_black
        );
    }
}