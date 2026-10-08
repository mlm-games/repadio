use std::fs::File;

use web_time::Duration;

use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::{FormatOptions, probe::Hint};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

use player_core::video::{
    VideoDecoder, VideoDecoderPrefs, avcc_to_annexb_with_len, parse_nal_length_size,
};
use rediakit_bitstream::sync::h264_avcc_has_idr;

/// The two regressions this file is a golden case for:
///
/// * a packet whose AVCC length prefix reads as an Annex-B start code must
///   still be reframed, and
/// * a stream with B-pictures must leave the decoder in display order.
#[test]
fn bframe_stream_stays_in_display_order() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/bframes_single_gop_720p.mp4");
    let Some(path) = path.exists().then_some(path) else {
        eprintln!("fixture missing, skipping");
        return;
    };
    let file = File::open(&path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut reader = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .unwrap();
    let track = reader
        .tracks()
        .iter()
        .find(|t| matches!(&t.codec_params, Some(CodecParameters::Video(_))))
        .cloned()
        .unwrap();
    let vp = match &track.codec_params {
        Some(CodecParameters::Video(vp)) => vp.clone(),
        _ => unreachable!(),
    };
    let extradata = vp
        .extra_data
        .first()
        .map(|e| e.data.to_vec())
        .unwrap_or_default();
    let nls = parse_nal_length_size(&extradata) as usize;
    let tb = track.time_base.unwrap();

    let mut dec = VideoDecoder::new_h264(
        vp.width.unwrap_or(0) as u32,
        vp.height.unwrap_or(0) as u32,
        &extradata,
        VideoDecoderPrefs {
            try_hw: false,
            allow_sw: true,
        },
    )
    .unwrap();

    let mut pts = Vec::new();
    while let Ok(Some(packet)) = reader.next_packet() {
        if packet.track_id != track.id {
            continue;
        }
        let pts_us = {
            let t = tb.calc_time_saturating(packet.pts);
            (t.as_secs_f64() * 1e6) as i64
        };
        dec.send_packet(
            &packet.data,
            pts_us,
            h264_avcc_has_idr(&packet.data, nls),
            0,
        )
        .unwrap();
        pts.extend(
            dec.drain_frames(Duration::from_micros(pts_us.max(0) as u64), 0)
                .into_iter()
                .map(|f| f.pts),
        );
    }
    if let Ok(rest) = dec.finish() {
        pts.extend(rest.into_iter().map(|f| f.pts));
    }

    assert!(
        pts.len() > 100,
        "expected a real frame count, got {}",
        pts.len()
    );
    for pair in pts.windows(2) {
        assert!(
            pair[1] >= pair[0],
            "frames left the decoder out of display order: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }
}

/// A hardware decoder is handed Annex-B, so every packet has to be reframed
/// even when it already *looks* like Annex-B.
///
/// An AVCC packet opens with its first NAL's length. Whenever that length is
/// `0x000001xx` the packet begins `00 00 01`, which is byte-for-byte a
/// three-byte start code — so a "does this already start with a start code?"
/// test mistakes it for Annex-B and passes the length-prefixed bytes straight
/// through. The decoder then reads the top length byte as a NAL header. This
/// file trips that on 36 of its 192 packets.
#[test]
fn avcc_packets_are_always_reframed() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/bframes_single_gop_720p.mp4");
    let Some(path) = path.exists().then_some(path) else {
        eprintln!("fixture missing, skipping");
        return;
    };
    let file = File::open(&path).unwrap();
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut reader = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .unwrap();
    let track = reader
        .tracks()
        .iter()
        .find(|t| matches!(&t.codec_params, Some(CodecParameters::Video(_))))
        .cloned()
        .unwrap();
    let extradata = match &track.codec_params {
        Some(CodecParameters::Video(vp)) => vp
            .extra_data
            .first()
            .map(|e| e.data.to_vec())
            .unwrap_or_default(),
        _ => unreachable!(),
    };
    let nls = parse_nal_length_size(&extradata) as usize;

    let mut deceptive = 0usize;
    let mut packets = 0usize;
    while let Ok(Some(packet)) = reader.next_packet() {
        if packet.track_id != track.id {
            continue;
        }
        packets += 1;
        // A length prefix of 0x000001xx renders as a start code.
        if packet.data.starts_with(&[0x00, 0x00, 0x01]) {
            deceptive += 1;
        }

        let annexb = avcc_to_annexb_with_len(&packet.data, nls);
        // Every Annex-B start code must be followed by a NAL header with a
        // clear forbidden_zero_bit and a type this profile does not use.
        let mut i = 0;
        while i + 4 <= annexb.len() {
            let start = match annexb[i..].windows(4).position(|w| w == [0, 0, 0, 1]) {
                Some(p) => i + p,
                None => break,
            };
            let header = annexb[start + 4];
            assert_eq!(
                header & 0x80,
                0,
                "packet {packets}: NAL header 0x{header:02x} has forbidden_zero_bit set \
                 (start code at {start})"
            );
            assert_ne!(
                header & 0x1F,
                20,
                "packet {packets}: NAL header 0x{header:02x} reads as the reserved slice \
                 extension type (start code at {start})"
            );
            i = start + 4;
        }
    }
    assert!(
        packets > 100,
        "expected the whole fixture, got {packets} packets"
    );
    assert!(
        deceptive > 0,
        "fixture no longer exercises length prefixes that mimic a start code"
    );
}
