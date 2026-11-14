//! Thin wrappers over the Core Audio HAL calls `dps live` makes. Each object
//! created here is destroyed by its guard's `Drop`.

mod input;
pub use input::*;

use std::ffi::{CStr, c_void};
use std::fmt;
use std::mem::{MaybeUninit, size_of};
use std::ptr::{self, NonNull};

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProc, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectAddPropertyListener, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectHasProperty, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertyElement, AudioObjectPropertyListenerProc,
    AudioObjectPropertyScope, AudioObjectPropertySelector, AudioObjectRemovePropertyListener,
    AudioObjectSetPropertyData, CATapDescription, CATapMuteBehavior,
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyBufferFrameSize,
    kAudioDevicePropertyDeviceIsRunning, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyLatency, kAudioDevicePropertyPreferredChannelsForStereo,
    kAudioDevicePropertySafetyOffset, kAudioDevicePropertyVolumeRangeDecibels,
    kAudioHardwareBadPropertySizeError, kAudioHardwarePropertyDevices,
    kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioHardwareUnspecifiedError,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectSystemObject, kAudioObjectUnknown, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyIsRunningOutput, kAudioStreamPropertyLatency,
    kAudioStreamPropertyTerminalType, kAudioStreamPropertyVirtualFormat, kAudioSubDeviceUIDKey,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, kAudioTapPropertyDescription,
    kAudioTapPropertyFormat, kAudioTapPropertyUID,
};
use objc2_core_audio_types::{AudioStreamBasicDescription, AudioValueRange};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_foundation::{NSArray, NSNumber, NSString};

pub use objc2_core_audio::{
    kAudioDeviceProcessorOverload, kAudioDevicePropertyDeviceIsAlive,
    kAudioDevicePropertyIOStoppedAbnormally, kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyStreams,
    kAudioDevicePropertyVolumeDecibels, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyScopeInput,
    kAudioObjectPropertyScopeOutput, kAudioObjectPropertyScopeWildcard,
};

pub type ObjectId = AudioObjectID;
pub type PropertyAddress = AudioObjectPropertyAddress;

pub const SYSTEM_OBJECT: ObjectId = kAudioObjectSystemObject as ObjectId;
pub const UNKNOWN: ObjectId = kAudioObjectUnknown;
pub const GLOBAL: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal;
pub const MAIN: AudioObjectPropertyElement = kAudioObjectPropertyElementMain;

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
    element_address(selector, scope, MAIN)
}

fn element_address(
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
    element: AudioObjectPropertyElement,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: element,
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
    // SAFETY: forwarded to the caller.
    unsafe { get_at(object, address(selector, scope), qualifier, action) }
}

/// Reads a fixed-size property of one element.
///
/// # Safety
/// `T` must be the property's C type.
unsafe fn get_element<T: Copy>(
    object: ObjectId,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
    element: AudioObjectPropertyElement,
    action: &'static str,
) -> Result<T, Error> {
    let address = element_address(selector, scope, element);
    // SAFETY: forwarded to the caller.
    unsafe { get_at::<T, ()>(object, address, None, action) }
}

/// # Safety
/// `T` (and `Q`) must be the property's C types.
unsafe fn get_at<T: Copy, Q>(
    object: ObjectId,
    address: AudioObjectPropertyAddress,
    qualifier: Option<&Q>,
    action: &'static str,
) -> Result<T, Error> {
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

fn has_property(
    object: ObjectId,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
    element: AudioObjectPropertyElement,
) -> bool {
    let address = element_address(selector, scope, element);
    // SAFETY: a valid address.
    unsafe { AudioObjectHasProperty(object, NonNull::from(&address)) }
}

/// The elements whose output volume in dB `device` reports: its main
/// volume, or else the channels of its preferred stereo pair (1 and 2 if
/// it names none) that have one; none for a device without a volume
/// control, as HDMI outputs and some DACs are.
pub fn volume_elements(device: ObjectId) -> Vec<AudioObjectPropertyElement> {
    let output = kAudioObjectPropertyScopeOutput;
    let has_volume = |e| has_property(device, kAudioDevicePropertyVolumeDecibels, output, e);
    if has_volume(MAIN) {
        return vec![MAIN];
    }
    // SAFETY: a UInt32[2] property.
    let pair: Result<[u32; 2], _> = unsafe {
        get(
            device,
            kAudioDevicePropertyPreferredChannelsForStereo,
            output,
            "read the device's stereo pair",
        )
    };
    pair.unwrap_or([1, 2])
        .into_iter()
        .filter(|&e| has_volume(e))
        .collect()
}

/// The output volume of `device` in dB below its maximum, mute aside: the
/// mean over `volume_elements`, or `None` if it has none.
pub fn output_volume_db(device: ObjectId) -> Result<Option<f32>, Error> {
    let output = kAudioObjectPropertyScopeOutput;
    let readings = volume_elements(device)
        .into_iter()
        .map(|element| {
            // SAFETY: a Float32 and an AudioValueRange property.
            unsafe {
                let db: f32 = get_element(
                    device,
                    kAudioDevicePropertyVolumeDecibels,
                    output,
                    element,
                    "read the output volume",
                )?;
                let range: Result<AudioValueRange, _> = get_element(
                    device,
                    kAudioDevicePropertyVolumeRangeDecibels,
                    output,
                    element,
                    "read the output volume's range",
                );
                Ok((db, range.map_or(0.0, |r| r.mMaximum)))
            }
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(below_max(&readings))
}

/// The mean of `readings`, each a volume in dB and its range's maximum, as
/// dB below the maximum: 0 is full volume.
pub fn below_max(readings: &[(f32, f64)]) -> Option<f32> {
    let below: Vec<f64> = readings
        .iter()
        .map(|&(db, max)| f64::from(db) - max)
        .filter(|db| db.is_finite())
        .collect();
    (!below.is_empty()).then(|| (below.iter().sum::<f64>() / below.len() as f64).min(0.0) as f32)
}

/// The device's IO buffer size in frames.
pub fn buffer_frames(device: ObjectId) -> Result<u32, Error> {
    // SAFETY: a UInt32 property.
    unsafe {
        get(
            device,
            kAudioDevicePropertyBufferFrameSize,
            GLOBAL,
            "read the IO buffer size",
        )
    }
}

/// The device's latency in `scope`, in frames.
pub fn device_latency(device: ObjectId, scope: AudioObjectPropertyScope) -> Result<u32, Error> {
    // SAFETY: a UInt32 property.
    unsafe {
        get(
            device,
            kAudioDevicePropertyLatency,
            scope,
            "read the device latency",
        )
    }
}

/// The device's safety offset in `scope`, in frames.
pub fn safety_offset(device: ObjectId, scope: AudioObjectPropertyScope) -> Result<u32, Error> {
    // SAFETY: a UInt32 property.
    unsafe {
        get(
            device,
            kAudioDevicePropertySafetyOffset,
            scope,
            "read the safety offset",
        )
    }
}

/// A stream's latency in frames.
pub fn stream_latency(stream: ObjectId) -> Result<u32, Error> {
    // SAFETY: a UInt32 property.
    unsafe {
        get(
            stream,
            kAudioStreamPropertyLatency,
            GLOBAL,
            "read a stream's latency",
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

/// Every audio device, input or output.
pub fn devices() -> Result<Vec<ObjectId>, Error> {
    // SAFETY: an array of AudioObjectIDs.
    unsafe {
        get_list(
            SYSTEM_OBJECT,
            kAudioHardwarePropertyDevices,
            GLOBAL,
            "list the audio devices",
        )
    }
}

/// The device's stream objects in `scope`, in the order the IOProc's
/// buffer lists use.
pub fn stream_ids(
    device: ObjectId,
    scope: AudioObjectPropertyScope,
) -> Result<Vec<ObjectId>, Error> {
    // SAFETY: an array of AudioStreamIDs.
    unsafe {
        get_list(
            device,
            kAudioDevicePropertyStreams,
            scope,
            "list the device's streams",
        )
    }
}

/// The device's streams in `scope`, in the order the IOProc's buffer lists
/// use.
pub fn streams(device: ObjectId, scope: AudioObjectPropertyScope) -> Result<Vec<Stream>, Error> {
    stream_ids(device, scope)?
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

/// Every process with a connection to the audio system.
pub fn process_objects() -> Result<Vec<ObjectId>, Error> {
    // SAFETY: an array of AudioObjectIDs.
    unsafe {
        get_list(
            SYSTEM_OBJECT,
            kAudioHardwarePropertyProcessObjectList,
            GLOBAL,
            "list audio processes",
        )
    }
}

/// Whether the process is running audio output. One that exits meanwhile
/// reads as not.
pub fn runs_output(process: ObjectId) -> bool {
    // SAFETY: a UInt32 property.
    let running: Result<u32, _> = unsafe {
        get(
            process,
            kAudioProcessPropertyIsRunningOutput,
            GLOBAL,
            "read a process's output state",
        )
    };
    running.is_ok_and(|r| r != 0)
}

/// The process's bundle ID; empty for a process without one.
pub fn bundle_id(process: ObjectId) -> Result<String, Error> {
    get_string(
        process,
        kAudioProcessPropertyBundleID,
        "read a process's bundle ID",
    )
}

/// Whether any process other than `own` is currently running audio output.
pub fn others_playing(own: ObjectId) -> Result<bool, Error> {
    Ok(process_objects()?
        .into_iter()
        .filter(|&p| p != own)
        .any(runs_output))
}

/// A process tap, destroyed on drop.
pub struct Tap {
    pub id: ObjectId,
    /// What it was created from, changed to change what it taps.
    description: Retained<CATapDescription>,
}

/// Process objects as the NSArray a CATapDescription takes.
fn process_array(processes: &[ObjectId]) -> Retained<NSArray<NSNumber>> {
    let numbers: Vec<_> = processes.iter().map(|&p| NSNumber::new_u32(p)).collect();
    NSArray::from_retained_slice(&numbers)
}

impl Tap {
    /// A private stereo mix of every process except those in `exclude`. If
    /// `muted`, the tapped processes are muted on their own devices while
    /// the tap is being read; otherwise they play on as before.
    pub fn global_stereo_excluding(
        exclude: &[ObjectId],
        name: &str,
        muted: bool,
    ) -> Result<Self, Error> {
        let excluded = process_array(exclude);
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
        Ok(Self { id, description })
    }

    /// Leaves out the processes in `exclude` from now on, and no others.
    /// The description keeps the tap's UUID, name, privacy and mute
    /// behaviour; AudioHardware.h says setting it changes an existing tap.
    pub fn exclude(&self, exclude: &[ObjectId]) -> Result<(), Error> {
        // SAFETY: a setter on an object we own.
        unsafe { self.description.setProcesses(&process_array(exclude)) };
        let address = address(kAudioTapPropertyDescription, GLOBAL);
        let description: *const CATapDescription = Retained::as_ptr(&self.description);
        // SAFETY: a CATapDescription property, passed like CF properties as
        // a pointer to the object pointer, which outlives the call.
        check("change the apps the system audio tap leaves out", unsafe {
            AudioObjectSetPropertyData(
                self.id,
                NonNull::from(&address),
                0,
                ptr::null(),
                size_of_val(&description) as u32,
                NonNull::from(&description).cast(),
            )
        })
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
        // SAFETY: forwarded to the caller.
        unsafe { Self::add_on_element(object, selector, scope, MAIN, proc, client, action) }
    }

    /// `add` for the property of one element.
    ///
    /// # Safety
    /// As for `add`.
    pub unsafe fn add_on_element(
        object: ObjectId,
        selector: AudioObjectPropertySelector,
        scope: AudioObjectPropertyScope,
        element: AudioObjectPropertyElement,
        proc: AudioObjectPropertyListenerProc,
        client: *mut c_void,
        action: &'static str,
    ) -> Result<Self, Error> {
        let address = element_address(selector, scope, element);
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

    /// Prints what this machine's HAL reports, reading only: no tap, no
    /// aggregate device, no IO. Run by hand with `--ignored --nocapture`.
    #[test]
    #[ignore = "reads the real audio system"]
    fn read_only_facts_about_this_machine() {
        let default = default_output_device().unwrap();
        for device in devices().unwrap() {
            let outputs = stream_ids(device, kAudioObjectPropertyScopeOutput).unwrap();
            println!(
                "device {device} {:?} uid {:?}: {} output streams{}",
                name(device),
                device_uid(device),
                outputs.len(),
                if Some(device) == default {
                    " (default output)"
                } else {
                    ""
                }
            );
        }
        for process in process_objects().unwrap() {
            println!(
                "process {process}: bundle {:?}, running output {}",
                bundle_id(process),
                runs_output(process)
            );
        }
        for device in devices().unwrap() {
            if stream_ids(device, kAudioObjectPropertyScopeOutput)
                .unwrap()
                .is_empty()
            {
                continue;
            }
            let output = kAudioObjectPropertyScopeOutput;
            let elements: Vec<_> = (0..=2)
                .filter(|&e| has_property(device, kAudioDevicePropertyVolumeDecibels, output, e))
                .collect();
            let ranges: Vec<_> = elements
                .iter()
                .map(|&e| {
                    // SAFETY: an AudioValueRange property.
                    let range: Result<AudioValueRange, _> = unsafe {
                        get_element(
                            device,
                            kAudioDevicePropertyVolumeRangeDecibels,
                            output,
                            e,
                            "read the range",
                        )
                    };
                    range.map(|r| (r.mMinimum, r.mMaximum))
                })
                .collect();
            println!(
                "device {device} {:?}: volume dB on elements {elements:?} with ranges {ranges:?}, \
                 used {:?}, reads {:?}",
                name(device),
                volume_elements(device),
                output_volume_db(device)
            );
            let streams = stream_ids(device, output).unwrap();
            println!(
                "device {device} {:?}: buffer {:?} frames at {:?} Hz, output latency {:?}, \
                 safety offset {:?}, stream latencies {:?}; input latency {:?}, safety offset \
                 {:?}",
                name(device),
                buffer_frames(device),
                nominal_sample_rate(device),
                device_latency(device, output),
                safety_offset(device, output),
                streams
                    .iter()
                    .map(|&s| stream_latency(s))
                    .collect::<Vec<_>>(),
                device_latency(device, kAudioObjectPropertyScopeInput),
                safety_offset(device, kAudioObjectPropertyScopeInput),
            );
        }
    }

    #[test]
    fn volume_is_the_mean_in_db_below_each_range_maximum() {
        assert_eq!(below_max(&[]), None);
        assert_eq!(below_max(&[(-20.0, 0.0)]), Some(-20.0));
        // A range topping out above 0 dB still reads 0 at full volume.
        assert_eq!(below_max(&[(6.0, 6.0)]), Some(0.0));
        assert_eq!(below_max(&[(-4.0, 6.0)]), Some(-10.0));
        // Two channels of a stereo pair, set apart: their mean.
        assert_eq!(below_max(&[(-10.0, 0.0), (-20.0, 0.0)]), Some(-15.0));
        // A reading that is not a number is left out.
        assert_eq!(below_max(&[(f32::NAN, 0.0), (-6.0, 0.0)]), Some(-6.0));
        assert_eq!(below_max(&[(f32::NEG_INFINITY, 0.0)]), None);
        assert_eq!(below_max(&[(1.0, 0.0)]), Some(0.0));
    }

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
