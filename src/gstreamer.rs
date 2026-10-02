//! H.264 through a GStreamer hardware encoder, found at runtime: `nvv4l2h264enc` (Jetson), `nvh264enc` (NVENC),
//! `vah264enc` / `vaapih264enc` (VAAPI). GStreamer is loaded with `dlopen`, so building needs no GStreamer and a
//! machine without it just falls back to software. One pipeline per picture size: `appsrc ! [upload] ! encoder !
//! h264parse ! appsink`, constant bitrate, no B-frames, bitrate changes in place, keyframes on request.

use anyhow::{Context, Result, anyhow, bail, ensure};
use libloading::Library;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr::null_mut;
use std::sync::OnceLock;
use std::time::Duration;
use zenoh_web::{DecodedFrame, EncodedVideo, VideoEncoder, VideoFormat, VideoTarget};

type Pointer = *mut c_void;

/// The GStreamer C functions this backend calls (all ABI-stable since 1.10).
struct Api {
    init_check: unsafe extern "C" fn(*mut c_int, *mut c_void, *mut *mut GError) -> c_int,
    parse_launch: unsafe extern "C" fn(*const c_char, *mut *mut GError) -> Pointer,
    bin_get_by_name: unsafe extern "C" fn(Pointer, *const c_char) -> Pointer,
    element_factory_find: unsafe extern "C" fn(*const c_char) -> Pointer,
    element_set_state: unsafe extern "C" fn(Pointer, c_int) -> c_int,
    element_send_event: unsafe extern "C" fn(Pointer, Pointer) -> c_int,
    element_get_bus: unsafe extern "C" fn(Pointer) -> Pointer,
    bus_pop_filtered: unsafe extern "C" fn(Pointer, c_int) -> Pointer,
    message_parse_error: unsafe extern "C" fn(Pointer, *mut *mut GError, *mut *mut c_char),
    mini_object_unref: unsafe extern "C" fn(Pointer),
    object_unref: unsafe extern "C" fn(Pointer),
    util_set_object_arg: unsafe extern "C" fn(Pointer, *const c_char, *const c_char),
    buffer_new_allocate: unsafe extern "C" fn(Pointer, usize, Pointer) -> Pointer,
    buffer_fill: unsafe extern "C" fn(Pointer, usize, *const c_void, usize) -> usize,
    buffer_get_size: unsafe extern "C" fn(Pointer) -> usize,
    buffer_extract: unsafe extern "C" fn(Pointer, usize, *mut c_void, usize) -> usize,
    sample_get_buffer: unsafe extern "C" fn(Pointer) -> Pointer,
    app_src_push_buffer: unsafe extern "C" fn(Pointer, Pointer) -> c_int,
    app_sink_try_pull_sample: unsafe extern "C" fn(Pointer, u64) -> Pointer,
    force_key_unit: unsafe extern "C" fn(u64, u64, u64, c_int, u32) -> Pointer,
    class_find_property: unsafe extern "C" fn(Pointer, *const c_char) -> Pointer,
    error_free: unsafe extern "C" fn(*mut GError),
    free: unsafe extern "C" fn(Pointer),
    _libraries: Vec<Library>,
}

#[repr(C)]
struct GError {
    domain: u32,
    code: c_int,
    message: *const c_char,
}

const STATE_NULL: c_int = 1;
const STATE_PLAYING: c_int = 4;
const MESSAGE_ERROR: c_int = 1 << 1;
const CLOCK_TIME_NONE: u64 = u64::MAX;

/// Opens the first of `names` that loads.
fn open(names: &[&str]) -> Result<Library> {
    let mut errors = Vec::new();
    for name in names {
        // SAFETY: GStreamer's libraries have no load-time side effects beyond their own initializers
        match unsafe { Library::new(*name) } {
            Ok(library) => return Ok(library),
            Err(error) => errors.push(error.to_string()),
        }
    }
    bail!("{}", errors.join("; "))
}

impl Api {
    fn load() -> Result<Api> {
        let names = |base: &str| [format!("lib{base}-1.0.so.0"), format!("lib{base}-1.0.0.dylib"), format!("lib{base}-1.0.dylib")];
        let load = |base: &str| open(&names(base).iter().map(String::as_str).collect::<Vec<_>>());
        let (core, app, video) = (load("gstreamer")?, load("gstapp")?, load("gstvideo")?);
        let gobject = open(&["libgobject-2.0.so.0", "libgobject-2.0.0.dylib"])?;
        let glib = open(&["libglib-2.0.so.0", "libglib-2.0.0.dylib"])?;
        // SAFETY: each symbol is looked up by its C name and given its documented C signature
        unsafe {
            macro_rules! symbol {
                ($library:expr, $name:literal) => {
                    *$library.get(concat!($name, "\0").as_bytes()).with_context(|| concat!("GStreamer has no ", $name))?
                };
            }
            Ok(Api {
                init_check: symbol!(core, "gst_init_check"),
                parse_launch: symbol!(core, "gst_parse_launch"),
                bin_get_by_name: symbol!(core, "gst_bin_get_by_name"),
                element_factory_find: symbol!(core, "gst_element_factory_find"),
                element_set_state: symbol!(core, "gst_element_set_state"),
                element_send_event: symbol!(core, "gst_element_send_event"),
                element_get_bus: symbol!(core, "gst_element_get_bus"),
                bus_pop_filtered: symbol!(core, "gst_bus_pop_filtered"),
                message_parse_error: symbol!(core, "gst_message_parse_error"),
                mini_object_unref: symbol!(core, "gst_mini_object_unref"),
                object_unref: symbol!(core, "gst_object_unref"),
                util_set_object_arg: symbol!(core, "gst_util_set_object_arg"),
                buffer_new_allocate: symbol!(core, "gst_buffer_new_allocate"),
                buffer_fill: symbol!(core, "gst_buffer_fill"),
                buffer_get_size: symbol!(core, "gst_buffer_get_size"),
                buffer_extract: symbol!(core, "gst_buffer_extract"),
                sample_get_buffer: symbol!(core, "gst_sample_get_buffer"),
                app_src_push_buffer: symbol!(app, "gst_app_src_push_buffer"),
                app_sink_try_pull_sample: symbol!(app, "gst_app_sink_try_pull_sample"),
                force_key_unit: symbol!(video, "gst_video_event_new_downstream_force_key_unit"),
                class_find_property: symbol!(gobject, "g_object_class_find_property"),
                error_free: symbol!(glib, "g_error_free"),
                free: symbol!(glib, "g_free"),
                _libraries: vec![core, app, video, gobject, glib],
            })
        }
    }

    /// A GError's message, freeing it.
    unsafe fn take_error(&self, error: *mut GError) -> String {
        if error.is_null() {
            return "unknown error".into();
        }
        // SAFETY: a GError GStreamer handed us, freed once
        unsafe {
            let message = CStr::from_ptr((*error).message).to_string_lossy().into_owned();
            (self.error_free)(error);
            message
        }
    }
}

/// GStreamer, loaded and initialized once per process.
fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    let api = API.get_or_init(|| {
        let api = Api::load().map_err(|error| format!("{error:#}"))?;
        let mut error = null_mut();
        // SAFETY: gst_init_check with no arguments
        if unsafe { (api.init_check)(null_mut(), null_mut(), &mut error) } == 0 {
            // SAFETY: the error init_check set
            return Err(format!("gst_init: {}", unsafe { api.take_error(error) }));
        }
        Ok(api)
    });
    api.as_ref().map_err(|error| anyhow!("GStreamer: {error}"))
}

/// How to drive one encoder element.
struct Element {
    name: &'static str,
    /// what turns system-memory I420 into what the encoder takes
    upload: &'static str,
    /// its `bitrate` property counts kbit/s (else bit/s)
    kbps: bool,
    /// low-latency constant-bitrate settings (ones the installed version lacks are skipped)
    settings: &'static [(&'static str, &'static str)],
    /// the properties that set the keyframe interval, in frames
    keyframe_interval: &'static [&'static str],
}

/// Hardware encoders, in the order they are tried.
static HARDWARE: [Element; 4] = [
    Element {
        name: "nvv4l2h264enc",
        upload: "nvvidconv ! video/x-raw(memory:NVMM),format=I420 !",
        kbps: false,
        settings: &[("control-rate", "1"), ("insert-sps-pps", "true"), ("insert-vui", "true"), ("maxperf-enable", "true"), ("preset-level", "1"), ("profile", "0"), ("poc-type", "2")],
        keyframe_interval: &["iframeinterval", "idrinterval"],
    },
    Element { name: "nvh264enc", upload: "", kbps: true, settings: &[("rc-mode", "cbr"), ("preset", "low-latency-hp"), ("zerolatency", "true"), ("bframes", "0")], keyframe_interval: &["gop-size"] },
    Element { name: "vah264enc", upload: "videoconvert ! video/x-raw,format=NV12 !", kbps: true, settings: &[("rate-control", "cbr"), ("b-frames", "0"), ("target-usage", "7")], keyframe_interval: &["key-int-max"] },
    Element { name: "vaapih264enc", upload: "videoconvert ! video/x-raw,format=NV12 !", kbps: true, settings: &[("rate-control", "cbr"), ("max-bframes", "0")], keyframe_interval: &["keyframe-period"] },
];
/// Software, for trying the pipeline on a machine without a hardware encoder.
static X264: Element = Element { name: "x264enc", upload: "", kbps: true, settings: &[("tune", "zerolatency"), ("speed-preset", "ultrafast"), ("bframes", "0")], keyframe_interval: &["key-int-max"] };
const KEYFRAME_SECONDS: f64 = 3.0;
/// Frames inside the encoder before `encode` waits for one to come out: hardware encoders take a few frame times
/// (nvv4l2h264enc ~40 ms on a loaded Orin), so waiting for each frame's own output capped a stream at ~25 fps.
const MAX_IN_FLIGHT: u64 = 3;
/// How long that wait may last (the first frame's includes the pipeline's start).
const OUTPUT_TIMEOUT: Duration = Duration::from_secs(3);

fn c_string(text: &str) -> CString {
    CString::new(text).expect("no NUL in GStreamer names")
}

/// A running `appsrc ! … ! appsink` pipeline (owned references, released on drop).
struct Pipeline {
    api: &'static Api,
    pipeline: Pointer,
    source: Pointer,
    sink: Pointer,
    encoder: Pointer,
    element: &'static Element,
    size: (u32, u32),
    fps: f64,
    bitrate_bps: u32,
    pushed: u64,
    pulled: u64,
}

// SAFETY: GStreamer objects are thread-safe; the encoder uses them one call at a time (`&mut self`)
unsafe impl Send for Pipeline {}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // SAFETY: our references, released once
        unsafe {
            (self.api.element_set_state)(self.pipeline, STATE_NULL);
            for object in [self.source, self.sink, self.encoder, self.pipeline] {
                (self.api.object_unref)(object);
            }
        }
    }
}

impl Pipeline {
    fn new(api: &'static Api, element: &'static Element, (width, height): (u32, u32), bitrate_bps: u32, fps: f64) -> Result<Self> {
        // colorimetry 2:4:5:1: limited range, BT.601 matrix, BT.709 transfer and primaries (zenoh-web's pictures)
        let description = format!(
            "appsrc name=source is-live=true do-timestamp=true format=time caps=video/x-raw,format=I420,width={width},height={height},framerate={}/1000,colorimetry=2:4:5:1 ! {} {} name=encoder ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! appsink name=sink sync=false",
            (fps * 1000.0).round().max(1.0) as u64,
            element.upload,
            element.name
        );
        let mut error = null_mut();
        // SAFETY: plain GStreamer calls; every returned reference is owned by the Pipeline
        unsafe {
            let pipeline = (api.parse_launch)(c_string(&description).as_ptr(), &mut error);
            if pipeline.is_null() || !error.is_null() {
                let message = api.take_error(error);
                if !pipeline.is_null() {
                    (api.object_unref)(pipeline);
                }
                bail!("GStreamer pipeline {description:?}: {message}");
            }
            let by_name = |name: &str| (api.bin_get_by_name)(pipeline, c_string(name).as_ptr());
            let mut pipeline = Pipeline { api, pipeline, source: by_name("source"), sink: by_name("sink"), encoder: by_name("encoder"), element, size: (width, height), fps, bitrate_bps: 0, pushed: 0, pulled: 0 };
            ensure!(!pipeline.source.is_null() && !pipeline.sink.is_null() && !pipeline.encoder.is_null(), "GStreamer pipeline lost its elements");
            for (name, value) in element.settings {
                pipeline.set(name, value);
            }
            let interval = ((fps * KEYFRAME_SECONDS).ceil() as u32).max(1).to_string();
            for name in element.keyframe_interval {
                pipeline.set(name, &interval);
            }
            pipeline.set_bitrate(bitrate_bps);
            ensure!((api.element_set_state)(pipeline.pipeline, STATE_PLAYING) != 0, "GStreamer pipeline would not start");
            Ok(pipeline)
        }
    }

    /// Sets an encoder property from its string form, if the installed version has it.
    fn set(&self, name: &str, value: &str) {
        let name = c_string(name);
        // SAFETY: a GObject's first field is its class pointer
        unsafe {
            let class = *(self.encoder as *const Pointer);
            if (self.api.class_find_property)(class, name.as_ptr()).is_null() {
                log::debug!("{}: no property {name:?}", self.element.name);
                return;
            }
            (self.api.util_set_object_arg)(self.encoder, name.as_ptr(), c_string(value).as_ptr());
        }
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) {
        let value = if self.element.kbps { bitrate_bps.div_ceil(1000) } else { bitrate_bps };
        self.set("bitrate", &value.to_string());
        self.bitrate_bps = bitrate_bps;
    }

    /// Fails with the pipeline's error, if it posted one.
    fn check_bus(&self) -> Result<()> {
        // SAFETY: the bus and message references are released here
        unsafe {
            let bus = (self.api.element_get_bus)(self.pipeline);
            let message = (self.api.bus_pop_filtered)(bus, MESSAGE_ERROR);
            (self.api.object_unref)(bus);
            if message.is_null() {
                return Ok(());
            }
            let (mut error, mut debug) = (null_mut(), null_mut());
            (self.api.message_parse_error)(message, &mut error, &mut debug);
            let details = if debug.is_null() { String::new() } else { CStr::from_ptr(debug).to_string_lossy().into_owned() };
            (self.api.free)(debug.cast());
            (self.api.mini_object_unref)(message);
            bail!("GStreamer {}: {} ({details})", self.element.name, self.api.take_error(error))
        }
    }

    /// Pushes one I420 picture and takes the oldest access unit out: at once if one is ready, waiting only once
    /// `MAX_IN_FLIGHT` frames are inside (every access unit comes out, in order, one per call).
    fn encode(&mut self, i420: &[u8], keyframe: bool) -> Result<Option<Vec<u8>>> {
        // SAFETY: buffers and samples are created, handed over or released as GStreamer documents
        unsafe {
            if keyframe {
                let event = (self.api.force_key_unit)(CLOCK_TIME_NONE, CLOCK_TIME_NONE, CLOCK_TIME_NONE, 1, 0);
                (self.api.element_send_event)(self.source, event);
            }
            let buffer = (self.api.buffer_new_allocate)(null_mut(), i420.len(), null_mut());
            ensure!(!buffer.is_null(), "GStreamer: no buffer");
            (self.api.buffer_fill)(buffer, 0, i420.as_ptr().cast(), i420.len());
            let flow = (self.api.app_src_push_buffer)(self.source, buffer);
            ensure!(flow == 0, "GStreamer push: flow {flow}");
            self.pushed += 1;
            let timeout = if self.pushed - self.pulled > MAX_IN_FLIGHT { OUTPUT_TIMEOUT } else { Duration::ZERO };
            let sample = (self.api.app_sink_try_pull_sample)(self.sink, timeout.as_nanos() as u64);
            self.check_bus()?;
            if sample.is_null() {
                return Ok(None);
            }
            self.pulled += 1;
            let out = (self.api.sample_get_buffer)(sample);
            let mut data = vec![0u8; (self.api.buffer_get_size)(out)];
            (self.api.buffer_extract)(out, 0, data.as_mut_ptr().cast(), data.len());
            (self.api.mini_object_unref)(sample);
            Ok(Some(data))
        }
    }
}

/// Whether an Annex B access unit holds an IDR slice (NAL type 5).
fn has_idr(data: &[u8]) -> bool {
    data.windows(4).any(|window| window[..3] == [0, 0, 1] && window[3] & 0x1f == 5)
}

/// H.264 through one GStreamer encoder element; see [`hardware`](GstreamerEncoder::hardware).
pub struct GstreamerEncoder {
    api: &'static Api,
    element: &'static Element,
    pipeline: Option<Pipeline>,
}

impl GstreamerEncoder {
    /// An encoder using the element called `name` (one of the hardware ones, or `x264enc`), if GStreamer has it.
    pub fn with_element(name: &str) -> Result<Self> {
        let api = api()?;
        let element = HARDWARE.iter().chain([&X264]).find(|element| element.name == name).ok_or_else(|| anyhow!("{name} is not an encoder this crate drives"))?;
        // SAFETY: the factory reference is released at once
        unsafe {
            let factory = (api.element_factory_find)(c_string(name).as_ptr());
            ensure!(!factory.is_null(), "GStreamer has no {name}");
            (api.object_unref)(factory);
        }
        Ok(GstreamerEncoder { api, element, pipeline: None })
    }

    /// The first hardware encoder that is installed and encodes a test frame.
    pub fn hardware() -> Result<Self> {
        let mut tried = Vec::new();
        for element in &HARDWARE {
            match GstreamerEncoder::with_element(element.name).and_then(|mut encoder| encoder.probe().map(|()| encoder)) {
                Ok(encoder) => return Ok(GstreamerEncoder { pipeline: None, ..encoder }),
                Err(error) => tried.push(format!("{}: {error:#}", element.name)),
            }
        }
        bail!("no GStreamer hardware H.264 encoder works ({})", tried.join("; "))
    }

    /// The element in use, e.g. `"nvv4l2h264enc"`.
    pub fn element(&self) -> &'static str {
        self.element.name
    }

    fn probe(&mut self) -> Result<()> {
        let image = zenoh_web::VideoImage::i420(320, 240, vec![128; 320 * 240 * 3 / 2])?;
        for _ in 0..=MAX_IN_FLIGHT + 1 {
            if self.encode(&DecodedFrame::Video(image.clone()), &VideoTarget::new(320, 240, 1_000_000, 30.0))?.is_some() {
                return Ok(());
            }
        }
        bail!("no frame came out")
    }
}

impl VideoEncoder for GstreamerEncoder {
    fn format(&self) -> VideoFormat {
        VideoFormat::H264
    }

    fn encode(&mut self, frame: &DecodedFrame, target: &VideoTarget) -> Result<Option<EncodedVideo>> {
        let DecodedFrame::Video(image) = frame else { bail!("{} takes pictures, got {frame:?}", self.element.name) };
        let size = (target.width, target.height);
        // encoders budget each frame from the caps' rate, so a rate that moves far rebuilds the pipeline
        let fresh = self.pipeline.as_ref().is_none_or(|pipeline| pipeline.size != size || (pipeline.fps / target.fps).max(target.fps / pipeline.fps) > 1.3);
        if fresh {
            self.pipeline = Some(Pipeline::new(self.api, self.element, size, target.bitrate_bps, target.fps)?);
        }
        let pipeline = self.pipeline.as_mut().expect("created above");
        if pipeline.bitrate_bps != target.bitrate_bps {
            pipeline.set_bitrate(target.bitrate_bps);
        }
        let i420 = image.to_i420(target.width, target.height)?;
        let encoded = pipeline.encode(i420.data(), target.keyframe && !fresh)?;
        Ok(encoded.map(|data| EncodedVideo { keyframe: has_idr(&data), data, width: target.width, height: target.height }))
    }
}
