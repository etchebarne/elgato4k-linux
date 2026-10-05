//! Capture device discovery and the GStreamer preview pipelines.
//!
//! Video and audio run in two independent pipelines so a stalled or busy
//! video device never takes the audio monitor down with it (and vice versa).
//! Both are tuned for preview latency rather than A/V sync: the video sink
//! renders frames as soon as they arrive and a leaky queue drops stale ones.

use std::fs;
use std::path::Path;

use gst::glib;
use gst::prelude::*;
use gtk::gdk;

/// Name fragment used to recognise Elgato capture devices.
const ELGATO_NAME: &str = "Elgato";

/// How the device encodes a video mode on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Encoding {
    Mjpeg,
    Raw(String),
}

/// One selectable resolution / framerate / encoding combination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoMode {
    pub encoding: Encoding,
    pub width: i32,
    pub height: i32,
    pub fps: gst::Fraction,
}

impl VideoMode {
    pub fn label(&self) -> String {
        let fps = self.fps.numer() as f64 / self.fps.denom() as f64;
        let fps = if fps.fract() == 0.0 { format!("{fps:.0}") } else { format!("{fps:.2}") };
        let encoding = match &self.encoding {
            Encoding::Mjpeg => "MJPEG".to_string(),
            Encoding::Raw(f) if f == "P010_10LE" => "P010 (10-bit)".to_string(),
            Encoding::Raw(f) => f.clone(),
        };
        format!("{}×{} @ {fps} fps · {encoding}", self.width, self.height)
    }

    fn caps(&self) -> gst::Caps {
        let builder = match &self.encoding {
            Encoding::Mjpeg => gst::Caps::builder("image/jpeg"),
            Encoding::Raw(format) => gst::Caps::builder("video/x-raw").field("format", format),
        };
        builder
            .field("width", self.width)
            .field("height", self.height)
            .field("framerate", self.fps)
            .build()
    }

    fn pixels_per_second(&self) -> f64 {
        self.width as f64 * self.height as f64 * self.fps.numer() as f64 / self.fps.denom() as f64
    }
}

/// A V4L2 capture node belonging to the card.
pub struct VideoSource {
    pub path: String,
    pub modes: Vec<VideoMode>,
}

/// Find the card's V4L2 capture node via sysfs.
///
/// UVC devices expose a second node (index 1) for metadata; only index 0
/// carries video.
pub fn find_video_source() -> Option<VideoSource> {
    let mut entries: Vec<_> = fs::read_dir("/sys/class/video4linux").ok()?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let dir = entry.path();
        let name = read_trimmed(&dir.join("name")).unwrap_or_default();
        let index = read_trimmed(&dir.join("index")).unwrap_or_default();
        if !name.contains(ELGATO_NAME) || index != "0" {
            continue;
        }
        let path = format!("/dev/{}", entry.file_name().to_string_lossy());
        let modes = probe_modes(&path);
        if !modes.is_empty() {
            return Some(VideoSource { path, modes });
        }
    }
    None
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Ask v4l2src which formats the device offers.
///
/// Format enumeration only needs the device in READY, so this works even
/// while another application (e.g. OBS) is streaming from it.
fn probe_modes(path: &str) -> Vec<VideoMode> {
    let Ok(src) = gst::ElementFactory::make("v4l2src").property("device", path).build() else {
        return Vec::new();
    };
    if src.set_state(gst::State::Ready).is_err() {
        let _ = src.set_state(gst::State::Null);
        return Vec::new();
    }
    let caps = src.static_pad("src").map(|pad| pad.query_caps(None));
    let _ = src.set_state(gst::State::Null);

    let mut modes = Vec::new();
    if let Some(caps) = caps {
        for (structure, features) in caps.iter_with_features() {
            // DMABuf variants duplicate the system-memory ones.
            if !features.is_equal(&gst::CapsFeatures::new_empty())
                && !features.contains(gst::CAPS_FEATURE_MEMORY_SYSTEM_MEMORY)
            {
                continue;
            }
            let encodings = match structure.name().as_str() {
                "image/jpeg" => vec![Encoding::Mjpeg],
                "video/x-raw" => strings(structure, "format").into_iter().map(Encoding::Raw).collect(),
                _ => continue,
            };
            for encoding in &encodings {
                for width in ints(structure, "width") {
                    for height in ints(structure, "height") {
                        for fps in fractions(structure, "framerate") {
                            let mode = VideoMode { encoding: encoding.clone(), width, height, fps };
                            if !modes.contains(&mode) {
                                modes.push(mode);
                            }
                        }
                    }
                }
            }
        }
    }

    // MJPEG first (it is what fits through USB at high resolutions), then
    // largest resolution, then highest framerate.
    modes.sort_by(|a, b| {
        let mjpeg = |m: &VideoMode| matches!(m.encoding, Encoding::Mjpeg);
        mjpeg(b)
            .cmp(&mjpeg(a))
            .then((b.width * b.height).cmp(&(a.width * a.height)))
            .then(b.pixels_per_second().total_cmp(&a.pixels_per_second()))
    });
    modes
}

/// The mode to use when the user has not picked one: 1080p60, preferring
/// uncompressed NV12 (no decoding, lowest latency) over MJPEG.
pub fn default_mode(modes: &[VideoMode]) -> usize {
    let is_1080p60 = |m: &VideoMode| {
        m.width == 1920 && m.height == 1080 && (m.pixels_per_second() / (1920.0 * 1080.0) - 60.0).abs() < 0.5
    };
    let nv12 = Encoding::Raw("NV12".into());
    modes
        .iter()
        .position(|m| is_1080p60(m) && m.encoding == nv12)
        .or_else(|| modes.iter().position(|m| is_1080p60(m) && m.encoding == Encoding::Mjpeg))
        .unwrap_or(0)
}

fn values<T: for<'a> glib::value::FromValue<'a> + 'static>(structure: &gst::StructureRef, field: &str) -> Vec<T> {
    let Ok(value) = structure.value(field) else { return Vec::new() };
    if let Ok(v) = value.get::<T>() {
        return vec![v];
    }
    if let Ok(list) = value.get::<gst::List>() {
        return list.iter().filter_map(|v| v.get::<T>().ok()).collect();
    }
    Vec::new()
}

fn ints(s: &gst::StructureRef, field: &str) -> Vec<i32> {
    values::<i32>(s, field)
}

fn strings(s: &gst::StructureRef, field: &str) -> Vec<String> {
    values::<String>(s, field)
}

fn fractions(s: &gst::StructureRef, field: &str) -> Vec<gst::Fraction> {
    values::<gst::Fraction>(s, field)
        .into_iter()
        // The card reports whatever the HDMI source sends, e.g. 59.94 fps as
        // 7013/117, so accept any sane rate rather than only n/1 and n/1001.
        .filter(|f| f.numer() > 0 && f.denom() > 0 && (1.0..=500.0).contains(&(f.numer() as f64 / f.denom() as f64)))
        .collect()
}

/// Find the card's audio capture device through the GStreamer device monitor
/// (PipeWire / PulseAudio).
pub fn find_audio_device() -> Option<gst::Device> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Audio/Source"), None);
    monitor.start().ok()?;
    let devices = monitor.devices();
    monitor.stop();

    let is_elgato = |d: &gst::Device| {
        d.display_name().contains(ELGATO_NAME)
            || d.properties().is_some_and(|p| {
                p.iter().any(|(_, v)| v.get::<String>().is_ok_and(|s| s.contains(ELGATO_NAME)))
            })
    };
    // Monitor sources ("Monitor of …") are sinks in disguise.
    let is_monitor = |d: &gst::Device| d.display_name().starts_with("Monitor of");

    // Go through the sound server: opening the raw ALSA device would take it
    // away from PipeWire (and OBS).
    let factory = |d: &gst::Device| {
        d.create_element(None).ok().and_then(|e| e.factory()).map(|f| f.name().to_string()).unwrap_or_default()
    };
    let candidates: Vec<_> = devices.into_iter().filter(|d| is_elgato(d) && !is_monitor(d)).collect();
    ["pulsesrc", "pipewiresrc"]
        .iter()
        .find_map(|wanted| candidates.iter().find(|d| factory(d) == *wanted).cloned())
}

/// Whether NVIDIA's JPEG decoder (nvcodec plugin) is usable.
pub fn hardware_jpeg_available() -> bool {
    gst::ElementFactory::find("nvjpegdec").is_some() && gst::ElementFactory::find("cudadownload").is_some()
}

// ---------------------------------------------------------------------------
// Video
// ---------------------------------------------------------------------------

/// Builds the GTK sink once; it is reused across pipeline rebuilds so the
/// `gtk::Picture` keeps the same paintable.
pub struct VideoSink {
    pub element: gst::Element,
    pub paintable: gdk::Paintable,
}

impl VideoSink {
    pub fn new() -> Result<Self, glib::BoolError> {
        let gtksink = gst::ElementFactory::make("gtk4paintablesink").build()?;
        let paintable = gtksink.property::<gdk::Paintable>("paintable");

        // With a GL context, colour conversion and upload happen on the GPU.
        let has_gl = paintable.property::<Option<gdk::GLContext>>("gl-context").is_some();
        let element = if has_gl {
            gst::ElementFactory::make("glsinkbin").property("sink", &gtksink).build()?
        } else {
            let bin = gst::Bin::new();
            let convert = gst::ElementFactory::make("videoconvert").build()?;
            bin.add_many([&convert, &gtksink])?;
            convert.link(&gtksink)?;
            let pad = convert.static_pad("sink").expect("videoconvert has a sink pad");
            bin.add_pad(&gst::GhostPad::with_target(&pad)?)?;
            bin.upcast()
        };
        element.set_property("sync", false);
        Ok(Self { element, paintable })
    }
}

pub fn build_video_pipeline(
    device: &str,
    mode: &VideoMode,
    sink: &gst::Element,
    hardware_decode: bool,
) -> Result<gst::Pipeline, glib::BoolError> {
    let pipeline = gst::Pipeline::with_name("video");

    let src = gst::ElementFactory::make("v4l2src").property("device", device).build()?;
    let filter = gst::ElementFactory::make("capsfilter").property("caps", mode.caps()).build()?;
    // Keep at most one frame in flight; drop old ones instead of adding latency.
    let queue = gst::ElementFactory::make("queue")
        .property("max-size-buffers", 1u32)
        .property("max-size-bytes", 0u32)
        .property("max-size-time", 0u64)
        .property_from_str("leaky", "downstream")
        .build()?;

    let mut chain = vec![src, filter];
    match mode.encoding {
        Encoding::Mjpeg if hardware_decode => {
            chain.push(gst::ElementFactory::make("jpegparse").build()?);
            chain.push(gst::ElementFactory::make("nvjpegdec").build()?);
            chain.push(gst::ElementFactory::make("cudadownload").build()?);
        }
        Encoding::Mjpeg => {
            chain.push(gst::ElementFactory::make("jpegparse").build()?);
            chain.push(gst::ElementFactory::make("jpegdec").build()?);
        }
        Encoding::Raw(_) => {}
    }
    chain.push(queue);
    chain.push(sink.clone());

    pipeline.add_many(&chain)?;
    gst::Element::link_many(&chain)?;
    Ok(pipeline)
}

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

/// Name of the `volume` element inside the audio pipeline.
pub const VOLUME_ELEMENT: &str = "volume";
/// Name of the `level` element inside the audio pipeline.
pub const LEVEL_ELEMENT: &str = "level";

/// Capture from the card and play it on the default output device.
pub fn build_audio_pipeline(device: &gst::Device) -> Result<gst::Pipeline, glib::BoolError> {
    let pipeline = gst::Pipeline::with_name("audio");

    // PipeWire's device monitor hands out pipewiresrc, whose timestamps jump
    // by ~65 ms about once a second here; pulsesink resyncs on every jump,
    // which is heard as a short dropout.  pulsesrc (via pipewire-pulse) on the
    // same node keeps clean timestamps.
    let node_name = device.properties().and_then(|p| p.get::<String>("node.name").ok());
    let src = match node_name {
        Some(node) if gst::ElementFactory::find("pulsesrc").is_some() => {
            gst::ElementFactory::make("pulsesrc").property("device", node).build()?
        }
        _ => device.create_element(None)?,
    };
    // Small capture buffers keep the monitor close to real time.
    if src.has_property("buffer-time") {
        src.set_property("buffer-time", 40_000i64);
        src.set_property("latency-time", 10_000i64);
    }

    // Without this, pipewiresrc happily negotiates mono and drops a channel.
    let src_caps = gst::ElementFactory::make("capsfilter")
        .property("caps", gst::Caps::builder("audio/x-raw").field("channels", 2).build())
        .build()?;
    let convert = gst::ElementFactory::make("audioconvert").build()?;
    let resample = gst::ElementFactory::make("audioresample").build()?;
    let level = gst::ElementFactory::make("level")
        .name(LEVEL_ELEMENT)
        .property("interval", 50_000_000u64)
        .property("post-messages", true)
        .build()?;
    let volume = gst::ElementFactory::make("volume").name(VOLUME_ELEMENT).build()?;

    let sink = match gst::ElementFactory::make("pulsesink").build() {
        Ok(sink) => {
            sink.set_property("buffer-time", 40_000i64);
            sink.set_property("latency-time", 10_000i64);
            sink.set_property("client-name", "Elgato 4K Capture");
            sink
        }
        Err(_) => gst::ElementFactory::make("autoaudiosink").build()?,
    };

    let chain = [&src, &src_caps, &convert, &resample, &level, &volume, &sink];
    pipeline.add_many(chain)?;
    gst::Element::link_many(chain)?;
    Ok(pipeline)
}

/// Extract the loudest channel's peak (dBFS) from a `level` element message.
pub fn level_peak_db(structure: &gst::StructureRef) -> Option<f64> {
    if structure.name() != "level" {
        return None;
    }
    let value = structure.value("peak").ok()?;
    #[allow(deprecated)]
    let peaks: Vec<f64> = if let Ok(array) = value.get::<glib::ValueArray>() {
        array.iter().filter_map(|v| v.get::<f64>().ok()).collect()
    } else if let Ok(array) = value.get::<gst::Array>() {
        array.iter().filter_map(|v| v.get::<f64>().ok()).collect()
    } else {
        return None;
    };
    peaks.into_iter().reduce(f64::max)
}
