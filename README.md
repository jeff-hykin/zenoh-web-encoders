# zenoh-web-encoders

Hardware H.264 encoders for [zenoh-web](https://github.com/jeff-hykin/zenoh-web), as `VideoEncoder`s for
`ServerBuilder::video_encoder`. [zenoh-web-cli](https://github.com/jeff-hykin/zenoh-web-cli) uses them
(`--video-encoder auto|software|videotoolbox|gstreamer`).

| feature | backend | needs |
|---|---|---|
| `videotoolbox` | macOS VideoToolbox (the media engine): constrained baseline, low-latency rate control | macOS (does nothing elsewhere) |
| `gstreamer` | the first GStreamer hardware encoder that works: `nvv4l2h264enc` (Jetson), `nvh264enc` (NVENC), `vah264enc` / `vaapih264enc` (VAAPI) | GStreamer 1.x at runtime only: it is loaded with `dlopen`, so building needs nothing and a machine without it falls back to software |

```rust
let selected = zenoh_web_encoders::select(zenoh_web_encoders::Backend::Auto)?;
if let Some(factory) = selected.factory {
    builder = builder.video_encoder(factory); // else zenoh-web's software H.264
}
```

- `select` probes each backend by encoding a test frame; `Auto` tries VideoToolbox, then GStreamer, then picks
  software. A named backend that doesn't work is an error.
- Each encoder is wrapped in `Fallback`: after its first error it hands over to openh264 for good (a new software
  encoder starts with a keyframe).
- All of them encode at the bitrate zenoh-web grants (changed in place, no keyframe), restart on a new picture size,
  give keyframes on request, and signal zenoh-web's colors (BT.601 matrix, BT.709 primaries and transfer).
- `examples/encode_file.rs` encodes raw RGB frames (or a moving test pattern) with any backend to an `.h264` file, to
  try an encoder on a machine and measure it offline.

Measured on the zenoh-web bench scene (720p60, offline, decoded by ffmpeg): VideoToolbox 3.95 Mbit/s → 29.34 dB,
8.2 → 29.68, 15.4 → 29.90; openh264 4.0 → 29.37, 8.3 → 29.68, 16.2 → 29.88. Same quality per bit, but on the media
engine instead of a core per stream.

On a Jetson AGX Orin (JetPack 6, GStreamer 1.20; under load from the robot's own stack): `nvv4l2h264enc` took 6-14 ms
per 720p frame (three frames in flight) against openh264's 34 ms, hit its bitrate, and scored 30.27 dB on the bench
scene at 16.6 Mbit/s against openh264's 29.57.
