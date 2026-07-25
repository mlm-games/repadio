use std::fs::File;
use std::time::Duration;

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::video::well_known::{CODEC_ID_HEVC, extra_data as ed_ids};
use symphonia::core::formats::{FormatOptions, probe::Hint};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

use player_core::video::VideoDecoder;

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

fn pts_s_to_dbg(pts: Duration) -> String {
    format!("{:.6}s", pts.as_secs_f64())
}

#[test]
fn test_pts_monotonic() {
    let path = "/home/ymsr/Videos/Big_Buck_Bunny_720_10s_1MB-h265.mp4";
    let file = File::open(path).expect("open mp4");
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());

    let hint = Hint::new();

    let mut reader = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .expect("probe");

    let tracks = reader.tracks().to_vec();
    let video_track = tracks
        .iter()
        .find(|t| match &t.codec_params {
            Some(CodecParameters::Video(vp)) => vp.codec == CODEC_ID_HEVC,
            _ => false,
        })
        .cloned()
        .expect("no H.265 track");

    let vp = match &video_track.codec_params {
        Some(CodecParameters::Video(vp)) => vp.clone(),
        _ => unreachable!(),
    };

    let width = vp.width.unwrap_or(0) as u32;
    let height = vp.height.unwrap_or(0) as u32;
    let extradata = vp
        .extra_data
        .iter()
        .find(|ed| ed.id == ed_ids::VIDEO_EXTRA_DATA_ID_HEVC_DECODER_CONFIG)
        .or_else(|| vp.extra_data.first())
        .map(|ed| ed.data.to_vec())
        .unwrap_or_default();
    let time_base = video_track.time_base.unwrap();
    let track_id = video_track.id;

    eprintln!(
        "Video track {}: {}x{}, time_base={}/{}",
        track_id, width, height, time_base.numer, time_base.denom
    );

    let mut decoder =
        VideoDecoder::new_hevc(width, height, &extradata).expect("create H.265 decoder");

    let mut gcd_pts_ticks: u64 = 0;
    let mut non_zero_pts_seen: u64 = 0;
    let tb_numer = time_base.numer.get() as u64;
    let tb_denom = time_base.denom.get() as u64;
    let mut frame_duration_us = 0u64;

    let mut output_frames: Vec<(usize, Duration, Option<i32>)> = Vec::new();
    let mut packet_count = 0;
    let mut prev_pts: Option<Duration> = None;
    let mut pts_dips = 0usize;
    let mut prev_poc: Option<i32> = None;
    let mut poc_non_monotonic = 0usize;
    let mut drain_batch_sizes: Vec<usize> = Vec::new();

    loop {
        let packet = match reader.next_packet() {
            Ok(Some(pkt)) => pkt,
            Ok(None) => break,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(symphonia::core::errors::Error::ResetRequired) => break,
            Err(e) => panic!("demux error: {e:?}"),
        };

        if packet.track_id != track_id {
            continue;
        }

        packet_count += 1;
        let pts_us = {
            let time = time_base.calc_time_saturating(packet.pts);
            (time.as_secs_f64() * 1_000_000.0) as i64
        };
        let fallback_pts = Duration::from_micros(pts_us.max(0) as u64);

        let pts_ticks = packet.pts.get();
        if pts_ticks > 0 {
            if gcd_pts_ticks == 0 {
                gcd_pts_ticks = pts_ticks as u64;
            } else {
                gcd_pts_ticks = gcd(gcd_pts_ticks, pts_ticks as u64);
            }
            non_zero_pts_seen += 1;
        }
        if non_zero_pts_seen >= 2 {
            let new_fd = (gcd_pts_ticks * tb_numer * 1_000_000) / tb_denom;
            if new_fd != frame_duration_us {
                frame_duration_us = new_fd;
                decoder.set_frame_duration_micros(frame_duration_us);
            }
        }

        let is_sync = packet
            .data
            .first()
            .map(|&b| ((b >> 1) & 0x3F) == 19)
            .unwrap_or(false);

        decoder
            .send_packet(&packet.data, pts_us, is_sync)
            .expect("send_packet");

        let fd = if non_zero_pts_seen >= 2 {
            frame_duration_us
        } else {
            0
        };
        let frames = decoder.drain_frames(fallback_pts, 0, fd);

        drain_batch_sizes.push(frames.len());

        for f in &frames {
            let global_idx = output_frames.len();
            if let Some(prev) = prev_pts {
                if f.pts < prev && f.pts.abs_diff(prev) > Duration::from_micros(100) {
                    pts_dips += 1;
                    eprintln!(
                        "PTS DIP #{}: prev={} -> curr={} (Δ={:+.6}s)  frame#{} POC={:?}",
                        pts_dips,
                        pts_s_to_dbg(prev),
                        pts_s_to_dbg(f.pts),
                        f.pts.as_secs_f64() - prev.as_secs_f64(),
                        global_idx,
                        f.poc,
                    );
                }
            }
            if let Some(prev) = prev_poc {
                if let Some(poc) = f.poc {
                    if poc < prev {
                        poc_non_monotonic += 1;
                        eprintln!("POC dip: frame#{} POC {} < prev {}", global_idx, poc, prev);
                    }
                }
            }
            if let Some(poc) = f.poc {
                prev_poc = Some(poc);
            }
            prev_pts = Some(f.pts);
            output_frames.push((global_idx, f.pts, f.poc));
        }

        if packet_count % 50 == 0 {
            let elapsed = output_frames.last().map(|f| f.1).unwrap_or(Duration::ZERO);
            eprintln!(
                "  progress: packet {} -> {} output frames, PTS={:.3}s",
                packet_count,
                output_frames.len(),
                elapsed.as_secs_f64(),
            );
        }
    }

    let finish_fd = if non_zero_pts_seen >= 2 {
        frame_duration_us
    } else {
        0
    };
    let final_frames = decoder.finish(finish_fd).expect("finish");
    for f in &final_frames {
        let global_idx = output_frames.len();
        if let Some(prev) = prev_pts {
            if f.pts < prev && f.pts.abs_diff(prev) > Duration::from_micros(100) {
                pts_dips += 1;
                eprintln!(
                    "PTS DIP (finish) #{}: prev={} -> curr={} (Δ={:+.6}s)  frame#{} POC={:?}",
                    pts_dips,
                    pts_s_to_dbg(prev),
                    pts_s_to_dbg(f.pts),
                    f.pts.as_secs_f64() - prev.as_secs_f64(),
                    global_idx,
                    f.poc,
                );
            }
        }
        if let Some(prev) = prev_poc {
            if let Some(poc) = f.poc {
                if poc < prev {
                    poc_non_monotonic += 1;
                    eprintln!(
                        "POC dip (finish): frame#{} POC {} < prev {}",
                        global_idx, poc, prev
                    );
                }
            }
        }
        if let Some(poc) = f.poc {
            prev_poc = Some(poc);
        }
        prev_pts = Some(f.pts);
        output_frames.push((global_idx, f.pts, f.poc));
    }

    let max_batch = drain_batch_sizes.iter().copied().max().unwrap_or(0);
    let batches_gt1 = drain_batch_sizes.iter().filter(|&&s| s > 1).count();
    eprintln!(
        "\nSent {} video packets, got {} output frames, {} PTS dips, {} POC dips",
        packet_count,
        output_frames.len(),
        pts_dips,
        poc_non_monotonic,
    );
    eprintln!(
        "Drain batch sizes: max={}, count>1={}, distribution: {:?}",
        max_batch,
        batches_gt1,
        {
            let mut hist = std::collections::BTreeMap::new();
            for &s in &drain_batch_sizes {
                *hist.entry(s).or_insert(0) += 1;
            }
            hist
        },
    );

    // Print first 10 and last 10 frames for inspection
    eprintln!("\nFirst 10 output frames:");
    for (i, (global_idx, pts, poc)) in output_frames.iter().take(10).enumerate() {
        eprintln!(
            "  output[{}] (frame#{})  PTS={}  POC={:?}",
            i,
            global_idx,
            pts_s_to_dbg(*pts),
            poc,
        );
    }
    if output_frames.len() > 20 {
        eprintln!("  ...");
        eprintln!("Last 10 output frames:");
        for (i, (global_idx, pts, poc)) in output_frames[output_frames.len().saturating_sub(10)..]
            .iter()
            .enumerate()
        {
            eprintln!(
                "  output[{}] (frame#{})  PTS={}  POC={:?}",
                output_frames.len() - 10 + i,
                global_idx,
                pts_s_to_dbg(*pts),
                poc,
            );
        }
    }

    // Raw sequence check for monotonic PTS (ignoring the 100µs threshold)
    let mut raw_dips = 0;
    for chunk in output_frames.windows(2) {
        if chunk[1].1 < chunk[0].1 {
            raw_dips += 1;
        }
    }
    if raw_dips > 0 {
        eprintln!(
            "PTS not strictly monotonic: {} dips in raw sequence (sub-100µs ignored by threshold)",
            raw_dips
        );
    }

    assert!(
        pts_dips == 0,
        "Found {pts_dips} PTS dips. PTS is NOT monotonic in decoder output"
    );
    assert!(
        poc_non_monotonic == 0,
        "Found {poc_non_monotonic} POC dips. POC is NOT monotonic"
    );
}
