//! Thin wrappers over the Core Audio HAL calls `dps live` makes. Each object
//! created here is destroyed by its guard's `Drop`.

mod input;
pub use input::*;

use std::ffi::{CStr, c_void};
use std::fmt;
use std::mem::{MaybeUninit, size_of};
use std::ptr::{self, NonNull};

use objc2::AnyThread;
use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProc, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectAddPropertyListener, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectPropertyListenerProc, AudioObjectPropertyScope, AudioObjectPropertySelector,
    AudioObjectRemovePropertyListener, AudioObjectSetPropertyData, CATapDescription,
    CATapMuteBehavior, kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyDeviceIsRunning,
    kAudioDevicePropertyDeviceUID, kAudioHardwareBadPropertySizeError,
    kAudioHardwarePropertyProcessObjectList, kAudioHardwarePropertyTranslatePIDToProcessObject,
    kAudioHardwareUnspecifiedError, kAudioObjectPropertyElementMain, kAudioObjectPropertyName,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioObjectUnknown,
    kAudioProcessPropertyIsRunningOutput, kAudioStreamPropertyTerminalType,
    kAudioStreamPropertyVirtualFormat, kAudioSubDeviceUIDKey, kAudioSubTapDriftCompensationKey,
    kAudioSubTapUIDKey, kAudioTapPropertyFormat, kAudioTapPropertyUID,
};
use objc2_core_audio_types::AudioStreamBasicDescription;
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_foundation::{NSArray, NSNumber, NSString};

pub use objc2_core_audio::{
    kAudioDeviceProcessorOverload, kAudioDevicePropertyDeviceIsAlive,
    kAudioDevicePropertyIOStoppedAbnormally, kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyStreams,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyScopeInput,
    kAudioObjectPropertyScopeOutput, kAudioObjectPropertyScopeWildcard,
};

pub type ObjectId = AudioObjectID;
pub type PropertyAddress = AudioObjectPropertyAddress;

pub const SYSTEM_OBJECT: ObjectId = kAudioObjectSystemObject as ObjectId;
pub const UNKNOWN: ObjectId = kAudioObjectUnknown;
pub const GLOBAL: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal;

/// A failed HAL call: what was being done and the OSStatus it returned.
#[derive(Debug, Clone, Copy)]
pub struct Error {
    pub action: &'static str,
    pub status: i32,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Most HAL errors are four-character codes such as 'nope' or '!obj'.
        let code = self.status.to_be_bytes();
        if code.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
            let code: String = code.iter().map(|&b| char::from(b)).collect();
            write!(f, "cannot {} (Core Audio error '{code}')", self.action)
        } else {
            write!(
                f,
                "cannot {} (Core Audio error {})",
                self.action, self.status
            )
        }
    }
}

impl From<Error> for String {
    fn from(e: Error) -> Self {
        e.to_string()
    }
}

fn check(action: &'static str, status: i32) -> Result<(), Error> {
    if status == 0 {
        Ok(())
    } else {
        Err(Error { action, status })
    }
}

fn address(
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Reads a fixed-size property, passing `qualifier` if given.
///
/// # Safety
/// `T` (and `Q`) must be the property's C types.
unsafe fn get_qualified<T: Copy, Q>(
    object: ObjectId,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
    qualifier: Option<&Q>,
    action: &'static str,
) -> Result<T, Error> {
    let address = address(selector, scope);
    let (qualifier_size, qualifier) = match qualifier {
        Some(q) => (size_of::<Q>() as u32, ptr::from_ref(q).cast::<c_void>()),
        None => (0, ptr::null()),
    };
    let mut value = MaybeUninit::<T>::uninit();
    let mut size = size_of::<T>() as u32;
    // SAFETY: `value` has room for `size` bytes and the caller vouches for T.
    check(action, unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            qualifier_size,
            qualifier,
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    })?;
    if size as usize != size_of::<T>() {
        return Err(Error {
            action,
            status: kAudioHardwareBadPropertySizeError,
        });
    }
    // SAFETY: the HAL filled all of it.
    Ok(unsafe { value.assume_init() })
}

/// # Safety
/// `T` must be the property's C type.
unsafe fn get<T: Copy>(
    object: ObjectId,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
    action: &'static str,
) -> Result<T, Error> {
    // SAFETY: forwarded to the caller.
    unsafe { get_qualified::<T, ()>(object, selector, scope, None, action) }
}

/// Reads a property that is an array of `T`.
///
/// # Safety
/// `T` must be the property's C element type.
unsafe fn get_list<T: Copy>(
    object: ObjectId,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
    action: &'static str,
) -> Result<Vec<T>, Error> {
    let address = address(selector, scope);
    let mut size = 0u32;
    // SAFETY: plain out-parameter.
    check(action, unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
        )
    })?;
    let capacity = size as usize / size_of::<T>();
    let mut items = Vec::<T>::with_capacity(capacity);
    if capacity == 0 {
        return Ok(items);
    }
    let mut size = (capacity * size_of::<T>()) as u32;
    // SAFETY: `items` has room for `size` bytes.
    check(action, unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::new_unchecked(items.as_mut_ptr()).cast(),
        )
    })?;
    // SAFETY: the HAL wrote `size` bytes of whole elements.
    unsafe { items.set_len((size as usize / size_of::<T>()).min(capacity)) };
    Ok(items)
}

/// Reads a CFString property, which the HAL hands over retained.
fn get_string(
    object: ObjectId,
    selector: AudioObjectPropertySelector,
    action: &'static str,
) -> Result<String, Error> {
    // SAFETY: these selectors are all CFStringRef properties.
    let raw: *const CFString = unsafe { get(object, selector, GLOBAL, action)? };
    let raw = NonNull::new(raw.cast_mut()).ok_or(Error {
        action,
        status: kAudioHardwareUnspecifiedError,
    })?;
    // SAFETY: the HAL returns a +1 reference that we now own.
    let string = unsafe { CFRetained::from_raw(raw) };
    Ok(string.to_string())
}

/// `kAudioHardwarePropertyRunLoop`, declared in AudioHardwareDeprecated.h,
/// which the enabled objc2-core-audio features leave out.
const PROPERTY_RUN_LOOP: AudioObjectPropertySelector = u32::from_be_bytes(*b"rnlp");

/// Has the HAL deliver property notifications on a thread of its own instead
/// of the main run loop, which `dps live` never runs. macOS 10.6 and later
/// default to the main run loop; recent versions already use their own
/// thread.
pub fn notify_on_hal_thread() -> Result<(), Error> {
    let address = address(PROPERTY_RUN_LOOP, GLOBAL);
    let none: *const c_void = ptr::null();
    // SAFETY: a CFRunLoopRef property, set to NULL as the header documents.
    check("set the thread for Core Audio notifications", unsafe {
        AudioObjectSetPropertyData(
            SYSTEM_OBJECT,
            NonNull::from(&address),
            0,
            ptr::null(),
            size_of_val(&none) as u32,
            NonNull::from(&none).cast(),
        )
    })
}

pub fn default_output_device() -> Result<Option<ObjectId>, Error> {
    // SAFETY: an AudioObjectID property.
    let id: ObjectId = unsafe {
        get(
            SYSTEM_OBJECT,
            kAudioHardwarePropertyDefaultOutputDevice,
            GLOBAL,
            "read the default output device",
        )?
    };
    Ok((id != kAudioObjectUnknown).then_some(id))
}

pub fn name(object: ObjectId) -> Result<String, Error> {
    get_string(object, kAudioObjectPropertyName, "read the device name")
}

pub fn device_uid(device: ObjectId) -> Result<String, Error> {
    get_string(device, kAudioDevicePropertyDeviceUID, "read the device UID")
}

pub fn nominal_sample_rate(device: ObjectId) -> Result<f64, Error> {
    // SAFETY: a Float64 property.
    unsafe {
        get(
            device,
            kAudioDevicePropertyNominalSampleRate,
            GLOBAL,
            "read the device sample rate",
        )
    }
}

pub fn is_running(device: ObjectId) -> Result<bool, Error> {
    // SAFETY: a UInt32 property.
    let running: u32 = unsafe {
        get(
            device,
            kAudioDevicePropertyDeviceIsRunning,
            GLOBAL,
            "check that audio IO is running",
        )?
    };
    Ok(running != 0)
}

/// One of a device's streams.
#[derive(Debug, Clone, Copy)]
pub struct Stream {
    /// The virtual (IOProc-side) format.
    pub format: AudioStreamBasicDescription,
    /// A `kAudioStreamTerminalType*` value or a USB terminal type.
    pub terminal: u32,
}

/// The device's streams in `scope`, in the order the IOProc's buffer lists
/// use.
pub fn streams(device: ObjectId, scope: AudioObjectPropertyScope) -> Result<Vec<Stream>, Error> {
    // SAFETY: an array of AudioStreamIDs.
    let streams: Vec<ObjectId> = unsafe {
        get_list(
            device,
            kAudioDevicePropertyStreams,
            scope,
            "list the device's streams",
        )?
    };
    streams
        .into_iter()
        .map(|stream| {
            // SAFETY: an AudioStreamBasicDescription and a UInt32 property.
            unsafe {
                Ok(Stream {
                    format: get(
                        stream,
                        kAudioStreamPropertyVirtualFormat,
                        GLOBAL,
                        "read a stream format",
                    )?,
                    terminal: get(
                        stream,
                        kAudioStreamPropertyTerminalType,
                        GLOBAL,
                        "read a stream's terminal type",
                    )?,
                })
            }
        })
        .collect()
}

/// The HAL's object for the process with `pid`, if it has one.
pub fn process_object(pid: i32) -> Result<Option<ObjectId>, Error> {
    // SAFETY: an AudioObjectID property qualified by a pid_t.
    let id: ObjectId = unsafe {
        get_qualified(
            SYSTEM_OBJECT,
            kAudioHardwarePropertyTranslatePIDToProcessObject,
            GLOBAL,
            Some(&pid),
            "find this process's audio object",
        )?
    };
    Ok((id != kAudioObjectUnknown).then_some(id))
}

/// Whether any process other than `own` is currently running audio output.
pub fn others_playing(own: ObjectId) -> Result<bool, Error> {
    // SAFETY: an array of AudioObjectIDs.
    let processes: Vec<ObjectId> = unsafe {
        get_list(
            SYSTEM_OBJECT,
            kAudioHardwarePropertyProcessObjectList,
            GLOBAL,
            "list audio processes",
        )?
    };
    Ok(processes.into_iter().filter(|&p| p != own).any(|p| {
        // SAFETY: a UInt32 property. A process that exits meanwhile just
        // reads as not playing.
        let running: Result<u32, _> = unsafe {
            get(
                p,
                kAudioProcessPropertyIsRunningOutput,
                GLOBAL,
                "read a process's output state",
            )
        };
        running.is_ok_and(|r| r != 0)
    }))
}

/// A process tap, destroyed on drop.
pub struct Tap {
    pub id: ObjectId,
}

impl Tap {
    /// A private stereo mix of every process except `exclude`. If `muted`,
    /// the tapped processes are muted on their own devices while the tap is
    /// being read; otherwise they play on as before.
    pub fn global_stereo_excluding(
        exclude: ObjectId,
        name: &str,
        muted: bool,
    ) -> Result<Self, Error> {
        let excluded = NSArray::from_retained_slice(&[NSNumber::new_u32(exclude)]);
        let mute = if muted {
            CATapMuteBehavior::MutedWhenTapped
        } else {
            CATapMuteBehavior::Unmuted
        };
        // SAFETY: plain Objective-C object setup on an object we own.
        let description = unsafe {
            let d = CATapDescription::initStereoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &excluded,
            );
            d.setName(&NSString::from_str(name));
            d.setPrivate(true);
            d.setMuteBehavior(mute);
            d
        };
        let mut id = kAudioObjectUnknown;
        // SAFETY: valid description and out-pointer.
        check("create the system audio tap", unsafe {
            AudioHardwareCreateProcessTap(Some(&description), &mut id)
        })?;
        Ok(Self { id })
    }

    pub fn uid(&self) -> Result<String, Error> {
        get_string(self.id, kAudioTapPropertyUID, "read the tap's UID")
    }

    pub fn format(&self) -> Result<AudioStreamBasicDescription, Error> {
        // SAFETY: an AudioStreamBasicDescription property.
        unsafe {
            get(
                self.id,
                kAudioTapPropertyFormat,
                GLOBAL,
                "read the tap's format",
            )
        }
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        // SAFETY: we created this tap and destroy it once.
        unsafe { AudioHardwareDestroyProcessTap(self.id) };
    }
}

fn key(k: &'static CStr) -> CFRetained<CFString> {
    CFString::from_str(k.to_str().expect("HAL keys are ASCII"))
}

fn dictionary(entries: &[(&'static CStr, &CFType)]) -> CFRetained<CFDictionary<CFString, CFType>> {
    let keys: Vec<_> = entries.iter().map(|(k, _)| key(k)).collect();
    let keys: Vec<&CFString> = keys.iter().map(|k| &**k).collect();
    let values: Vec<&CFType> = entries.iter().map(|(_, v)| *v).collect();
    CFDictionary::from_slices(&keys, &values)
}

/// A private aggregate device, destroyed on drop.
pub struct Aggregate {
    pub id: ObjectId,
}

impl Aggregate {
    /// Combines the output device `output_uid`, which is the clock, with the
    /// tap `tap_uid`, drift-compensated against that clock.
    pub fn new(output_uid: &str, tap_uid: &str) -> Result<Self, Error> {
        let output_uid = CFString::from_str(output_uid);
        let tap_uid = CFString::from_str(tap_uid);
        let uid = CFString::from_str(&format!("dps-live-{}-{tap_uid}", std::process::id()));
        let name = CFString::from_str("dps live");
        let yes = CFBoolean::new(true);
        let no = CFBoolean::new(false);
        let sub_device = dictionary(&[(kAudioSubDeviceUIDKey, &output_uid)]);
        let sub_tap = dictionary(&[
            (kAudioSubTapUIDKey, &tap_uid),
            (kAudioSubTapDriftCompensationKey, yes),
        ]);
        let sub_devices = CFArray::from_objects(&[&*sub_device]);
        let sub_taps = CFArray::from_objects(&[&*sub_tap]);
        let description = dictionary(&[
            (kAudioAggregateDeviceNameKey, &name),
            (kAudioAggregateDeviceUIDKey, &uid),
            (kAudioAggregateDeviceMainSubDeviceKey, &output_uid),
            (kAudioAggregateDeviceIsPrivateKey, yes),
            (kAudioAggregateDeviceIsStackedKey, no),
            (kAudioAggregateDeviceSubDeviceListKey, &sub_devices),
            (kAudioAggregateDeviceTapListKey, &sub_taps),
        ]);
        let mut id = kAudioObjectUnknown;
        // SAFETY: a CFDictionary of the documented keys and an out-pointer.
        check("create the aggregate device", unsafe {
            AudioHardwareCreateAggregateDevice(description.as_ref(), NonNull::from(&mut id))
        })?;
        Ok(Self { id })
    }
}

impl Drop for Aggregate {
    fn drop(&mut self) {
        // SAFETY: we created this device and destroy it once.
        unsafe { AudioHardwareDestroyAggregateDevice(self.id) };
    }
}

/// A registered, running IOProc that owns the data it is called with. On
/// drop it stops and unregisters the proc, then frees that data.
pub struct IoProc<T> {
    device: ObjectId,
    id: AudioDeviceIOProcID,
    client: NonNull<T>,
}

impl<T> IoProc<T> {
    /// Registers `proc` on `device` with `client` as its data and starts IO.
    ///
    /// # Safety
    /// `proc` must be sound when called with a pointer to a `T`, from the
    /// HAL's IO thread.
    pub unsafe fn start(
        device: ObjectId,
        proc: AudioDeviceIOProc,
        client: Box<T>,
    ) -> Result<Self, Error> {
        let client = NonNull::from(Box::leak(client));
        let mut id: AudioDeviceIOProcID = None;
        // SAFETY: forwarded to the caller; `id` is an out-pointer.
        let registered = check("register the audio callback", unsafe {
            AudioDeviceCreateIOProcID(device, proc, client.as_ptr().cast(), NonNull::from(&mut id))
        });
        if let Err(e) = registered {
            // SAFETY: made by `Box::leak` above, and the HAL never got it.
            drop(unsafe { Box::from_raw(client.as_ptr()) });
            return Err(e);
        }
        let guard = Self { device, id, client };
        // SAFETY: `id` was just registered on `device`.
        check("start audio IO", unsafe { AudioDeviceStart(device, id) })?;
        Ok(guard)
    }
}

impl<T> Drop for IoProc<T> {
    fn drop(&mut self) {
        // SAFETY: `id` is registered on `device`. Stopping a proc that never
        // started succeeds.
        let (stopped, destroyed) = unsafe {
            (
                AudioDeviceStop(self.device, self.id),
                AudioDeviceDestroyIOProcID(self.device, self.id),
            )
        };
        // If either call failed, the HAL may still call the proc with
        // `client`, so it is leaked instead.
        if stopped == 0 && destroyed == 0 {
            // SAFETY: made by `Box::leak` in `start`, and the proc is gone.
            drop(unsafe { Box::from_raw(self.client.as_ptr()) });
        }
    }
}

/// A property listener, removed on drop.
pub struct Listener {
    object: ObjectId,
    address: AudioObjectPropertyAddress,
    proc: AudioObjectPropertyListenerProc,
    client: *mut c_void,
}

impl Listener {
    /// # Safety
    /// `proc` must be sound for `client`, and `client` must stay valid for
    /// as long as the HAL may call it, which includes a little after drop.
    pub unsafe fn add(
        object: ObjectId,
        selector: AudioObjectPropertySelector,
        scope: AudioObjectPropertyScope,
        proc: AudioObjectPropertyListenerProc,
        client: *mut c_void,
        action: &'static str,
    ) -> Result<Self, Error> {
        let address = address(selector, scope);
        // SAFETY: forwarded to the caller.
        check(action, unsafe {
            AudioObjectAddPropertyListener(object, NonNull::from(&address), proc, client)
        })?;
        Ok(Self {
            object,
            address,
            proc,
            client,
        })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // SAFETY: the same arguments it was added with.
        unsafe {
            AudioObjectRemovePropertyListener(
                self.object,
                NonNull::from(&self.address),
                self.proc,
                self.client,
            )
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_show_four_character_codes() {
        let e = Error {
            action: "start audio IO",
            status: i32::from_be_bytes(*b"nope"),
        };
        assert_eq!(
            e.to_string(),
            "cannot start audio IO (Core Audio error 'nope')"
        );
        let e = Error {
            action: "read a stream format",
            status: -50,
        };
        assert_eq!(
            e.to_string(),
            "cannot read a stream format (Core Audio error -50)"
        );
    }
}
