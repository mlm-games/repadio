use std::time::Duration;

use anyhow::Result;
use videoson::{
    NalFormat, VideoCodecParams, VideoDecoder as VideoDecoderTrait, VideoDecoderOptions,
    codec_h264::H264Decoder, codec_rav1d::Rav1dSafeDecoder,
};

#[derive(Debug, Clone)]
pub struct VideoStreamInfo {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub codec_name: &'static str,
}

pub struct DecodedVideoFrame {
    pub width: u32,
    pub height: u32,
    pub y_plane: Vec<u8>,
    pub u_plane: Vec<u8>,
    pub v_plane: Vec<u8>,
    pub pts: Duration,
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
        let inner = H264Decoder::try_new(&params, &VideoDecoderOptions::default())
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
        let inner = Rav1dSafeDecoder::try_new(&params, &VideoDecoderOptions::default())
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

    pub fn drain_frames(&mut self, fallback_pts: Duration) -> Vec<DecodedVideoFrame> {
        let mut frames = Vec::new();
        while let Ok(Some(frame)) = self.inner.receive_frame() {
            let w = frame.width as u32;
            let h = frame.height as u32;

            let y_data = match &frame.plane_data[0].data {
                videoson::PlaneData::U8(v) => v.as_slice(),
                _ => continue,
            };
            // If U or V planes are missing, skip the frame.
            let u_plane_data = match frame.plane_data.get(1).and_then(|p| match &p.data {
                videoson::PlaneData::U8(v) => Some(v.as_slice()),
                _ => None,
            }) {
                Some(d) if !d.is_empty() => d,
                _ => continue,
            };
            let v_plane_data = match frame.plane_data.get(2).and_then(|p| match &p.data {
                videoson::PlaneData::U8(v) => Some(v.as_slice()),
                _ => None,
            }) {
                Some(d) if !d.is_empty() => d,
                _ => continue,
            };

            let y_stride = frame.plane_data[0].stride;
            let u_stride = frame.plane_data[1].stride;
            let v_stride = frame.plane_data[2].stride;

            // Tightly pack Y plane (remove stride padding).
            let mut y_plane = Vec::with_capacity(w as usize * h as usize);
            for row in 0..h as usize {
                let start = row * y_stride;
                y_plane.extend_from_slice(&y_data[start..start + w as usize]);
            }

            let uv_w = (w / 2) as usize;
            let uv_h = (h / 2) as usize;
            let mut u_plane = Vec::with_capacity(uv_w * uv_h);
            let mut v_plane = Vec::with_capacity(uv_w * uv_h);
            for row in 0..uv_h {
                let u_start = row * u_stride;
                let v_start = row * v_stride;
                u_plane.extend_from_slice(&u_plane_data[u_start..u_start + uv_w]);
                v_plane.extend_from_slice(&v_plane_data[v_start..v_start + uv_w]);
            }

            let pts = frame
                .pts
                .map(|p| Duration::from_micros(p.max(0) as u64))
                .unwrap_or(fallback_pts);

            frames.push(DecodedVideoFrame {
                width: w,
                height: h,
                y_plane,
                u_plane,
                v_plane,
                pts,
            });
        }
        frames
    }

    pub fn finish(&mut self) -> Result<Vec<DecodedVideoFrame>> {
        self.inner
            .send_eos()
            .map_err(|e| anyhow::anyhow!("videoson eos: {e:?}"))?;
        Ok(self.drain_frames(Duration::ZERO))
    }

    pub fn reset(&mut self) {
        self.inner.reset();
    }
}

fn parse_nal_length_size(extradata: &[u8]) -> u8 {
    if extradata.len() < 5 {
        return 4;
    }
    // Byte 4: bits 0-1 = lengthSizeMinusOne
    (extradata[4] & 0x03) + 1
}

pub fn yuv420_to_nv12(
    _y_plane: &[u8],
    u_plane: &[u8],
    v_plane: &[u8],
    width: u32,
    height: u32,
) -> Vec<u8> {
    let uv_size = (width / 2 * height / 2) as usize;
    let mut uv = Vec::with_capacity(uv_size * 2);
    for i in 0..uv_size {
        uv.push(u_plane.get(i).copied().unwrap_or(128));
        uv.push(v_plane.get(i).copied().unwrap_or(128));
    }
    uv
}
