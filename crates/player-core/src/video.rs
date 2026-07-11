use web_time::Duration;

use anyhow::Result;
use repose_core::color::ColorInfo;
use videoson::{
    NalFormat, VideoCodecParams, VideoDecoder as VideoDecoderTrait, VideoDecoderOptions,
    VideoOutputFormat, codec_h264::H264Decoder, codec_rav1d::Rav1dSafeDecoder,
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
}

pub struct VideoDecoder {
    inner: Box<dyn VideoDecoderTrait>,
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
        };
        let inner = H264Decoder::try_new(&params, &opts)
            .map_err(|e| anyhow::anyhow!("videoson H.264 init: {e:?}"))?;
        Ok(Self {
            inner: Box::new(inner),
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
        };
        let inner = Rav1dSafeDecoder::try_new(&params, &opts)
            .map_err(|e| anyhow::anyhow!("videoson AV1 init: {e:?}"))?;
        Ok(Self {
            inner: Box::new(inner),
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

    pub fn drain_frames(
        &mut self,
        fallback_pts: Duration,
        load_serial: u64,
    ) -> Vec<DecodedVideoFrame> {
        let mut frames = Vec::new();

        while let Ok(Some(frame)) = self.inner.receive_frame() {
            // NV12: [Y, UV]
            if frame.plane_data.len() < 2 {
                continue;
            }

            let y_data = match &frame.plane_data[0].data {
                videoson::PlaneData::U8(v) => v.as_slice(),
                _ => continue,
            };
            let uv_data = match &frame.plane_data[1].data {
                videoson::PlaneData::U8(v) => v.as_slice(),
                _ => continue,
            };

            let w = frame.width as usize;
            let h = frame.height as usize;
            let y_stride = frame.plane_data[0].stride;
            let uv_stride = frame.plane_data[1].stride;

            // Defensive tight-pack (decoders already pack, but keep safe).
            let mut y_plane = Vec::with_capacity(w * h);
            for row in 0..h {
                let start = row * y_stride;
                if start + w > y_data.len() {
                    break;
                }
                y_plane.extend_from_slice(&y_data[start..start + w]);
            }

            let uv_w = ((frame.width + 1) / 2 * 2) as usize;
            let uv_h = ((frame.height + 1) / 2) as usize;
            let mut uv_plane = Vec::with_capacity(uv_w * uv_h);
            for row in 0..uv_h {
                let start = row * uv_stride;
                if start + uv_w > uv_data.len() {
                    break;
                }
                uv_plane.extend_from_slice(&uv_data[start..start + uv_w]);
            }

            let pts = frame
                .pts
                .map(|p| Duration::from_micros(p.max(0) as u64))
                .unwrap_or(fallback_pts);

            frames.push(DecodedVideoFrame {
                width: frame.width,
                height: frame.height,
                y_plane,
                uv_plane,
                pts,
                load_serial,
                color_info: ColorInfo::default(),
            });
        }

        frames
    }

    pub fn finish(&mut self) -> Result<Vec<DecodedVideoFrame>> {
        self.inner
            .send_eos()
            .map_err(|e| anyhow::anyhow!("videoson eos: {e:?}"))?;
        Ok(self.drain_frames(Duration::ZERO, 0))
    }

    pub fn reset(&mut self) {
        self.inner.reset();
    }
}

pub fn parse_nal_length_size(extradata: &[u8]) -> u8 {
    if extradata.len() < 5 {
        return 4;
    }
    (extradata[4] & 0x03) + 1
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
