//! Blocking access to the card's settings through the elgato4k-linux library.
//!
//! Every operation opens the device, does its work and closes it again, so
//! the GUI never holds the USB interface longer than necessary.  Calls are
//! serialised with a global lock and must run off the GTK main thread.

use std::sync::Mutex;

use elgato4k_linux::{
    AudioInput, DeviceModel, DeviceStatus, EdidRangePolicy, EdidSource, ElgatoDevice, ElgatoError,
    HdrToneMapping, VideoScaler,
};

static DEVICE_LOCK: Mutex<()> = Mutex::new(());

/// A single setting change requested from the UI.
#[derive(Clone, Copy, Debug)]
pub enum Setting {
    HdrToneMapping(HdrToneMapping),
    ColorRange(EdidRangePolicy),
    EdidSource(EdidSource),
    AudioInput(AudioInput),
    VideoScaler(VideoScaler),
}

pub struct Snapshot {
    pub model: DeviceModel,
    pub status: DeviceStatus,
}

pub fn read() -> Result<Snapshot, ElgatoError> {
    let _guard = DEVICE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let device = ElgatoDevice::open()?;
    Ok(Snapshot { model: device.model(), status: device.read_status()? })
}

pub fn apply(setting: Setting) -> Result<(), ElgatoError> {
    let _guard = DEVICE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let device = ElgatoDevice::open()?;
    match setting {
        Setting::HdrToneMapping(v) => device.set_hdr_mapping(v),
        Setting::ColorRange(v) => device.set_hdmi_range(v),
        Setting::EdidSource(v) => device.set_edid_source(v),
        Setting::AudioInput(v) => device.set_audio_input(v),
        Setting::VideoScaler(v) => device.set_video_scaler(v),
    }
}

/// True when the failure is a USB permission problem that the udev rule fixes.
pub fn is_permission_error(error: &ElgatoError) -> bool {
    matches!(error, ElgatoError::Usb(rusb::Error::Access))
}
