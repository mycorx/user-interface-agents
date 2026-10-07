// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! One Voice Processing IO audio unit: created, bound to a device per
//! direction, given a fixed 48 kHz mono client format, started — and torn
//! down in the right order on drop.
//!
//! Audio played through element 0 is the echo reference; audio rendered from
//! element 1 has it cancelled. The 2026-10-07 spike confirmed both elements
//! accept different devices and that 48 kHz mono f32 is accepted on both even
//! when a device runs at 16 kHz (VPIO converts).

use objc2_audio_toolbox::{
    AURenderCallbackStruct, AUVoiceIOOtherAudioDuckingConfiguration,
    AUVoiceIOOtherAudioDuckingLevel, AudioComponentDescription, AudioComponentFindNext,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioOutputUnitStart,
    AudioOutputUnitStop, AudioUnit, AudioUnitElement, AudioUnitGetProperty, AudioUnitInitialize,
    AudioUnitPropertyID, AudioUnitRender, AudioUnitRenderActionFlags, AudioUnitScope,
    AudioUnitSetProperty, AudioUnitUninitialize, kAUVoiceIOProperty_OtherAudioDuckingConfiguration,
    kAudioOutputUnitProperty_CurrentDevice, kAudioOutputUnitProperty_EnableIO,
    kAudioOutputUnitProperty_SetInputCallback, kAudioUnitManufacturer_Apple,
    kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitProperty_SetRenderCallback,
    kAudioUnitProperty_StreamFormat, kAudioUnitScope_Global, kAudioUnitScope_Input,
    kAudioUnitScope_Output, kAudioUnitSubType_VoiceProcessingIO, kAudioUnitType_Output,
};
use objc2_core_audio::AudioDeviceID;
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, kAudio_ParamError,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM,
};
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{NonNull, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use uia_core::audio::{AudioError, f32_to_i16};

/// The app-side rate on both elements. Fixed: VPIO converts to whatever the
/// hardware runs at, so `format()` never changes, even across a reopen.
pub(crate) const CLIENT_RATE_HZ: u32 = 48_000;

/// Upper bound on frames per callback; also the input scratch size.
const MAX_FRAMES: u32 = 4096;
const OUTPUT_ELEMENT: AudioUnitElement = 0;
const INPUT_ELEMENT: AudioUnitElement = 1;

/// State shared by the callbacks, the supervisor, and the source/sink.
/// Outlives any one `VoiceUnit`, which is what lets queued audio and the
/// capture channel survive a device reopen.
pub(crate) struct Shared {
    pub(crate) queue: Mutex<VecDeque<i16>>,
    pub(crate) frames_tx: tokio::sync::mpsc::Sender<Vec<i16>>,
    pub(crate) render_calls: AtomicU64,
    pub(crate) render_frames: AtomicU64,
}

impl Shared {
    pub(crate) fn new(frames_tx: tokio::sync::mpsc::Sender<Vec<i16>>) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            frames_tx,
            render_calls: AtomicU64::new(0),
            render_frames: AtomicU64::new(0),
        }
    }
}

/// What the callbacks receive as their refcon.
struct CallbackCtx {
    unit: AudioUnit,
    shared: Arc<Shared>,
    /// Touched only by the input callback; `UnsafeCell` so the render
    /// callback's shared borrow of the same context never aliases a `&mut`.
    scratch: UnsafeCell<Vec<f32>>,
}

pub(crate) struct VoiceUnit {
    unit: AudioUnit,
    ctx: *mut CallbackCtx,
    initialized: bool,
    /// The rate VPIO actually processes at: the lower of the two devices'.
    /// Logged, because below 48 kHz the assistant's voice is band-limited.
    pub(crate) processing_rate_hz: u32,
}

/// Mono 48 kHz float, non-interleaved: one buffer per callback.
fn client_format() -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: f64::from(CLIENT_RATE_HZ),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat
            | kAudioFormatFlagIsPacked
            | kAudioFormatFlagIsNonInterleaved,
        mBytesPerPacket: 4,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4,
        mChannelsPerFrame: 1,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

/// "OSStatus -50", or "OSStatus 1852797029 'nope'" when the status is a
/// printable four-char code, which is how most AudioUnit errors are spelled.
pub(crate) fn describe_status(status: i32) -> String {
    let bytes = status.to_be_bytes();
    if bytes.iter().all(|b| (0x20..0x7f).contains(b)) {
        format!("OSStatus {status} '{}'", String::from_utf8_lossy(&bytes))
    } else {
        format!("OSStatus {status}")
    }
}

fn check(status: i32, what: &str) -> Result<(), AudioError> {
    if status == 0 {
        Ok(())
    } else {
        Err(AudioError::DeviceUnavailable(format!(
            "coreaudio: {what} failed ({})",
            describe_status(status)
        )))
    }
}

fn set<T>(
    unit: AudioUnit,
    id: AudioUnitPropertyID,
    scope: AudioUnitScope,
    element: AudioUnitElement,
    value: &T,
    what: &str,
) -> Result<(), AudioError> {
    // SAFETY: `value` is a valid `T` of `size_of::<T>()` bytes for the call.
    let status = unsafe {
        AudioUnitSetProperty(
            unit,
            id,
            scope,
            element,
            std::ptr::from_ref(value).cast(),
            size_of::<T>() as u32,
        )
    };
    check(status, what)
}

/// How many frames the input callback may render into a scratch buffer of
/// `capacity`, or `None` when the request does not fit.
pub(crate) fn input_len(frames: u32, capacity: usize) -> Option<usize> {
    usize::try_from(frames).ok().filter(|&n| n <= capacity)
}

/// Fill `out` from the playback queue, padding with silence on underrun.
/// `None` means the lock was contended: the whole period is silence.
pub(crate) fn drain_into(queue: Option<&mut VecDeque<i16>>, out: &mut [f32]) {
    match queue {
        Some(queue) => {
            for slot in out {
                *slot = queue.pop_front().map_or(0.0, |s| f32::from(s) / 32768.0);
            }
        }
        None => out.fill(0.0),
    }
}

/// Element 1's callback: pull the processed (echo-cancelled) microphone
/// audio and hand it to the source. Never blocks: a full channel drops this
/// frame, the same policy as `CpalSource` and `WasapiSource`.
///
/// # Safety
///
/// Called only by CoreAudio, with `refcon` the `CallbackCtx` the unit was
/// configured with.
unsafe extern "C-unwind" fn input_callback(
    refcon: NonNull<c_void>,
    flags: NonNull<AudioUnitRenderActionFlags>,
    timestamp: NonNull<AudioTimeStamp>,
    bus: u32,
    frames: u32,
    _io: *mut AudioBufferList,
) -> i32 {
    // SAFETY: refcon is the `CallbackCtx` this unit was configured with; it
    // is freed only after the unit is disposed.
    let ctx = unsafe { refcon.cast::<CallbackCtx>().as_ref() };
    // SAFETY: only this callback ever touches `scratch`.
    let scratch = unsafe { &mut *ctx.scratch.get() };
    let Some(len) = input_len(frames, scratch.len()) else {
        return kAudio_ParamError;
    };
    let mut list = AudioBufferList {
        mNumberBuffers: 1,
        mBuffers: [AudioBuffer {
            mNumberChannels: 1,
            // `frames <= MAX_FRAMES` (checked above), so this cannot overflow.
            mDataByteSize: frames * 4,
            mData: scratch.as_mut_ptr().cast(),
        }],
    };
    // SAFETY: `list` points at `len` writable floats, matching `frames`.
    let status = unsafe {
        AudioUnitRender(
            ctx.unit,
            flags.as_ptr(),
            timestamp,
            bus,
            frames,
            NonNull::from(&mut list),
        )
    };
    if status != 0 {
        return status;
    }
    let _ = ctx.shared.frames_tx.try_send(f32_to_i16(&scratch[..len]));
    0
}

/// Element 0's callback: what to play, which is also the echo reference.
///
/// # Safety
///
/// Called only by CoreAudio, with `refcon` the `CallbackCtx` the unit was
/// configured with and `io` null or a valid buffer list for the call.
unsafe extern "C-unwind" fn render_callback(
    refcon: NonNull<c_void>,
    _flags: NonNull<AudioUnitRenderActionFlags>,
    _timestamp: NonNull<AudioTimeStamp>,
    _bus: u32,
    frames: u32,
    io: *mut AudioBufferList,
) -> i32 {
    // SAFETY: as in `input_callback`; this callback only reads `shared`.
    let ctx = unsafe { refcon.cast::<CallbackCtx>().as_ref() };
    if io.is_null() {
        return 0;
    }
    // SAFETY: `io` is non-null and valid for the call; reading the header
    // field creates no reference to the list.
    let count = unsafe { (*io).mNumberBuffers } as usize;
    // `addr_of_mut!` avoids a reference to the 1-element `mBuffers` array, so
    // the pointer keeps the caller's whole-list provenance rather than one
    // `AudioBuffer`'s, which is what makes indexing past element 0 sound.
    // SAFETY: `io` is valid; no reference is created.
    let first = unsafe { std::ptr::addr_of_mut!((*io).mBuffers) }.cast::<AudioBuffer>();
    // SAFETY: CoreAudio's list holds `mNumberBuffers` buffers (C's flexible
    // array), valid and exclusively ours for the duration of the call.
    let buffers = unsafe { std::slice::from_raw_parts_mut(first, count) };
    let mut queue = ctx.shared.queue.try_lock().ok();
    for (index, buffer) in buffers.iter_mut().enumerate() {
        if buffer.mData.is_null() {
            continue;
        }
        // Never write more floats than the buffer can hold.
        let floats = (buffer.mDataByteSize / 4) as usize;
        // SAFETY: the buffer holds `mDataByteSize` bytes of f32 (our format).
        let out = unsafe { std::slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), floats) };
        if index == 0 {
            drain_into(queue.as_deref_mut(), out);
        } else {
            out.fill(0.0);
        }
    }
    ctx.shared.render_calls.fetch_add(1, Ordering::Relaxed);
    ctx.shared
        .render_frames
        .fetch_add(u64::from(frames), Ordering::Relaxed);
    0
}

impl VoiceUnit {
    /// Create, configure and start a VPIO unit bound to `input` and `output`.
    pub(crate) fn open(
        input: AudioDeviceID,
        output: AudioDeviceID,
        shared: &Arc<Shared>,
    ) -> Result<Self, AudioError> {
        let desc = AudioComponentDescription {
            componentType: kAudioUnitType_Output,
            componentSubType: kAudioUnitSubType_VoiceProcessingIO,
            componentManufacturer: kAudioUnitManufacturer_Apple,
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        // SAFETY: `desc` is valid for the call; null starts the search.
        let component = unsafe { AudioComponentFindNext(null_mut(), NonNull::from(&desc)) };
        if component.is_null() {
            return Err(AudioError::DeviceUnavailable(
                "coreaudio: the VoiceProcessingIO audio unit is not available".to_string(),
            ));
        }
        let mut unit: AudioUnit = null_mut();
        // SAFETY: `component` is non-null; `unit` is a valid out-pointer.
        check(
            unsafe { AudioComponentInstanceNew(component, NonNull::from(&mut unit)) },
            "creating the VoiceProcessingIO unit",
        )?;
        let ctx = Box::into_raw(Box::new(CallbackCtx {
            unit,
            shared: Arc::clone(shared),
            scratch: UnsafeCell::new(vec![0.0; MAX_FRAMES as usize]),
        }));
        // Built before configuring, so every failure below is cleaned up by Drop.
        let mut this = Self {
            unit,
            ctx,
            initialized: false,
            processing_rate_hz: 0,
        };
        this.configure(input, output)?;
        Ok(this)
    }

    fn configure(&mut self, input: AudioDeviceID, output: AudioDeviceID) -> Result<(), AudioError> {
        let unit = self.unit;
        let on: u32 = 1;
        set(
            unit,
            kAudioOutputUnitProperty_EnableIO,
            kAudioUnitScope_Input,
            INPUT_ELEMENT,
            &on,
            "enabling input",
        )?;
        set(
            unit,
            kAudioOutputUnitProperty_EnableIO,
            kAudioUnitScope_Output,
            OUTPUT_ELEMENT,
            &on,
            "enabling output",
        )?;
        set(
            unit,
            kAudioOutputUnitProperty_CurrentDevice,
            kAudioUnitScope_Global,
            INPUT_ELEMENT,
            &input,
            "binding the input device",
        )?;
        set(
            unit,
            kAudioOutputUnitProperty_CurrentDevice,
            kAudioUnitScope_Global,
            OUTPUT_ELEMENT,
            &output,
            "binding the output device",
        )?;
        let format = client_format();
        set(
            unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Output,
            INPUT_ELEMENT,
            &format,
            "setting the capture format",
        )?;
        set(
            unit,
            kAudioUnitProperty_StreamFormat,
            kAudioUnitScope_Input,
            OUTPUT_ELEMENT,
            &format,
            "setting the playback format",
        )?;
        set(
            unit,
            kAudioUnitProperty_MaximumFramesPerSlice,
            kAudioUnitScope_Global,
            0,
            &MAX_FRAMES,
            "setting the maximum frames per callback",
        )?;
        let render = AURenderCallbackStruct {
            inputProc: Some(render_callback),
            inputProcRefCon: self.ctx.cast(),
        };
        set(
            unit,
            kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Input,
            OUTPUT_ELEMENT,
            &render,
            "installing the render callback",
        )?;
        let capture = AURenderCallbackStruct {
            inputProc: Some(input_callback),
            inputProcRefCon: self.ctx.cast(),
        };
        set(
            unit,
            kAudioOutputUnitProperty_SetInputCallback,
            kAudioUnitScope_Global,
            INPUT_ELEMENT,
            &capture,
            "installing the input callback",
        )?;

        // Keep other apps' audio as loud as VPIO allows. macOS 14+; on older
        // systems the property is unknown, which is not a reason to fail.
        let ducking = AUVoiceIOOtherAudioDuckingConfiguration {
            mEnableAdvancedDucking: 1,
            mDuckingLevel: AUVoiceIOOtherAudioDuckingLevel::Min,
        };
        if let Err(e) = set(
            unit,
            kAUVoiceIOProperty_OtherAudioDuckingConfiguration,
            kAudioUnitScope_Global,
            0,
            &ducking,
            "minimising other-app ducking",
        ) {
            eprintln!("uia-audio: {e}; other apps may be ducked while the assistant listens");
        }

        // SAFETY: fully configured above.
        check(
            unsafe { AudioUnitInitialize(unit) },
            "initialising the VoiceProcessingIO unit",
        )?;
        self.initialized = true;
        self.processing_rate_hz = self.device_side_rate();
        // SAFETY: initialised.
        check(
            unsafe { AudioOutputUnitStart(unit) },
            "starting voice processing",
        )?;
        Ok(())
    }

    /// The input element's hardware-side rate, or 0 if it cannot be read.
    fn device_side_rate(&self) -> u32 {
        let mut format = client_format();
        let mut size = size_of::<AudioStreamBasicDescription>() as u32;
        // SAFETY: `format`/`size` are a valid out-buffer for this property.
        let status = unsafe {
            AudioUnitGetProperty(
                self.unit,
                kAudioUnitProperty_StreamFormat,
                kAudioUnitScope_Input,
                INPUT_ELEMENT,
                NonNull::from(&mut format).cast(),
                NonNull::from(&mut size),
            )
        };
        if status == 0 {
            format.mSampleRate as u32
        } else {
            0
        }
    }
}

impl Drop for VoiceUnit {
    fn drop(&mut self) {
        // Order matters: stop the callbacks, dispose the unit, and only then
        // free the context they dereference.
        // SAFETY: `unit` came from AudioComponentInstanceNew and is disposed
        // exactly once; `ctx` came from Box::into_raw and is freed once,
        // after no callback can run.
        unsafe {
            AudioOutputUnitStop(self.unit);
            if self.initialized {
                AudioUnitUninitialize(self.unit);
            }
            AudioComponentInstanceDispose(self.unit);
            drop(Box::from_raw(self.ctx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draining_converts_queued_samples_to_float() {
        let mut q: VecDeque<i16> = [16384, -32768].into_iter().collect();
        let mut out = [9.0f32; 2];
        drain_into(Some(&mut q), &mut out);
        assert_eq!(out, [0.5, -1.0]);
        assert!(q.is_empty());
    }

    /// Underrun pads with silence rather than leaving stale samples or
    /// stalling the device.
    #[test]
    fn an_underrun_pads_with_silence() {
        let mut q: VecDeque<i16> = [16384].into_iter().collect();
        let mut out = [9.0f32; 3];
        drain_into(Some(&mut q), &mut out);
        assert_eq!(out, [0.5, 0.0, 0.0]);
    }

    /// Review focus 5: `write`/`clear` holding the lock when the callback
    /// fires means silence for one period, never a blocked audio thread.
    #[test]
    fn a_contended_queue_plays_silence() {
        let mut out = [9.0f32; 4];
        drain_into(None, &mut out);
        assert_eq!(out, [0.0; 4]);
    }

    /// Review focus 4: CoreAudio asking for more than the scratch buffer
    /// holds is refused, never written past the end.
    #[test]
    fn oversized_input_requests_are_refused() {
        assert_eq!(input_len(512, 4096), Some(512));
        assert_eq!(input_len(4096, 4096), Some(4096));
        assert_eq!(input_len(4097, 4096), None);
        assert_eq!(input_len(u32::MAX, 4096), None);
    }

    #[test]
    fn failures_name_printable_four_char_codes() {
        // 'nope' as a big-endian four-char code.
        let status = i32::from_be_bytes(*b"nope");
        assert_eq!(describe_status(status), format!("OSStatus {status} 'nope'"));
        // -50 is not printable as four characters.
        assert_eq!(describe_status(-50), "OSStatus -50");
    }
}
