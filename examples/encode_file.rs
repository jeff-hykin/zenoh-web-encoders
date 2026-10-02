//! Encodes raw RGB frames (or a moving test pattern) with one backend and writes the H.264 stream, to try an encoder
//! on a machine (e.g. a Jetson) and to measure its quality offline.
//! `cargo run --release --features gstreamer --example encode_file -- gstreamer out.h264 1280 720 8000000 60 [in.rgb] [frames]`
//! Backends: software, videotoolbox, gstreamer (the first hardware element that works), or a GStreamer element name.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::time::Instant;
use zenoh_web::{DecodedFrame, VideoEncoder, VideoImage, VideoTarget};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [backend, out, width, height, bitrate, fps] = &args[..6] else { anyhow::bail!("usage: BACKEND OUT.h264 WIDTH HEIGHT BITRATE FPS [IN.rgb] [FRAMES]") };
    let (width, height, bitrate, fps): (u32, u32, u32, f64) = (width.parse()?, height.parse()?, bitrate.parse()?, fps.parse()?);
    let frame_len = (width * height * 3) as usize;
    let input = match args.get(6) {
        Some(path) => {
            let mut bytes = Vec::new();
            std::fs::File::open(path).with_context(|| path.clone())?.read_to_end(&mut bytes)?;
            bytes
        }
        None => Vec::new(),
    };
    let frames = args.get(7).map(|count| count.parse()).transpose()?.unwrap_or(if input.is_empty() { 300 } else { input.len() / frame_len });
    let mut encoder: Box<dyn VideoEncoder> = match backend.as_str() {
        "software" => Box::new(zenoh_web::H264Encoder::default()),
        #[cfg(all(feature = "videotoolbox", target_os = "macos"))]
        "videotoolbox" => Box::new(zenoh_web_encoders::videotoolbox::VideoToolboxEncoder::default()),
        #[cfg(feature = "gstreamer")]
        "gstreamer" => Box::new(zenoh_web_encoders::gstreamer::GstreamerEncoder::hardware()?),
        #[cfg(feature = "gstreamer")]
        element => Box::new(zenoh_web_encoders::gstreamer::GstreamerEncoder::with_element(element)?),
        #[cfg(not(feature = "gstreamer"))]
        other => anyhow::bail!("{other} is not built in"),
    };
    let mut file = std::fs::File::create(out)?;
    let (mut total_ms, mut total_bytes, mut keyframes, mut produced) = (0.0, 0usize, 0, 0);
    for index in 0..frames {
        let pixels = if input.is_empty() {
            // a gradient that pans 4 px per frame, with a checker so motion and detail cost bits
            (0..height).flat_map(|y| (0..width).flat_map(move |x| {
                let (u, v) = ((x + index as u32 * 4) % 512, y % 512);
                let check = if (u / 32 + v / 32) % 2 == 0 { 40 } else { 0 };
                [(u / 2) as u8 + check, (v / 2) as u8, ((u + v) / 4) as u8]
            })).collect()
        } else {
            input[index * frame_len..(index + 1) * frame_len].to_vec()
        };
        let frame = DecodedFrame::Video(VideoImage::rgb8(width, height, pixels)?);
        let started = Instant::now();
        let encoded = encoder.encode(&frame, &VideoTarget::new(width, height, bitrate, fps))?;
        total_ms += started.elapsed().as_secs_f64() * 1000.0;
        if let Some(encoded) = encoded {
            file.write_all(&encoded.data)?;
            total_bytes += encoded.data.len();
            keyframes += encoded.keyframe as usize;
            produced += 1;
        }
    }
    println!("{backend}: {frames} frames in, {produced} out ({keyframes} keyframes), {:.2} ms/frame, {:.2} Mbit/s at {fps} fps", total_ms / frames as f64, total_bytes as f64 * 8.0 / (produced.max(1) as f64 / fps) / 1e6);
    Ok(())
}
