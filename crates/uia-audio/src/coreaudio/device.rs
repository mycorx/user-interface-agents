// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Which CoreAudio devices exist, which one a configured name means, whether
//! a bound device is still there, and listening for that to change.

use super::Wake;
use objc2_core_audio::{
    AudioDeviceID, AudioObjectAddPropertyListener, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectPropertyScope, AudioObjectPropertySelector, AudioObjectRemovePropertyListener,
    AudioObjectSetPropertyData, kAudioDevicePropertyAvailableNominalSampleRates,
    kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyDeviceNameCFString,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyStreamConfiguration,
    kAudioDevicePropertyStreams, kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
    kAudioStreamPropertyTerminalType, kAudioStreamTerminalTypeHeadphones,
    kAudioStreamTerminalTypeLFESpeaker, kAudioStreamTerminalTypeReceiverSpeaker,
    kAudioStreamTerminalTypeSpeaker, kAudioStreamTerminalTypeUnknown,
};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioValueRange};
use objc2_core_foundation::{CFRetained, CFString};
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{NonNull, null};
use std::sync::mpsc::Sender;
use uia_core::audio::AudioError;

const SYSTEM: AudioObjectID = kAudioObjectSystemObject as AudioObjectID;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Input,
    Output,
}

impl Direction {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Direction::Input => "input",
            Direction::Output => "output",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeviceInfo {
    pub(crate) id: AudioDeviceID,
    pub(crate) name: String,
}

/// Which of `devices` a configured `wanted` name means, or the system default
/// when nothing is configured.
///
/// Pure, so the rules — pinned names ignore the default, unpinned choices
/// follow it, a miss is an error rather than a guess — are tested without
/// hardware. `devices` must already be filtered to `dir`, which is what makes
/// a name shared by a mic and a speaker resolve to the right one of each.
pub(crate) fn choose(
    devices: &[DeviceInfo],
    default_id: Option<AudioDeviceID>,
    wanted: Option<&str>,
    dir: Direction,
) -> Result<DeviceInfo, AudioError> {
    match wanted {
        Some(wanted) => devices
            .iter()
            .find(|d| crate::device_name_matches(&d.name, wanted))
            .cloned()
            .ok_or_else(|| {
                let available: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
                AudioError::DeviceUnavailable(format!(
                    "coreaudio: no audio device matching {wanted:?} among {} devices: {available:?}",
                    dir.label()
                ))
            }),
        None => default_id
            .and_then(|id| devices.iter().find(|d| d.id == id))
            .cloned()
            .ok_or_else(|| {
                AudioError::DeviceUnavailable(format!(
                    "coreaudio: no default {} device",
                    dir.label()
                ))
            }),
    }
}

impl Direction {
    fn scope(self) -> AudioObjectPropertyScope {
        match self {
            Direction::Input => kAudioObjectPropertyScopeInput,
            Direction::Output => kAudioObjectPropertyScopeOutput,
        }
    }

    fn default_selector(self) -> AudioObjectPropertySelector {
        match self {
            Direction::Input => kAudioHardwarePropertyDefaultInputDevice,
            Direction::Output => kAudioHardwarePropertyDefaultOutputDevice,
        }
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

/// Read a fixed-size property. `Err` carries the OSStatus.
fn get<T: Copy>(
    object: AudioObjectID,
    addr: AudioObjectPropertyAddress,
    mut value: T,
) -> Result<T, i32> {
    let mut size = size_of::<T>() as u32;
    // SAFETY: `value` is a valid, writable `T` of exactly `size` bytes, and
    // `addr` lives for the call.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    if status == 0 { Ok(value) } else { Err(status) }
}

/// Read a variable-length property holding an array of object IDs (devices,
/// streams). `Err` carries the OSStatus.
fn get_ids(
    object: AudioObjectID,
    addr: AudioObjectPropertyAddress,
) -> Result<Vec<AudioObjectID>, i32> {
    get_array(object, addr, 0)
}

/// Read a variable-length property holding an array of `T`. `zero` fills
/// the buffer before CoreAudio writes it. `Err` carries the OSStatus.
fn get_array<T: Copy>(
    object: AudioObjectID,
    addr: AudioObjectPropertyAddress,
    zero: T,
) -> Result<Vec<T>, i32> {
    let mut size = 0u32;
    // SAFETY: `addr` and `size` are valid for the call.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&addr),
            0,
            null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 {
        return Err(status);
    }
    let mut ids = vec![zero; size as usize / size_of::<T>()];
    if ids.is_empty() {
        return Ok(ids);
    }
    // Never let CoreAudio write more than `ids` holds.
    let mut size = (ids.len() * size_of::<T>()) as u32;
    // SAFETY: `ids` is a properly aligned `[T]` with room for exactly `size`
    // bytes.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            null(),
            NonNull::from(&mut size),
            NonNull::from(&mut ids[..]).cast(),
        )
    };
    if status != 0 {
        return Err(status);
    }
    // The list can shrink between the two calls.
    ids.truncate(size as usize / size_of::<T>());
    Ok(ids)
}

fn device_ids() -> Result<Vec<AudioDeviceID>, AudioError> {
    get_ids(
        SYSTEM,
        address(
            kAudioHardwarePropertyDevices,
            kAudioObjectPropertyScopeGlobal,
        ),
    )
    .map_err(|status| {
        AudioError::DeviceUnavailable(format!(
            "coreaudio: could not list devices (OSStatus {status})"
        ))
    })
}

/// A device's input streams; empty if it has none or they cannot be read.
fn input_stream_ids(id: AudioDeviceID) -> Vec<AudioObjectID> {
    get_ids(
        id,
        address(kAudioDevicePropertyStreams, kAudioObjectPropertyScopeInput),
    )
    .unwrap_or_default()
}

/// What a stream says it is connected to (`kAudioStreamTerminalType*`, or a
/// USB terminal type). Unreadable reads as 0, "unknown", like a stream that
/// reports nothing.
fn terminal_type(stream: AudioObjectID) -> u32 {
    get(
        stream,
        address(
            kAudioStreamPropertyTerminalType,
            kAudioObjectPropertyScopeGlobal,
        ),
        kAudioStreamTerminalTypeUnknown,
    )
    .unwrap_or(kAudioStreamTerminalTypeUnknown)
}

/// Whether an input stream is one of a running Voice Processing IO unit's
/// echo-reference taps rather than a microphone.
///
/// While VPIO runs in this process, every output device gains input streams
/// carrying what it plays. Measured: taps report the output terminal they
/// mirror ('spkr', 'hdph') or 0; real mics report 'micr', 'hmic' or a USB
/// input terminal such as 0x201. Only a device that has outputs can carry a
/// tap, so on an input-only device nothing is one — which is what keeps a
/// real mic that reports 0 listed.
///
/// Known limit: an unreadable terminal type counts as 0 (see
/// `terminal_type`), so a combined in+out device (one device ID) whose real
/// mic reports 0 is hidden from the macOS input dropdown while VPIO runs.
/// The default input and a `uia.toml` pin still reach it: opening resolves
/// against the unfiltered list.
fn is_echo_reference_tap(terminal_type: u32, device_has_outputs: bool) -> bool {
    const OUTPUT_TERMINALS: [u32; 5] = [
        kAudioStreamTerminalTypeUnknown,
        kAudioStreamTerminalTypeSpeaker,
        kAudioStreamTerminalTypeHeadphones,
        kAudioStreamTerminalTypeLFESpeaker,
        kAudioStreamTerminalTypeReceiverSpeaker,
    ];
    // USB Audio Class output terminal types.
    const USB_OUTPUT_TERMINALS: std::ops::RangeInclusive<u32> = 0x300..=0x3FF;
    device_has_outputs
        && (OUTPUT_TERMINALS.contains(&terminal_type)
            || USB_OUTPUT_TERMINALS.contains(&terminal_type))
}

/// Whether `id` has at least one input stream that is not an echo-reference
/// tap, i.e. is a microphone a user could pick. See `is_echo_reference_tap`
/// for the one kind of real mic this hides.
fn has_real_input(id: AudioDeviceID) -> bool {
    let has_outputs = channel_count(id, Direction::Output) > 0;
    input_stream_ids(id)
        .into_iter()
        .any(|stream| !is_echo_reference_tap(terminal_type(stream), has_outputs))
}

/// Names for a settings UI's `dir` device picker: no echo-reference taps
/// (inputs), no VPIO private aggregate, each name once, in CoreAudio's order.
///
/// Only for listing. Opening resolves against the unfiltered list (see
/// `resolve`), after the supervisor has waited for any torn-down unit's taps
/// to go away.
pub(crate) fn list_names(dir: Direction) -> Result<Vec<String>, AudioError> {
    let names = device_ids()?
        .into_iter()
        .filter(|&id| match dir {
            Direction::Input => has_real_input(id),
            Direction::Output => channel_count(id, Direction::Output) > 0,
        })
        .map(device_name)
        .filter(|name| !is_vpio_private(name))
        .collect();
    Ok(crate::dedupe_names(names))
}

/// Total channels a device has in `dir`; 0 means it is not a device of that
/// direction at all.
fn channel_count(id: AudioDeviceID, dir: Direction) -> u32 {
    let addr = address(kAudioDevicePropertyStreamConfiguration, dir.scope());
    let mut size = 0u32;
    // SAFETY: `addr` and `size` are valid for the call.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            id,
            NonNull::from(&addr),
            0,
            null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 || (size as usize) < size_of::<AudioBufferList>() {
        return 0;
    }
    // u64 storage so the list is 8-byte aligned, as its pointer field needs.
    let mut storage = vec![0u64; (size as usize).div_ceil(8)];
    // SAFETY: `storage` holds at least `size` bytes.
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            NonNull::from(&addr),
            0,
            null(),
            NonNull::from(&mut size),
            NonNull::from(&mut storage[..]).cast(),
        )
    };
    if status != 0 {
        return 0;
    }
    // Never trust more bytes than both CoreAudio reported and `storage` holds.
    let written = (size as usize).min(storage.len() * size_of::<u64>());
    let list = storage.as_ptr().cast::<AudioBufferList>();
    // SAFETY: `storage` is zero-initialised, 8-byte aligned and at least
    // `size_of::<AudioBufferList>()` bytes (checked against the first `size`
    // above), so reading the header field is in bounds of initialised memory.
    let declared = unsafe { (*list).mNumberBuffers };
    let count = buffers_that_fit(declared, written);
    // `addr_of!` avoids a reference to the 1-element `mBuffers` array, so the
    // pointer keeps `storage`'s whole-allocation provenance rather than one
    // `AudioBuffer`'s, which is what makes indexing past element 0 sound.
    // SAFETY: `list` points into `storage`; no reference is created.
    let first = unsafe { std::ptr::addr_of!((*list).mBuffers) }.cast::<AudioBuffer>();
    // SAFETY: `first` is aligned (AudioBuffer's alignment divides 8) and
    // `buffers_that_fit` guarantees `count` whole buffers lie within the
    // `written` bytes CoreAudio initialised in `storage`, which outlives
    // this slice.
    let buffers: &[AudioBuffer] = unsafe { std::slice::from_raw_parts(first, count) };
    buffers.iter().map(|b| b.mNumberChannels).sum()
}

/// How many of a list's `declared` buffers actually lie inside the `written`
/// bytes, so a count CoreAudio disagrees with is never read past the data.
fn buffers_that_fit(declared: u32, written: usize) -> usize {
    let room = written.saturating_sub(std::mem::offset_of!(AudioBufferList, mBuffers))
        / size_of::<AudioBuffer>();
    (declared as usize).min(room)
}

/// The same name cpal reports, so a name chosen in Settings resolves here.
fn device_name(id: AudioDeviceID) -> String {
    let addr = address(
        kAudioDevicePropertyDeviceNameCFString,
        kAudioObjectPropertyScopeGlobal,
    );
    let mut name: *mut CFString = std::ptr::null_mut();
    let mut size = size_of::<*mut CFString>() as u32;
    // SAFETY: the property is documented to write one CFString pointer.
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            NonNull::from(&addr),
            0,
            null(),
            NonNull::from(&mut size),
            NonNull::from(&mut name).cast(),
        )
    };
    match NonNull::new(name) {
        // SAFETY: returned under the create rule, so we own this reference.
        Some(name) if status == 0 => unsafe { CFRetained::from_raw(name) }.to_string(),
        _ => format!("<unnamed device {id}>"),
    }
}

fn list(dir: Direction) -> Result<Vec<DeviceInfo>, AudioError> {
    Ok(device_ids()?
        .into_iter()
        .filter(|&id| channel_count(id, dir) > 0)
        .map(|id| DeviceInfo {
            id,
            name: device_name(id),
        })
        .collect())
}

/// The current system default device for `dir`, if there is one.
pub(crate) fn default_device(dir: Direction) -> Option<AudioDeviceID> {
    get(
        SYSTEM,
        address(dir.default_selector(), kAudioObjectPropertyScopeGlobal),
        0,
    )
    .ok()
    .filter(|&id| id != 0)
}

/// The device `wanted` names in `dir` right now, or the current default.
pub(crate) fn resolve(dir: Direction, wanted: Option<&str>) -> Result<DeviceInfo, AudioError> {
    choose(&list(dir)?, default_device(dir), wanted, dir)
}

/// The private aggregate a running Voice Processing IO unit adds to this
/// process's device list (measured: `VPAUAggregateAudioDevice-0x<hex>`,
/// transport `'grup'`). Never a device a user chose.
fn is_vpio_private(name: &str) -> bool {
    name.starts_with("VPAUAggregateAudioDevice")
}

/// Whether a voice-processing unit's private aggregate is still in this
/// process's device list — i.e. a torn-down unit's echo-reference taps may
/// still be visible. A failed listing counts as "not present": the wait this
/// feeds is a best effort, never a reason to stop reopening.
pub(crate) fn vpio_aggregate_present() -> bool {
    device_ids().is_ok_and(|ids| ids.into_iter().any(|id| is_vpio_private(&device_name(id))))
}

fn nominal_rate_address() -> AudioObjectPropertyAddress {
    address(
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal,
    )
}

/// The rate a device runs at, or `None` if it cannot be read (a dead
/// device, say) or is not a positive number.
pub(crate) fn nominal_rate(id: AudioDeviceID) -> Option<f64> {
    get(id, nominal_rate_address(), 0.0f64)
        .ok()
        .filter(|rate| rate.is_finite() && *rate > 0.0)
}

/// The nominal rates a device supports, as `(min, max)` ranges (`min ==
/// max` for a discrete rate). Empty if they cannot be read.
pub(crate) fn available_rates(id: AudioDeviceID) -> Vec<(f64, f64)> {
    get_array(
        id,
        address(
            kAudioDevicePropertyAvailableNominalSampleRates,
            kAudioObjectPropertyScopeGlobal,
        ),
        AudioValueRange {
            mMinimum: 0.0,
            mMaximum: 0.0,
        },
    )
    .unwrap_or_default()
    .into_iter()
    .map(|r| (r.mMinimum, r.mMaximum))
    .collect()
}

/// Ask a device to run at `rate`. It may take a moment to read back; `Err`
/// carries the OSStatus.
pub(crate) fn set_nominal_rate(id: AudioDeviceID, rate: f64) -> Result<(), i32> {
    let addr = nominal_rate_address();
    // SAFETY: `rate` is the one f64 this property takes, valid for
    // `size_of::<f64>()` bytes for the call; `addr` lives for the call.
    let status = unsafe {
        AudioObjectSetPropertyData(
            id,
            NonNull::from(&addr),
            0,
            null(),
            size_of::<f64>() as u32,
            NonNull::from(&rate).cast(),
        )
    };
    if status == 0 { Ok(()) } else { Err(status) }
}

/// False once a device has been unplugged (or its ID never existed).
pub(crate) fn is_alive(id: AudioDeviceID) -> bool {
    get::<u32>(
        id,
        address(
            kAudioDevicePropertyDeviceIsAlive,
            kAudioObjectPropertyScopeGlobal,
        ),
        0,
    )
    .is_ok_and(|alive| alive != 0)
}

/// CoreAudio property listeners that wake the supervisor.
///
/// The listener only sends `Wake::Changed`; every decision and every rebuild
/// happens on the supervisor thread, never on CoreAudio's notification thread.
pub(crate) struct Listeners {
    registered: Vec<(AudioObjectID, AudioObjectPropertyAddress)>,
    client: *mut c_void,
}

/// SAFETY contract for `client`: a `&'static Sender<Wake>`, see `register`.
unsafe extern "C-unwind" fn on_change(
    _object: AudioObjectID,
    _count: u32,
    _addresses: NonNull<AudioObjectPropertyAddress>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is the leaked `&'static Sender<Wake>` from `register`,
    // so it is valid for as long as any notification can still arrive.
    let tx = unsafe { &*client.cast::<Sender<Wake>>() };
    let _ = tx.send(Wake::Changed);
    0
}

impl Listeners {
    /// Listen for the device list, both system defaults, and each bound
    /// device dying.
    ///
    /// `client` is `'static` on purpose: CoreAudio gives no guarantee that a
    /// notification already in flight has finished when
    /// `AudioObjectRemovePropertyListener` returns, so the sender it
    /// dereferences must never be freed. `open_voice_processing` leaks one
    /// per backend for exactly this.
    pub(crate) fn register(client: &'static Sender<Wake>, bound: &[AudioDeviceID]) -> Self {
        let mut this = Self {
            registered: Vec::new(),
            client: std::ptr::from_ref(client).cast_mut().cast(),
        };
        this.add(SYSTEM, kAudioHardwarePropertyDevices);
        this.add(SYSTEM, kAudioHardwarePropertyDefaultInputDevice);
        this.add(SYSTEM, kAudioHardwarePropertyDefaultOutputDevice);
        for &id in bound {
            this.add(id, kAudioDevicePropertyDeviceIsAlive);
        }
        this
    }

    fn add(&mut self, object: AudioObjectID, selector: AudioObjectPropertySelector) {
        let addr = address(selector, kAudioObjectPropertyScopeGlobal);
        // SAFETY: `on_change` matches the listener signature; `self.client`
        // satisfies its contract.
        let status = unsafe {
            AudioObjectAddPropertyListener(
                object,
                NonNull::from(&addr),
                Some(on_change),
                self.client,
            )
        };
        if status == 0 {
            self.registered.push((object, addr));
        } else {
            // Not fatal: the supervisor also re-checks on a timer.
            eprintln!(
                "uia-audio: coreaudio could not listen for property {selector:#x} on object {object} \
                 (OSStatus {status}); relying on the periodic check"
            );
        }
    }
}

impl Drop for Listeners {
    fn drop(&mut self) {
        for (object, addr) in &self.registered {
            // SAFETY: removing exactly what `add` registered.
            unsafe {
                AudioObjectRemovePropertyListener(
                    *object,
                    NonNull::from(addr),
                    Some(on_change),
                    self.client,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: AudioDeviceID, name: &str) -> DeviceInfo {
        DeviceInfo {
            id,
            name: name.to_string(),
        }
    }

    fn outputs() -> Vec<DeviceInfo> {
        vec![
            dev(112, "Odyssey G75F"),
            dev(107, "Creative Pebble X"),
            dev(81, "MacBook Pro Speakers"),
        ]
    }

    #[test]
    fn no_name_means_the_system_default() {
        let got = choose(&outputs(), Some(81), None, Direction::Output).unwrap();
        assert_eq!(got.id, 81);
    }

    /// The whole point of following the default: when it moves, so does an
    /// unpinned choice.
    #[test]
    fn an_unpinned_choice_follows_the_default_when_it_moves() {
        let before = choose(&outputs(), Some(81), None, Direction::Output).unwrap();
        let after = choose(&outputs(), Some(107), None, Direction::Output).unwrap();
        assert_ne!(before.id, after.id);
        assert_eq!(after.id, 107);
    }

    /// And a pinned name must not.
    #[test]
    fn a_pinned_name_ignores_the_default() {
        for default in [Some(81), Some(107), Some(112), None] {
            let got = choose(&outputs(), default, Some("pebble"), Direction::Output).unwrap();
            assert_eq!(got.id, 107, "default {default:?}");
        }
    }

    /// Same rule as every other lookup in this crate: case-insensitive
    /// substring, so a name pasted from Settings matches without its exact
    /// punctuation.
    #[test]
    fn names_match_case_insensitively_by_substring() {
        let got = choose(&outputs(), None, Some("macbook pro"), Direction::Output).unwrap();
        assert_eq!(got.id, 81);
    }

    #[test]
    fn an_unmatched_name_is_a_clear_error_naming_what_exists() {
        let err = choose(&outputs(), Some(81), Some("AirPods"), Direction::Output).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("no audio device matching"), "got: {text}");
        assert!(
            text.contains("Creative Pebble X"),
            "must list what exists: {text}"
        );
    }

    #[test]
    fn no_default_and_no_name_is_an_error_not_a_guess() {
        let err = choose(&outputs(), None, None, Direction::Output).unwrap_err();
        assert!(
            err.to_string().contains("no default output device"),
            "got: {err}"
        );
    }

    /// The private aggregate VPIO creates while it runs (measured name:
    /// `VPAUAggregateAudioDevice-0x<hex>`) is never a user's device.
    #[test]
    fn the_vpio_aggregate_is_recognised_by_name() {
        assert!(is_vpio_private("VPAUAggregateAudioDevice-0x600003a1c000"));
        assert!(is_vpio_private("VPAUAggregateAudioDevice"));
        assert!(!is_vpio_private("Creative Pebble X"));
        assert!(!is_vpio_private("MacBook Pro Speakers"));
        // A prefix, not a substring: a user's own aggregate is kept.
        assert!(!is_vpio_private("My VPAUAggregateAudioDevice"));
    }

    fn code(c: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*c)
    }

    /// Measured on real mics with VPIO running: C920 and Pebble mic report
    /// 'micr', the Jabra mic 'hmic', the MacBook mic 0x201 (USB-style
    /// "microphone"). Never taps, even on a device that also has outputs.
    #[test]
    fn real_microphones_are_not_taps() {
        for t in [code(b"micr"), code(b"hmic"), 0x201] {
            assert!(!is_echo_reference_tap(t, true), "{t:#x}");
        }
    }

    /// Measured taps: Pebble speaker 'spkr', Jabra speaker 'hdph', Odyssey
    /// HDMI and MacBook speakers 0.
    #[test]
    fn measured_speaker_taps_are_taps() {
        for t in [code(b"spkr"), code(b"hdph"), 0] {
            assert!(is_echo_reference_tap(t, true), "{t:#x}");
        }
    }

    /// The rest of the output-terminal family: LFE, receiver speaker, and
    /// USB output terminals (0x300..=0x3FF).
    #[test]
    fn other_output_terminals_are_taps() {
        for t in [code(b"lfes"), code(b"rspk"), 0x300, 0x301, 0x3FF] {
            assert!(is_echo_reference_tap(t, true), "{t:#x}");
        }
        assert!(!is_echo_reference_tap(0x400, true));
        assert!(!is_echo_reference_tap(0x2FF, true));
    }

    /// An input-only device has no speaker for VPIO to tap, so whatever its
    /// stream calls itself — even 0 — it is a real input.
    #[test]
    fn nothing_on_an_input_only_device_is_a_tap() {
        for t in [
            code(b"micr"),
            code(b"hmic"),
            0x201,
            code(b"spkr"),
            code(b"hdph"),
            0,
            0x301,
        ] {
            assert!(!is_echo_reference_tap(t, false), "{t:#x}");
        }
    }

    /// Aggregate and multi-stream USB devices report several buffers; the
    /// count read from the list must never reach past the bytes written.
    #[test]
    fn the_buffer_count_is_clamped_to_the_bytes_written() {
        let header = std::mem::offset_of!(AudioBufferList, mBuffers);
        let one = size_of::<AudioBuffer>();
        // Declared and written agree: all of them.
        assert_eq!(buffers_that_fit(3, header + 3 * one), 3);
        // Declares more than were written: only whole written buffers.
        assert_eq!(buffers_that_fit(5, header + 2 * one), 2);
        assert_eq!(buffers_that_fit(5, header + 2 * one + one - 1), 2);
        // Fewer bytes than the header: none, and no underflow.
        assert_eq!(buffers_that_fit(4, 0), 0);
        assert_eq!(buffers_that_fit(4, header), 0);
        // Declares fewer than would fit: the declared count.
        assert_eq!(buffers_that_fit(1, header + 4 * one), 1);
    }

    /// Review focus 3: "Creative Pebble X" is a microphone (id 103) AND a
    /// speaker (id 107). Each direction resolves from its own list, so the
    /// same name binds different devices per direction.
    #[test]
    fn same_name_devices_resolve_per_direction() {
        let inputs = vec![
            dev(90, "Jabra Evolve2 30 SE"),
            dev(103, "Creative Pebble X"),
        ];
        let input = choose(&inputs, Some(90), Some("Pebble"), Direction::Input).unwrap();
        let output = choose(&outputs(), Some(81), Some("Pebble"), Direction::Output).unwrap();
        assert_eq!((input.id, output.id), (103, 107));
    }
}
