//! The HAL calls `tonewell calibrate` makes beyond those of `tonewell live`.

use objc2_core_audio::{
    kAudioDevicePropertyTransportType, kAudioDeviceTransportTypeVirtual,
    kAudioHardwarePropertyDefaultInputDevice,
};

use super::{Error, GLOBAL, ObjectId, SYSTEM_OBJECT, UNKNOWN, get};

pub fn default_input_device() -> Result<Option<ObjectId>, Error> {
    // SAFETY: an AudioObjectID property.
    let id: ObjectId = unsafe {
        get(
            SYSTEM_OBJECT,
            kAudioHardwarePropertyDefaultInputDevice,
            GLOBAL,
            "read the default input device",
        )?
    };
    Ok((id != UNKNOWN).then_some(id))
}

/// Whether `device` is a virtual one, such as a loopback driver, rather than
/// hardware.
pub fn is_virtual(device: ObjectId) -> Result<bool, Error> {
    // SAFETY: a UInt32 property.
    let transport: u32 = unsafe {
        get(
            device,
            kAudioDevicePropertyTransportType,
            GLOBAL,
            "read how the device is connected",
        )?
    };
    Ok(transport == kAudioDeviceTransportTypeVirtual)
}
