use std::sync::Arc;

use web_time::Duration;

use anyhow::Result;
use repose_core::color::ColorInfo;
use videoson::{
    NalFormat, VideoCodecParams, VideoDecoder as VideoDecoderTrait, VideoDecoderOptions,
    VideoOutputFormat, codec_h264::H264Decoder, codec_h265::H265Decoder,
    codec_rav1d::Rav1dSafeDecoder, codec_vp8::Vp8Decoder, codec_vp9::Vp9Decoder,
};

#[cfg(feature = "hw")]
use baabaabaabaabababbababbaa::traits::{VideoDecoderInputBoxed, VideoDecoderOutputBoxed};
#[cfg(feature = "hw")]
use baabaabaabaabababbababbaa::{
    Dimensions, VideoCodecId as HwCodecId, VideoDecoderConfig, VideoOutputMode, default_host,
};
#[cfg(feature = "hw")]
use bytes::Bytes;

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
    pub y_plane: Arc<[u8]>,
    /// Packed chroma: `width * ceil(height/2)` bytes, U/V interleaved.
    pub uv_plane: Arc<[u8]>,
    pub pts: Duration,
    pub load_serial: u64,
    pub color_info: ColorInfo,
    /// Picture Order Count (display order index).
    /// Set by the H.265 decoder; `None` for other codecs/decodecs.
    pub poc: Option<i32>,
}

enum DecoderInner {
    Software(Box<dyn VideoDecoderTrait>),
    #[cfg(feature = "hw")]
    Hardware(Box<HwDecoder>),
}

#[cfg(feature = "hw")]
struct HwDecoder {
    input: Box<dyn VideoDecoderInputBoxed>,
    output: Box<dyn VideoDecoderOutputBoxed>,
    #[cfg(not(target_arch = "wasm32"))]
    runtime: tokio::runtime::Runtime,
    nal_len_size: usize,
    pending_config: Option<Vec<u8>>,
    initial_config: Vec<u8>,
}

#[cfg(feature = "hw")]
impl HwDecoder {
    fn try_new(codec: HwCodecId, width: u32, height: u32, extradata: &[u8]) -> Option<Self> {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (codec, width, height, extradata);
            return None;
        }
        let description = if extradata.is_empty() {
            None
        } else {
            Some(Bytes::copy_from_slice(extradata))
        };
        let host = default_host();
        let config = VideoDecoderConfig {
            codec: codec.clone(),
            resolution: Some(Dimensions::new(width, height)),
            description,
            hardware_acceleration: Some(true),
            output_mode: VideoOutputMode::Cpu,
        };
        let (input, output) = host.create_video_decoder(config).ok()?;
        #[cfg(not(target_arch = "wasm32"))]
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        let (initial_config, nal_len_size) = match &codec {
            HwCodecId::H264 { .. } => (
                parse_avcc(extradata),
                parse_nal_length_size(extradata) as usize,
            ),
            HwCodecId::Hevc => (
                parse_hvcc(extradata),
                parse_nal_length_size_hevc(extradata) as usize,
            ),
            _ => (Vec::new(), 0),
        };
        let pending_config = if initial_config.is_empty() {
            None
        } else {
            Some(initial_config.clone())
        };
        Some(Self {
            input: Box::new(input) as Box<dyn VideoDecoderInputBoxed>,
            output: Box::new(output) as Box<dyn VideoDecoderOutputBoxed>,
            #[cfg(not(target_arch = "wasm32"))]
            runtime,
            nal_len_size,
            pending_config,
            initial_config,
        })
    }

    fn decode(&mut self, data: &[u8], pts: Duration, is_sync: bool) -> Result<()> {
        let mut payload: Vec<u8> = if self.nal_len_size > 0 && !has_annexb_start_code(data) {
            avcc_to_annexb_with_len(data, self.nal_len_size)
        } else {
            data.to_vec()
        };
        let mut send_sync = is_sync;
        if let Some(cfg) = self.pending_config.take() {
            let mut prefixed = Vec::with_capacity(cfg.len() + payload.len());
            prefixed.extend_from_slice(&cfg);
            prefixed.extend_from_slice(&payload);
            payload = prefixed;
            send_sync = true;
        }
        let pkt = baabaabaabaabababbababbaa::EncodedVideoPacket {
            payload: Bytes::from(payload),
            timestamp: pts,
            keyframe: send_sync,
        };
        self.input
            .decode(pkt)
            .map_err(|e| anyhow::anyhow!("hw decode: {e:?}"))
    }

    fn try_drain_hw_frames(&mut self) -> Vec<baabaabaabaabababbababbaa::VideoFrame> {
        let mut out = Vec::new();
        loop {
            match self.output.try_frame() {
                Ok(Some(f)) => out.push(f),
                Ok(None) => break,
                Err(_) => break,
            }
        }
        out
    }

    fn flush(&mut self) -> Result<()> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.runtime
                .block_on(self.input.flush())
                .map_err(|e| anyhow::anyhow!("hw flush: {e:?}"))?;
        }
        #[cfg(target_arch = "wasm32")]
        {
            // WebCodecs flush is async but try_frame will drain anyway; no-op for now
        }
        Ok(())
    }

    fn reset(&mut self) {
        let _ = self.flush();
        if !self.initial_config.is_empty() {
            self.pending_config = Some(self.initial_config.clone());
        }
        loop {
            match self.output.try_frame() {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }
    }
}

#[cfg(feature = "hw")]
fn hw_frame_to_decoded(
    mut frame: baabaabaabaabababbababbaa::VideoFrame,
    fallback_pts: Duration,
    load_serial: u64,
) -> Option<DecodedVideoFrame> {
    // Ensure CPU accessible (copies DmaBuf/WebCodecs hardware buffers)
    if frame.is_hardware() {
        if frame.ensure_cpu().is_err() {
            log::warn!("hw frame ensure_cpu failed");
            return None;
        }
    }
    let w = frame.dimensions.width;
    let h = frame.dimensions.height;
    let pts = if frame.timestamp != Duration::ZERO {
        frame.timestamp
    } else {
        fallback_pts
    };
    // `VideoPlanes::Cpu(Vec<u8>)` holds packed planes according to `frame.format`
    let data = match &frame.planes {
        baabaabaabaabababbababbaa::VideoPlanes::Cpu(v) => v.clone(),
        baabaabaabaabababbababbaa::VideoPlanes::Hardware(_) => {
            log::warn!("hw frame still hardware after ensure_cpu");
            return None;
        }
    };
    let (y_plane, uv_plane) = match frame.format {
        baabaabaabaabababbababbaa::PixelFormat::Nv12 => {
            let y_size = (w as usize) * (h as usize);
            let uv_size = (w as usize) * ((h as usize + 1) / 2);
            if data.len() < y_size + uv_size {
                log::warn!("hw Nv12 frame too short");
                return None;
            }
            let y = &data[0..y_size];
            let uv = &data[y_size..y_size + uv_size];
            (
                Arc::<[u8]>::from(y.to_vec()),
                Arc::<[u8]>::from(uv.to_vec()),
            )
        }
        baabaabaabaabababbababbaa::PixelFormat::Yuv420p => {
            let y_size = (w as usize) * (h as usize);
            let uv_w = (w as usize + 1) / 2;
            let uv_h = (h as usize + 1) / 2;
            let u_size = uv_w * uv_h;
            let v_size = u_size;
            if data.len() < y_size + u_size + v_size {
                log::warn!("hw Yuv420p frame too short");
                return None;
            }
            let y = &data[0..y_size];
            let u = &data[y_size..y_size + u_size];
            let v = &data[y_size + u_size..y_size + u_size + v_size];
            let mut uv = Vec::with_capacity(u_size * 2);
            for i in 0..u_size {
                uv.push(u[i]);
                uv.push(v[i]);
            }
            (Arc::<[u8]>::from(y.to_vec()), Arc::<[u8]>::from(uv))
        }
        _ => {
            log::warn!("hw unsupported pixel format {:?}", frame.format);
            return None;
        }
    };
    Some(DecodedVideoFrame {
        width: w,
        height: h,
        y_plane,
        uv_plane,
        pts,
        load_serial,
        color_info: ColorInfo::default(),
        poc: None,
    })
}

enum FallbackCodec {
    H264,
    H265,
    Av1,
    Vp8,
    Vp9,
}

pub struct VideoDecoder {
    inner: DecoderInner,

    reorder: Vec<DecodedVideoFrame>,
    fallback: Option<(FallbackCodec, u32, u32, Vec<u8>)>,
}

impl VideoDecoder {
    /// `true` if this instance is using a hardware backend (`vaapi`/`mediacodec`/`webcodecs`), `false` for `videoson` software.
    /// Cheap to call; useful for `PlayerSnapshot` / `mpv`-like `hwdec-current` property.
    pub fn is_hardware(&self) -> bool {
        #[cfg(feature = "hw")]
        {
            matches!(self.inner, DecoderInner::Hardware(_))
        }
        #[cfg(not(feature = "hw"))]
        {
            false
        }
    }

    pub fn decoder_kind(&self) -> &'static str {
        if self.is_hardware() { "hw" } else { "sw" }
    }
}

impl VideoDecoder {
    fn software_fallback_h264(
        width: u32,
        height: u32,
        extradata: &[u8],
    ) -> Result<Box<dyn VideoDecoderTrait>> {
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
            ..Default::default()
        };
        let inner = H264Decoder::try_new(&params, &opts)
            .map_err(|e| anyhow::anyhow!("videoson H.264 init: {e:?}"))?;
        Ok(Box::new(inner))
    }

    pub fn new_h264(width: u32, height: u32, extradata: &[u8]) -> Result<Self> {
        #[cfg(feature = "hw")]
        {
            if let Some(hw) = HwDecoder::try_new(
                HwCodecId::H264 {
                    profile: None,
                    level: None,
                },
                width,
                height,
                extradata,
            ) {
                log::info!("video: H.264 HW decoder selected [hw] hwdec-current=hw");
                return Ok(Self {
                    inner: DecoderInner::Hardware(Box::new(hw)),
                    reorder: Vec::new(),
                    fallback: Some((FallbackCodec::H264, width, height, extradata.to_vec())),
                });
            } else {
                log::info!("video: H.264 HW unavailable, using SW [sw]");
            }
        }
        Ok(Self {
            inner: DecoderInner::Software(Self::software_fallback_h264(width, height, extradata)?),
            reorder: Vec::new(),
            fallback: None,
        })
    }

    fn software_fallback_av1(
        width: u32,
        height: u32,
        extradata: &[u8],
    ) -> Result<Box<dyn VideoDecoderTrait>> {
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
            ..Default::default()
        };
        let inner = Rav1dSafeDecoder::try_new(&params, &opts)
            .map_err(|e| anyhow::anyhow!("videoson AV1 init: {e:?}"))?;
        Ok(Box::new(inner))
    }

    pub fn new_av1(width: u32, height: u32, extradata: &[u8]) -> Result<Self> {
        #[cfg(feature = "hw")]
        {
            if let Some(hw) = HwDecoder::try_new(HwCodecId::Av1, width, height, extradata) {
                log::info!("video: AV1 HW decoder selected [hw] hwdec-current=hw");
                return Ok(Self {
                    inner: DecoderInner::Hardware(Box::new(hw)),
                    reorder: Vec::new(),
                    fallback: Some((FallbackCodec::Av1, width, height, extradata.to_vec())),
                });
            } else {
                log::info!("video: AV1 HW unavailable, using SW [sw]");
            }
        }
        Ok(Self {
            inner: DecoderInner::Software(Self::software_fallback_av1(width, height, extradata)?),
            reorder: Vec::new(),
            fallback: None,
        })
    }

    fn software_fallback_vp8(
        width: u32,
        height: u32,
        extradata: &[u8],
    ) -> Result<Box<dyn VideoDecoderTrait>> {
        let params = VideoCodecParams {
            codec: videoson::CodecType::VP8,
            coded_width: width,
            coded_height: height,
            extradata: extradata.to_vec(),
            nal_format: None,
        };
        let opts = VideoDecoderOptions {
            verify: false,
            output_format: VideoOutputFormat::Nv12,
            tolerate_truncated_chroma: false,
            ..Default::default()
        };
        let inner = Vp8Decoder::try_new(&params, &opts)
            .map_err(|e| anyhow::anyhow!("videoson VP8 init: {e:?}"))?;
        Ok(Box::new(inner))
    }

    pub fn new_vp8(width: u32, height: u32, extradata: &[u8]) -> Result<Self> {
        #[cfg(feature = "hw")]
        {
            if let Some(hw) = HwDecoder::try_new(HwCodecId::Vp8, width, height, extradata) {
                log::info!("video: VP8 HW decoder selected [hw] hwdec-current=hw");
                return Ok(Self {
                    inner: DecoderInner::Hardware(Box::new(hw)),
                    reorder: Vec::new(),
                    fallback: Some((FallbackCodec::Vp8, width, height, extradata.to_vec())),
                });
            } else {
                log::info!("video: VP8 HW unavailable, using SW [sw]");
            }
        }
        Ok(Self {
            inner: DecoderInner::Software(Self::software_fallback_vp8(width, height, extradata)?),
            reorder: Vec::new(),
            fallback: None,
        })
    }

    fn software_fallback_vp9(
        width: u32,
        height: u32,
        extradata: &[u8],
    ) -> Result<Box<dyn VideoDecoderTrait>> {
        let params = VideoCodecParams {
            codec: videoson::CodecType::VP9,
            coded_width: width,
            coded_height: height,
            extradata: extradata.to_vec(),
            nal_format: None,
        };
        #[cfg(not(target_arch = "wasm32"))]
        let threads = Some(4);
        #[cfg(target_arch = "wasm32")]
        let threads = None;
        let opts = VideoDecoderOptions {
            verify: false,
            output_format: VideoOutputFormat::Nv12,
            tolerate_truncated_chroma: false,
            threads,
            ..Default::default()
        };
        let inner = Vp9Decoder::try_new(&params, &opts)
            .map_err(|e| anyhow::anyhow!("videoson VP9 init: {e:?}"))?;
        Ok(Box::new(inner))
    }

    pub fn new_vp9(width: u32, height: u32, extradata: &[u8]) -> Result<Self> {
        #[cfg(feature = "hw")]
        {
            if let Some(hw) = HwDecoder::try_new(HwCodecId::Vp9, width, height, extradata) {
                log::info!("video: VP9 HW decoder selected [hw] hwdec-current=hw");
                return Ok(Self {
                    inner: DecoderInner::Hardware(Box::new(hw)),
                    reorder: Vec::new(),
                    fallback: Some((FallbackCodec::Vp9, width, height, extradata.to_vec())),
                });
            } else {
                log::info!("video: VP9 HW unavailable, using SW [sw]");
            }
        }
        Ok(Self {
            inner: DecoderInner::Software(Self::software_fallback_vp9(width, height, extradata)?),
            reorder: Vec::new(),
            fallback: None,
        })
    }

    pub fn new_hevc_software(width: u32, height: u32, extradata: &[u8]) -> Result<Self> {
        Ok(Self {
            inner: DecoderInner::Software(Self::software_fallback_hevc(width, height, extradata)?),
            reorder: Vec::new(),
            fallback: None,
        })
    }

    fn software_fallback_hevc(
        width: u32,
        height: u32,
        extradata: &[u8],
    ) -> Result<Box<dyn VideoDecoderTrait>> {
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
            ..Default::default()
        };
        let inner = H265Decoder::try_new(&params, &opts)
            .map_err(|e| anyhow::anyhow!("videoson H.265 init: {e:?}"))?;
        Ok(Box::new(inner))
    }

    pub fn new_hevc(width: u32, height: u32, extradata: &[u8]) -> Result<Self> {
        #[cfg(feature = "hw")]
        {
            if let Some(hw) = HwDecoder::try_new(HwCodecId::Hevc, width, height, extradata) {
                log::info!("video: HEVC HW decoder selected [hw] hwdec-current=hw");
                return Ok(Self {
                    inner: DecoderInner::Hardware(Box::new(hw)),
                    reorder: Vec::new(),
                    fallback: Some((FallbackCodec::H265, width, height, extradata.to_vec())),
                });
            } else {
                log::info!("video: HEVC HW unavailable, using SW [sw]");
            }
        }
        Ok(Self {
            inner: DecoderInner::Software(Self::software_fallback_hevc(width, height, extradata)?),
            reorder: Vec::new(),
            fallback: None,
        })
    }

    pub fn send_packet(&mut self, data: &[u8], pts_us: i64, is_sync: bool) -> Result<()> {
        match &mut self.inner {
            DecoderInner::Software(inner) => {
                let packet = videoson::Packet {
                    track_id: 0,
                    pts: Some(pts_us),
                    dts: None,
                    duration: None,
                    is_sync,
                    data: data.to_vec(),
                };
                inner
                    .send_packet(&packet)
                    .map_err(|e| anyhow::anyhow!("videoson send: {e:?}"))
            }
            #[cfg(feature = "hw")]
            DecoderInner::Hardware(hw) => {
                let res = hw.decode(data, Duration::from_micros(pts_us.max(0) as u64), is_sync);
                if let Err(e) = &res {
                    let msg = format!("{e:?}");
                    if msg.contains("Dropped") || msg.contains("NoBackend") {
                        if let Some((codec, w, h, extradata)) = self.fallback.take() {
                            log::warn!("HW decode failed ({msg}), falling back to SW");
                            let sw: Box<dyn VideoDecoderTrait> = match codec {
                                FallbackCodec::H264 => {
                                    Self::software_fallback_h264(w, h, &extradata)?
                                }
                                FallbackCodec::H265 => {
                                    Self::software_fallback_hevc(w, h, &extradata)?
                                }
                                FallbackCodec::Av1 => {
                                    Self::software_fallback_av1(w, h, &extradata)?
                                }
                                FallbackCodec::Vp8 => {
                                    Self::software_fallback_vp8(w, h, &extradata)?
                                }
                                FallbackCodec::Vp9 => {
                                    Self::software_fallback_vp9(w, h, &extradata)?
                                }
                            };
                            self.inner = DecoderInner::Software(sw);
                            let packet = videoson::Packet {
                                track_id: 0,
                                pts: Some(pts_us),
                                dts: None,
                                duration: None,
                                is_sync,
                                data: data.to_vec(),
                            };
                            if let DecoderInner::Software(inner) = &mut self.inner {
                                return inner.send_packet(&packet).map_err(|e| {
                                    anyhow::anyhow!("videoson send after fallback: {e:?}")
                                });
                            }
                        }
                    }
                }
                res
            }
        }
    }

    pub fn set_frame_duration_micros(&mut self, us: u64) {
        match &mut self.inner {
            DecoderInner::Software(inner) => inner.set_frame_duration_micros(us),
            #[cfg(feature = "hw")]
            DecoderInner::Hardware(_) => {}
        }
    }

    fn plane_to_arc(
        data: &videoson::PlaneData,
        width: usize,
        height: usize,
        stride: usize,
    ) -> Arc<[u8]> {
        let size = width * height;
        let mut arc = Arc::new_uninit_slice(size);
        // Safety: freshly created Arc has refcount 1.
        let uninit = Arc::get_mut(&mut arc).unwrap();
        // Cast MaybeUninit<u8> → u8 (same layout) for direct writing.
        let dst: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(uninit.as_mut_ptr().cast(), size) };
        match data {
            videoson::PlaneData::U8(s) => {
                for row in 0..height {
                    let src_start = row * stride;
                    let dst_start = row * width;
                    let avail = width.min(s.len().saturating_sub(src_start));
                    if avail == 0 {
                        break;
                    }
                    dst[dst_start..dst_start + avail]
                        .copy_from_slice(&s[src_start..src_start + avail]);
                }
            }
            videoson::PlaneData::U16(s) => {
                for row in 0..height {
                    let src_start = row * stride;
                    let dst_start = row * width;
                    for col in 0..width {
                        if src_start + col >= s.len() {
                            break;
                        }
                        dst[dst_start + col] = (s[src_start + col] >> 2) as u8;
                    }
                }
            }
        }
        // Safety: all `size` elements were written above.
        unsafe { arc.assume_init() }
    }

    pub fn drain_frames(
        &mut self,
        fallback_pts: Duration,
        load_serial: u64,
        frame_duration_us: u64,
    ) -> Vec<DecodedVideoFrame> {
        // HW path: poll try_frame
        match &mut self.inner {
            #[cfg(feature = "hw")]
            DecoderInner::Hardware(hw) => {
                for frame in hw.try_drain_hw_frames() {
                    if let Some(decoded) = hw_frame_to_decoded(frame, fallback_pts, load_serial) {
                        self.reorder.push(decoded);
                    }
                }
            }
            DecoderInner::Software(inner) => {
                while let Ok(Some(frame)) = inner.receive_frame() {
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

                    let y_plane = Self::plane_to_arc(&frame.plane_data[0].data, w, h, y_stride);
                    let uv_plane =
                        Self::plane_to_arc(&frame.plane_data[1].data, uv_w, uv_h, uv_stride);

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
            }
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
        self.reorder.drain(..).collect()
    }

    pub fn finish(&mut self, frame_duration_us: u64) -> Result<Vec<DecodedVideoFrame>> {
        match &mut self.inner {
            DecoderInner::Software(inner) => {
                inner
                    .send_eos()
                    .map_err(|e| anyhow::anyhow!("videoson eos: {e:?}"))?;
            }
            #[cfg(feature = "hw")]
            DecoderInner::Hardware(hw) => {
                hw.flush()?;
            }
        }
        // Drain remaining frames from decoder (including decoder's pending
        // frames flushed by send_eos).  No hold-back in drain_frames, so
        // self.reorder is always empty after the call.
        let mut result = self.drain_frames(Duration::ZERO, 0, frame_duration_us);
        result.extend(self.reorder.drain(..));
        Ok(result)
    }

    pub fn reset(&mut self) {
        match &mut self.inner {
            DecoderInner::Software(inner) => {
                let _ = inner.reset();
            }
            #[cfg(feature = "hw")]
            DecoderInner::Hardware(hw) => {
                hw.reset();
            }
        }
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

pub fn parse_hvcc(data: &[u8]) -> Vec<u8> {
    if data.len() < 23 || data[0] != 1 {
        return Vec::new();
    }
    let num_arrays = data[22] as usize;
    let mut out = Vec::new();
    let mut pos = 23usize;
    for _ in 0..num_arrays {
        if pos >= data.len() {
            break;
        }
        let _nal_type = data[pos] & 0x3F;
        pos += 1;
        if pos + 2 > data.len() {
            break;
        }
        let num_nalus = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        for _ in 0..num_nalus {
            if pos + 2 > data.len() {
                break;
            }
            let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + len > data.len() {
                break;
            }
            out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            out.extend_from_slice(&data[pos..pos + len]);
            pos += len;
        }
    }
    out
}

pub fn parse_avcc(data: &[u8]) -> Vec<u8> {
    if data.len() < 6 || data[0] != 1 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut pos = 6usize;
    let num_sps = (data[5] & 0x1F) as usize;
    for _ in 0..num_sps {
        if pos + 2 > data.len() {
            break;
        }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + len > data.len() {
            break;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }
    if pos >= data.len() {
        return out;
    }
    let num_pps = data[pos] as usize;
    pos += 1;
    for _ in 0..num_pps {
        if pos + 2 > data.len() {
            break;
        }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;
        if pos + len > data.len() {
            break;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&data[pos..pos + len]);
        pos += len;
    }
    out
}

pub fn avcc_to_annexb_with_len(data: &[u8], len_size: usize) -> Vec<u8> {
    if len_size == 0 {
        return data.to_vec();
    }
    let start_code: &[u8] = &[0x00, 0x00, 0x00, 0x01];
    let mut output = Vec::with_capacity(data.len() + 64);
    let mut offset = 0;
    while offset + len_size <= data.len() {
        let mut nalu_len = 0usize;
        for _ in 0..len_size {
            nalu_len = (nalu_len << 8) | data[offset] as usize;
            offset += 1;
        }
        if nalu_len == 0 || offset + nalu_len > data.len() {
            continue;
        }
        output.extend_from_slice(start_code);
        output.extend_from_slice(&data[offset..offset + nalu_len]);
        offset += nalu_len;
    }
    if output.is_empty() {
        data.to_vec()
    } else {
        output
    }
}

pub fn avcc_to_annexb(data: &[u8]) -> Vec<u8> {
    avcc_to_annexb_with_len(data, 4)
}

pub(crate) fn has_annexb_start_code(data: &[u8]) -> bool {
    if data.len() >= 4 && data[0..4] == [0x00, 0x00, 0x00, 0x01] {
        true
    } else if data.len() >= 3 && data[0..3] == [0x00, 0x00, 0x01] {
        true
    } else {
        false
    }
}

pub fn yuv420_uv_to_nv12(u_plane: &[u8], v_plane: &[u8], width: u32, height: u32) -> Vec<u8> {
    let uv_size = (width.div_ceil(2) * height.div_ceil(2)) as usize;
    let mut uv = Vec::with_capacity(uv_size * 2);
    for i in 0..uv_size {
        uv.push(u_plane.get(i).copied().unwrap_or(128));
        uv.push(v_plane.get(i).copied().unwrap_or(128));
    }
    uv
}
