//! player-sync: the A/V synchronization contract for PurePlay.
//!
//! (`videoson` H.264/AV1) plugs into these traits.
//! The AUDIO pipeline is the master clock; video only observes it.

use std::sync::Arc;
use std::time::Duration;

use player_core::AudioPlayer;

/// A monotonic media clock. Video presentation reads this to decide
/// which frame to display and when to drop frames.
pub trait MediaClock: Send + Sync {
    /// Current presentation time of the media stream.
    fn now(&self) -> Duration;
    /// Whether the clock is advancing.
    fn is_running(&self) -> bool;
}

/// The audio engine IS the master clock: position is derived from
/// frames actually consumed by the CPAL callback, so it reflects
/// what the listener has heard, not what has been decoded.
impl MediaClock for AudioPlayer {
    fn now(&self) -> Duration {
        self.position()
    }
    fn is_running(&self) -> bool {
        self.is_playing()
    }
}

/// One decoded video frame in planar 8-bit YUV420 —
/// exactly what videoson's H.264 (`rust_h264`) and AV1 (`rav1d-safe`)
/// decoders emit for the supported profile.
#[derive(Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub y_plane: Arc<Vec<u8>>,
    pub u_plane: Arc<Vec<u8>>,
    pub v_plane: Arc<Vec<u8>>,
    pub y_stride: usize,
    pub uv_stride: usize,
    /// Presentation timestamp relative to stream start.
    pub pts: Duration,
}

/// Where decoded frames go. Later `VideoSurface` should implement
/// this (upload Y/U/V planes to three WGPU textures and doing
/// YUV→RGB in a shader.)
pub trait VideoFrameSink: Send + Sync {
    /// Submit a frame for presentation. The sink decides (using the
    /// `MediaClock`) whether to display, hold, or drop it.
    fn submit(&self, frame: VideoFrame);
    /// Flush all pending frames (on seek or stop).
    fn flush(&self);
}

/// The decoder-side abstraction videoson will sit behind.
/// `open` gets a demuxed elementary stream; `next_frame` pulls
/// decoded frames in presentation order.
pub trait VideoDecoder: Send {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    /// Decode and return the next frame, or `None` at end of stream.
    fn next_frame(&mut self) -> Option<VideoFrame>;
    /// Reset internal state after a seek.
    fn reset(&mut self);
}

/// Drives presentation: pulls frames from a `VideoDecoder`, compares
/// `frame.pts` against the `MediaClock`, and pushes on-time frames
/// into the `VideoFrameSink`.
pub struct SyncDriver<C: MediaClock> {
    pub clock: Arc<C>,
    /// Frames later than this behind the clock are dropped.
    pub drop_threshold: Duration,
}

impl<C: MediaClock> SyncDriver<C> {
    pub fn new(clock: Arc<C>) -> Self {
        Self {
            clock,
            drop_threshold: Duration::from_millis(50),
        }
    }

    /// Decide what to do with a frame given the current clock.
    pub fn schedule(&self, pts: Duration) -> FrameAction {
        let now = self.clock.now();
        if pts + self.drop_threshold < now {
            FrameAction::Drop
        } else if pts <= now {
            FrameAction::PresentNow
        } else {
            FrameAction::WaitFor(pts - now)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameAction {
    PresentNow,
    WaitFor(Duration),
    Drop,
}
