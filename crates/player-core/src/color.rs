use oxideav_bitstream::h264::ebsp_to_rbsp;
use oxideav_h264 as h264;
use repose_core::color::{ChromaSiting, ColorInfo, ColorRange, MatrixCoeffs, Primaries, Transfer};

/// Colour used when the stream carries no colour description.
///
/// H.264 §E.3.1 and H.265 §E.3.1 infer CICP code 2 ("unspecified") for every
/// absent field, and ffmpeg then decodes BT.601 limited range with no transfer
/// conversion. `Transfer::Srgb` decodes sRGB in the shader and the sRGB render
/// target re-encodes it, so it is the same no-op.
pub fn fallback_color_info() -> ColorInfo {
    ColorInfo {
        range: ColorRange::Limited,
        matrix: MatrixCoeffs::Bt601,
        primaries: Primaries::Bt709,
        transfer: Transfer::Srgb,
        chroma_siting: ChromaSiting::Left,
    }
}

fn matrix_from_cicp(code: u8) -> Option<MatrixCoeffs> {
    Some(match code {
        0 => MatrixCoeffs::Identity,
        1 | 8 | 12 => MatrixCoeffs::Bt709,
        4..=7 => MatrixCoeffs::Bt601,
        10 | 11 => MatrixCoeffs::Bt2020Ncl,
        _ => return None,
    })
}

fn primaries_from_cicp(code: u8) -> Option<Primaries> {
    Some(match code {
        1 => Primaries::Bt709,
        4 | 7 | 8 => Primaries::Bt601_525,
        5 | 6 | 22 => Primaries::Bt601_625,
        10 => Primaries::Bt2020,
        12 | 13 | 14 => Primaries::DciP3,
        _ => return None,
    })
}

fn transfer_from_cicp(code: u8) -> Option<Transfer> {
    Some(match code {
        1 | 4 | 5 | 6 | 7 | 14 | 15 => Transfer::Bt709,
        8 => Transfer::Linear,
        11 | 12 | 13 => Transfer::Srgb,
        16 => Transfer::Pq,
        18 => Transfer::Hlg,
        _ => return None,
    })
}

/// Builds `ColorInfo` from an H.264/H.265 colour description. `colour` is the
/// `(primaries, transfer, matrix)` triple, `None` when
/// `colour_description_present_flag` is 0. Unsupported codes keep the fallback.
fn signalled(colour: Option<(u8, u8, u8)>, full_range: bool) -> ColorInfo {
    let mut info = fallback_color_info();
    info.range = if full_range {
        ColorRange::Full
    } else {
        ColorRange::Limited
    };
    if let Some((primaries, transfer, matrix)) = colour {
        if let Some(v) = primaries_from_cicp(primaries) {
            info.primaries = v;
        }
        if let Some(v) = transfer_from_cicp(transfer) {
            info.transfer = v;
        }
        if let Some(v) = matrix_from_cicp(matrix) {
            info.matrix = v;
        }
    }
    info
}

/// Colour described by an `AVCDecoderConfigurationRecord` (ISO/IEC 14496-15
/// §5.3.3.1): the SPS `vui_parameters()` of the first carried SPS.
pub fn avcc_color_info(data: &[u8]) -> ColorInfo {
    avcc_colour(data).unwrap_or_else(fallback_color_info)
}

fn avcc_colour(data: &[u8]) -> Option<ColorInfo> {
    if data.len() < 7 || data[0] != 1 {
        return None;
    }
    let num_sps = usize::from(data[5] & 0x1F);
    let mut pos = 6;
    for _ in 0..num_sps {
        let len = usize::from(u16::from_be_bytes([*data.get(pos)?, *data.get(pos + 1)?]));
        pos += 2;
        let nal = data.get(pos..pos.checked_add(len)?)?;
        pos += len;
        if nal.is_empty() || nal[0] & 0x1F != 7 {
            continue;
        }
        let rbsp = ebsp_to_rbsp(nal);
        let signal = h264::sps::Sps::parse(rbsp.get(1..)?)
            .ok()?
            .vui?
            .video_signal_type?;
        let d = signal.colour_description?;
        return Some(signalled(
            Some((
                d.colour_primaries,
                d.transfer_characteristics,
                d.matrix_coefficients,
            )),
            signal.video_full_range_flag,
        ));
    }
    None
}

/// Colour described by an `HEVCDecoderConfigurationRecord` (ISO/IEC 14496-15
/// §8.3.3.1.2): the VUI of the carried SPS (NAL type 33).
pub fn hvcc_color_info(data: &[u8]) -> ColorInfo {
    hvcc_colour(data).unwrap_or_else(fallback_color_info)
}

fn hvcc_colour(data: &[u8]) -> Option<ColorInfo> {
    let record = oxideav_h265::hvcc::parse_hvcc(data).ok()?;
    let sps = record
        .nal_units
        .iter()
        .find(|n| n.header.nal_unit_type == 33)?;
    let sps = oxideav_h265::sps::SeqParameterSet::parse(&sps.rbsp).ok()?;
    let signal = sps
        .vui_parameters
        .as_ref()
        .and_then(|v| v.video_signal_type);
    let colour = signal.as_ref().and_then(|s| {
        s.colour_description.as_ref().map(|c| {
            (
                c.colour_primaries,
                c.transfer_characteristics,
                c.matrix_coeffs,
            )
        })
    });
    Some(signalled(
        colour,
        signal.is_some_and(|s| s.video_full_range_flag),
    ))
}

/// Colour reported by a software decoder on the frame itself. videoson leaves
/// `ColorInfo::default()` (all zeroes, `full_range = false`) when the stream
/// signals nothing, so that shape stays on the fallback.
pub fn videoson_color_info(color: videoson::ColorInfo) -> Option<ColorInfo> {
    if color.primaries == 0 && color.transfer == 0 && color.matrix == 0 && !color.full_range {
        return None;
    }
    Some(signalled(
        Some((color.primaries, color.transfer, color.matrix)),
        color.full_range,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_bitstream::bit_writer::BitWriter;

    const HVCC: &[u8] = &[
        0x01, 0x01, 0x60, 0x00, 0x00, 0x00, 0x90, 0x00, 0x00, 0x00, 0x00, 0x00, 0x5d, 0xf0, 0x00,
        0xfc, 0xfd, 0xf8, 0xf8, 0x00, 0x00, 0x0f, 0x01, 0x21, 0x00, 0x01, 0x00, 0x29, 0x42, 0x01,
        0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00,
        0x5d, 0xa0, 0x02, 0x80, 0x80, 0x2d, 0x16, 0x59, 0x59, 0xa4, 0x93, 0x2b, 0xc0, 0x40, 0x40,
        0x00, 0x00, 0x03, 0x00, 0x40, 0x00, 0x00, 0x07, 0x82,
    ];

    fn h264_sps(full_range: bool, colour: Option<(u8, u8, u8)>) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.write_bits(66, 8); // profile_idc: Baseline, so no high-profile block
        w.write_bits(0, 8); // constraint_set0..5_flag + reserved_zero_2bits
        w.write_bits(40, 8); // level_idc
        w.write_ue(0).unwrap(); // seq_parameter_set_id
        w.write_ue(0).unwrap(); // log2_max_frame_num_minus4
        w.write_ue(0).unwrap(); // pic_order_cnt_type
        w.write_ue(0).unwrap(); // log2_max_pic_order_cnt_lsb_minus4
        w.write_ue(1).unwrap(); // max_num_ref_frames
        w.write_bit(0); // gaps_in_frame_num_value_allowed_flag
        w.write_ue(79).unwrap(); // pic_width_in_mbs_minus1
        w.write_ue(44).unwrap(); // pic_height_in_map_units_minus1
        w.write_bit(1); // frame_mbs_only_flag
        w.write_bit(1); // direct_8x8_inference_flag
        w.write_bit(0); // frame_cropping_flag
        w.write_bit(1); // vui_parameters_present_flag
        w.write_bit(0); // aspect_ratio_info_present_flag
        w.write_bit(0); // overscan_info_present_flag
        w.write_bit(1); // video_signal_type_present_flag
        w.write_bits(5, 3); // video_format
        w.write_bit(u32::from(full_range));
        w.write_bit(u32::from(colour.is_some()));
        if let Some((p, t, m)) = colour {
            w.write_bits(u32::from(p), 8);
            w.write_bits(u32::from(t), 8);
            w.write_bits(u32::from(m), 8);
        }
        w.write_bit(0); // chroma_loc_info_present_flag
        w.write_bit(0); // timing_info_present_flag
        w.write_bit(0); // nal_hrd_parameters_present_flag
        w.write_bit(0); // vcl_hrd_parameters_present_flag
        w.write_bit(0); // pic_struct_present_flag
        w.write_bit(0); // bitstream_restriction_flag
        w.finish()
    }

    fn avcc(sps: &[u8]) -> Vec<u8> {
        let mut nal = vec![0x67];
        nal.extend_from_slice(sps);
        let mut out = vec![1, 66, 0, 40, 0xFF, 0xE1];
        out.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        out.extend_from_slice(&nal);
        out
    }

    #[test]
    fn avcc_signalled_colour() {
        let info = avcc_color_info(&avcc(&h264_sps(true, Some((10, 16, 1)))));
        assert_eq!(info.range, ColorRange::Full);
        assert_eq!(info.matrix, MatrixCoeffs::Bt709);
        assert_eq!(info.primaries, Primaries::Bt2020);
        assert_eq!(info.transfer, Transfer::Pq);
        assert_eq!(info.chroma_siting, ChromaSiting::Left);
    }

    #[test]
    fn avcc_absent_colour_description_is_fallback() {
        assert_eq!(
            avcc_color_info(&avcc(&h264_sps(false, None))),
            fallback_color_info()
        );
    }

    #[test]
    fn unsupported_matrix_keeps_fallback() {
        let info = avcc_color_info(&avcc(&h264_sps(true, Some((2, 2, 9)))));
        assert_eq!(info.range, ColorRange::Full);
        assert_eq!(info.matrix, MatrixCoeffs::Bt601);
    }

    #[test]
    fn hvcc_extradata_reaches_the_vui() {
        assert_eq!(hvcc_colour(HVCC), Some(fallback_color_info()));
    }

    #[test]
    fn videoson_unset_stays_on_fallback() {
        assert_eq!(videoson_color_info(videoson::ColorInfo::default()), None);

        let info = videoson_color_info(videoson::ColorInfo {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: true,
        })
        .unwrap();
        assert_eq!(info.range, ColorRange::Full);
        assert_eq!(info.matrix, MatrixCoeffs::Bt709);
        assert_eq!(info.primaries, Primaries::Bt709);
        assert_eq!(info.transfer, Transfer::Bt709);
    }
}
