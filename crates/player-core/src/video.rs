use web_time::Duration;

use anyhow::Result;
use repose_core::color::ColorInfo;
use videoson::{
    NalFormat, VideoCodecParams, VideoDecoder as VideoDecoderTrait, VideoDecoderOptions,
    VideoOutputFormat, codec_h264::H264Decoder, codec_h265::H265Decoder,
    codec_rav1d::Rav1dSafeDecoder,
};

#[derive(Debug, Clone)]
pub struct VideoStreamInfo {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub codec_name: &'static str,
}

/// Decoded frame in **NV12** layout, ready for GPU upload.
pub struct DecodedVideoFrame {
    pub width: u32,
    pub height: u32,
    /// Packed luma: `width * height` bytes.
    pub y_plane: Vec<u8>,
    /// Packed chroma: `width * ceil(height/2)` bytes, U/V interleaved.
    pub uv_plane: Vec<u8>,
    pub pts: Duration,
    pub load_serial: u64,
    pub color_info: ColorInfo,
    /// Picture Order Count (display order index).
    /// Set by the H.265 decoder; `None` for other codecs/decodecs.
    pub poc: Option<i32>,
}

pub struct VideoDecoder {
    inner: Box<dyn VideoDecoderTrait>,
    /// Reorder buffer that holds frames by PTS and emits the smallest-PTS
    /// frame each call. Cross-batch reordering is handled by VideoSink::poll().
    reorder: Vec<DecodedVideoFrame>,
}

impl VideoDecoder {
    pub fn new_h264(width: u32, height: u32, extradata: &[u8]) -> Result<Self> {
        let nal_length_size = parse_nal_length_size(extradata);
        let params = VideoCodecParams {
            codec: videoson::CodecType::H264,
            coded_width: width,
            coded_height: height,
            extradata: extradata.to_vec(),
            nal_format: Some(NalFormat::Avcc {
                nal_len_size: nal_length_size,
            }),
        };
        let opts = VideoDecoderOptions {
            verify: false,
            output_format: VideoOutputFormat::Nv12,
            tolerate_truncated_chroma: false,
        };
        let inner = H264Decoder::try_new(&params, &opts)
            .map_err(|e| anyhow::anyhow!("videoson H.264 init: {e:?}"))?;
        Ok(Self {
            inner: Box::new(inner),
            reorder: Vec::new(),
        })
    }

    pub fn new_av1(width: u32, height: u32, extradata: &[u8]) -> Result<Self> {
        let params = VideoCodecParams {
            codec: videoson::CodecType::AV1,
            coded_width: width,
            coded_height: height,
            extradata: extradata.to_vec(),
            nal_format: None,
        };
        let opts = VideoDecoderOptions {
            verify: false,
            output_format: VideoOutputFormat::Nv12,
            tolerate_truncated_chroma: false,
        };
        let inner = Rav1dSafeDecoder::try_new(&params, &opts)
            .map_err(|e| anyhow::anyhow!("videoson AV1 init: {e:?}"))?;
        Ok(Self {
            inner: Box::new(inner),
            reorder: Vec::new(),
        })
    }

    pub fn send_packet(&mut self, data: &[u8], pts_us: i64, is_sync: bool) -> Result<()> {
        let packet = videoson::Packet {
            track_id: 0,
            pts: Some(pts_us),
            dts: None,
            duration: None,
            is_sync,
            data: data.to_vec(),
        };
        self.inner
            .send_packet(&packet)
            .map_err(|e| anyhow::anyhow!("videoson send: {e:?}"))
    }

    pub fn set_frame_duration_micros(&mut self, us: u64) {
        self.inner.set_frame_duration_micros(us);
    }

    fn plane_u8(
        &self,
        data: &videoson::PlaneData,
        width: usize,
        height: usize,
        stride: usize,
    ) -> Vec<u8> {
        match data {
            videoson::PlaneData::U8(s) => {
                let mut plane = Vec::with_capacity(width * height);
                for row in 0..height {
                    let start = row * stride;
                    if start + width > s.len() {
                        break;
                    }
                    plane.extend_from_slice(&s[start..start + width]);
                }
                plane
            }
            videoson::PlaneData::U16(s) => {
                let mut plane = Vec::with_capacity(width * height);
                for row in 0..height {
                    let start = row * stride;
                    for col in 0..width {
                        if start + col >= s.len() {
                            break;
                        }
                        plane.push((s[start + col] >> 2) as u8);
                    }
                }
                plane
            }
        }
    }

    pub fn drain_frames(
        &mut self,
        fallback_pts: Duration,
        load_serial: u64,
        frame_duration_us: u64,
    ) -> Vec<DecodedVideoFrame> {
        while let Ok(Some(frame)) = self.inner.receive_frame() {
            if frame.plane_data.len() < 2 {
                log::warn!("video drain: frame with <2 planes, skipping");
                continue;
            }

            let w = frame.width as usize;
            let h = frame.height as usize;
            let y_stride = frame.plane_data[0].stride;
            let uv_stride = frame.plane_data[1].stride;
            let uv_w = ((frame.width + 1) / 2 * 2) as usize;
            let uv_h = ((frame.height + 1) / 2) as usize;

            let y_plane = self.plane_u8(&frame.plane_data[0].data, w, h, y_stride);
            let uv_plane = self.plane_u8(&frame.plane_data[1].data, uv_w, uv_h, uv_stride);

            // PTS from container may be non-monotonic when the muxer assumed
            // B-frame reordering but the bitstream has none.  Recompute from
            // POC when frame_duration_us is provided.
            let pts = if frame_duration_us > 0 {
                frame
                    .poc
                    .filter(|&p| p >= 0)
                    .map(|p| Duration::from_micros(p as u64 * frame_duration_us))
                    .unwrap_or(
                        frame
                            .pts
                            .map(|p| Duration::from_micros(p.max(0) as u64))
                            .unwrap_or(fallback_pts),
                    )
            } else {
                frame
                    .pts
                    .map(|p| Duration::from_micros(p.max(0) as u64))
                    .unwrap_or(fallback_pts)
            };

            self.reorder.push(DecodedVideoFrame {
                width: frame.width,
                height: frame.height,
                y_plane,
                uv_plane,
                pts,
                load_serial,
                color_info: ColorInfo::default(),
                poc: frame.poc,
            });
        }

        if self.reorder.is_empty() {
            return Vec::new();
        }

        // Sort by PTS to handle decoders that emit frames out of display
        // order (e.g. H.264 with B-frames).  The H.265 decoder emits in
        // strict POC order, so this sort is redundant but harmless.
        // No hold-back: frames leave immediately so the renderer's
        // VideoSink can schedule them by PTS without a 1-frame delay.
        self.reorder.sort_by_key(|f| f.pts);
        return self.reorder.drain(..).collect();
    }

    pub fn finish(&mut self, frame_duration_us: u64) -> Result<Vec<DecodedVideoFrame>> {
        self.inner
            .send_eos()
            .map_err(|e| anyhow::anyhow!("videoson eos: {e:?}"))?;
        // Drain remaining frames from decoder (including decoder's pending
        // frames flushed by send_eos).  No hold-back in drain_frames, so
        // self.reorder is always empty after the call.
        let mut result = self.drain_frames(Duration::ZERO, 0, frame_duration_us);
        result.extend(self.reorder.drain(..));
        Ok(result)
    }

    pub fn new_hevc(width: u32, height: u32, extradata: &[u8]) -> Result<Self> {
        let nal_len_size = parse_nal_length_size_hevc(extradata);
        let params = VideoCodecParams {
            codec: videoson::CodecType::H265,
            coded_width: width,
            coded_height: height,
            extradata: extradata.to_vec(),
            nal_format: Some(NalFormat::Hvcc { nal_len_size }),
        };
        let opts = VideoDecoderOptions {
            verify: false,
            output_format: VideoOutputFormat::Nv12,
            tolerate_truncated_chroma: false,
        };
        let inner = H265Decoder::try_new(&params, &opts)
            .map_err(|e| anyhow::anyhow!("videoson H.265 init: {e:?}"))?;
        Ok(Self {
            inner: Box::new(inner),
            reorder: Vec::new(),
        })
    }

    pub fn reset(&mut self) {
        let _ = self.inner.reset();
        self.reorder.clear();
    }
}

pub fn parse_nal_length_size(extradata: &[u8]) -> u8 {
    if extradata.len() < 5 {
        return 4;
    }
    (extradata[4] & 0x03) + 1
}

pub fn parse_nal_length_size_hevc(extradata: &[u8]) -> u8 {
    if extradata.len() > 21 {
        (extradata[21] & 0x03) + 1
    } else {
        4
    }
}

/// Legacy helper for callers that still have separate U/V planes.
pub fn yuv420_uv_to_nv12(u_plane: &[u8], v_plane: &[u8], width: u32, height: u32) -> Vec<u8> {
    let uv_size = (width.div_ceil(2) * height.div_ceil(2)) as usize;
    let mut uv = Vec::with_capacity(uv_size * 2);
    for i in 0..uv_size {
        uv.push(u_plane.get(i).copied().unwrap_or(128));
        uv.push(v_plane.get(i).copied().unwrap_or(128));
    }
    uv
}
