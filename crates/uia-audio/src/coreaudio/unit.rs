// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! One Voice Processing IO audio unit: created, bound to a device per
//! direction, given a fixed 48 kHz mono client format, started — and torn
//! down in the right order on drop.
//!
//! Audio played through element 0 is the echo reference; audio rendered from
//! element 1 has it cancelled. The 2026-10-07 spike confirmed both elements
//! accept different devices and that 48 kHz mono f32 is accepted on both even
//! when a device runs at 16 kHz (VPIO converts).

use crate::live::{LiveFormat, PlaybackQueue};
use objc2_audio_toolbox::{
    AURenderCallbackStruct, AUVoiceIOOtherAudioDuckingConfiguration,
    AUVoiceIOOtherAudioDuckingLevel, AudioComponentDescription, AudioComponentFindNext,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioOutputUnitStart,
    AudioOutputUnitStop, AudioUnit, AudioUnitElement, AudioUnitGetProperty, AudioUnitInitialize,
    AudioUnitPropertyID, AudioUnitRender, AudioUnitRenderActionFlags, AudioUnitScope,
    AudioUnitSetProperty, AudioUnitUninitialize, kAUVoiceIOProperty_OtherAudioDuckingConfiguration,
    kAudioOutputUnitProperty_CurrentDevice, kAudioOutputUnitProperty_EnableIO,
    kAudioOutputUnitProperty_SetInputCallback, kAudioUnitManufacturer_Apple,
    kAudioUnitProperty_SetRenderCallback, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Global,
    kAudioUnitScope_Input, kAudioUnitScope_Output, kAudioUnitSubType_VoiceProcessingIO,
    kAudioUnitType_Output,
};
use objc2_core_audio::AudioDeviceID;
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, kAudio_ParamError,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM,
};
use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{NonNull, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use uia_core::audio::{AudioError, WaitingCell, f32_to_i16};

/// The app-side rate on both elements. Fixed: VPIO converts to whatever the
/// hardware runs at, so the formats stay put across a unit rebuild. They
/// change only while the unit is split onto cpal streams.
pub(crate) const CLIENT_RATE_HZ: u32 = 48_000;

/// The input callback's scratch capacity in frames, and nothing more: requests
/// beyond it are refused by `input_len`. Must NOT be applied as
/// `kAudioUnitProperty_MaximumFramesPerSlice`: VPIO adopts that value as its
/// actual callback period (measured: unset/512 → 10.7 ms, 1024 → 21.3 ms,
/// 4096 → 85.3 ms at 48 kHz), which would blow the 50 ms barge-in budget.
const MAX_FRAMES: u32 = 4096;
const OUTPUT_ELEMENT: AudioUnitElement = 0;
const INPUT_ELEMENT: AudioUnitElement = 1;

/// The capture channel's sending half. The supervisor owns the original and
/// each unit's callbacks hold a clone, so the channel closes — and the
/// source's `next_frame` returns `None` — whenever the supervisor thread
/// ends, normally or by panic.
pub(crate) type FramesTx = tokio::sync::mpsc::Sender<Vec<i16>>;

/// State shared by the callbacks, the supervisor, the cpal legs of a split
/// unit, and the source/sink. Outlives any one `VoiceUnit` or leg, which is
/// what lets queued audio survive a device reopen. Deliberately holds no
/// capture sender: the source and sink keep this alive, and that must not
/// keep the microphone channel open.
pub(crate) struct Shared {
    /// Filled by the sink, drained by the unit's render callback or, while
    /// split, the output leg. Also carries the level `CoreAudioSink::level`
    /// reads.
    pub(crate) playback: PlaybackQueue,
    /// What the source and sink report: [`CLIENT_RATE_HZ`] while a unit
    /// runs, the leg's device format while split.
    pub(crate) input_format: Arc<LiveFormat>,
    pub(crate) output_format: Arc<LiveFormat>,
    /// What each direction waits for, if anything.
    pub(crate) input_waiting: WaitingCell,
    pub(crate) output_waiting: WaitingCell,
    pub(crate) render_calls: AtomicU64,
    pub(crate) render_frames: AtomicU64,
}

impl Shared {
    pub(crate) fn new() -> Self {
        Self {
            playback: PlaybackQueue::default(),
            input_format: Arc::new(LiveFormat::new()),
            output_format: Arc::new(LiveFormat::new()),
            input_waiting: WaitingCell::default(),
            output_waiting: WaitingCell::default(),
            render_calls: AtomicU64::new(0),
            render_frames: AtomicU64::new(0),
        }
    }
}

/// What the callbacks receive as their refcon.
struct CallbackCtx {
    unit: AudioUnit,
    shared: Arc<Shared>,
    /// A clone of the supervisor's sender, dropped with the unit.
    frames_tx: FramesTx,
    /// Touched only by the input callback; `UnsafeCell` so the render
    /// callback's shared borrow of the same context never aliases a `&mut`.
    scratch: UnsafeCell<Vec<f32>>,
}

pub(crate) struct VoiceUnit {
    unit: AudioUnit,
    ctx: *mut CallbackCtx,
    initialized: bool,
    /// Element 1's input-scope (hardware-side) sample rate, 0 if unreadable.
    /// VPIO runs both sides at the slower device's rate (measured), so this
    /// reflects that device — after the supervisor has raised it where it
    /// can. Logged, because below 24 kHz (the engine's voice rate) the
    /// assistant's voice is band-limited.
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
    let _ = ctx.frames_tx.try_send(f32_to_i16(&scratch[..len]));
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
    for (index, buffer) in buffers.iter_mut().enumerate() {
        if buffer.mData.is_null() {
            continue;
        }
        // Never write more floats than the buffer can hold.
        let floats = (buffer.mDataByteSize / 4) as usize;
        // SAFETY: the buffer holds `mDataByteSize` bytes of f32 (our format).
        let out = unsafe { std::slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), floats) };
        if index == 0 {
            // The client format is mono (see `client_format`).
            ctx.shared.playback.try_fill(out, 1);
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
        frames_tx: &FramesTx,
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
            frames_tx: frames_tx.clone(),
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
