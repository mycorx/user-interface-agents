// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! The device-open sequence both the capture and render sides share.
//!
//! Factored out because the two example spikes
//! (`examples/wasapi_aec_poc.rs`, `examples/wasapi_aec_ab_test.rs`) each
//! duplicated it near-verbatim, and the one step that actually matters is easy
//! to lose in that duplication: **`SetClientProperties` must be called before
//! `Initialize`**. That ordering is what makes Windows treat the stream as a
//! call and apply its own AEC/AGC/NS beneath us; reversed, everything still
//! "works" and no echo cancellation happens at all.

use std::mem::size_of;
use uia_core::audio::AudioError;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMOPTIONS_NONE,
    AudioCategory_Communications, AudioClientProperties, DEVICE_STATE_ACTIVE, EDataFlow,
    IAudioClient2, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, eCommunications,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, STGM_READ};
use windows::Win32::System::Threading::CreateEventW;
use windows::core::PCWSTR;

/// 20 ms — deliberately NOT the 200 ms the two example spikes use.
///
/// This is a floor request (WASAPI rounds up to at least two device periods,
/// typically 10 ms each in shared mode), and on the render side it directly
/// bounds barge-in: `WasapiSink::clear()` can only drop what is still in our
/// own software queue, never what has already been handed to WASAPI via
/// `ReleaseBuffer`. At 200 ms, up to a fifth of a second of the assistant's
/// voice would keep playing after the user interrupts — PLAN.md's frozen
/// decision gives the sink 50 ms. 20 ms is both inside that budget and the
/// canonical event-driven double-buffer size.
pub(crate) const BUFFER_DURATION_HNS: i64 = 20 * 10_000;

/// A WASAPI event `HANDLE` that can cross a thread boundary.
///
/// `HANDLE` is `*mut c_void`, so it is neither `Send` nor `Sync`, which would
/// make any struct holding one fail the `AudioSource`/`AudioSink: Send`
/// bound. A kernel event handle is just an opaque process-wide object id —
/// signalling one from another thread is exactly what it exists for — so
/// carrying it across threads is sound even though the raw pointer type
/// cannot say so.
#[derive(Clone, Copy)]
pub(crate) struct EventHandle(pub(crate) HANDLE);

// SAFETY: see the type's doc comment — a kernel event handle is not thread
// affine, and every use site here either signals it (`SetEvent`), waits on it,
// or closes it exactly once after joining the only thread that waits.
unsafe impl Send for EventHandle {}
unsafe impl Sync for EventHandle {}

/// An opened, initialised, not-yet-started WASAPI client.
pub(crate) struct OpenedClient {
    pub(crate) client: IAudioClient2,
    pub(crate) event: EventHandle,
    pub(crate) sample_rate_hz: u32,
    pub(crate) channels: u16,
    /// The endpoint's own name (e.g. "Headset Microphone (Realtek Audio)"),
    /// read once at open time so callers can log which physical device
    /// `eCommunications` actually resolved to. `open_communications_client`'s
    /// doc comment already notes this can differ from the console/default
    /// device a user expects — this is what lets that be confirmed rather
    /// than guessed when someone reports "wrong device".
    pub(crate) device_name: String,
}

/// Open an `eCommunications`-role endpoint for `direction` and initialise it
/// in shared, event-driven mode under `AudioCategory_Communications`.
///
/// `device_name_filter`, when `Some`, picks the first active endpoint whose
/// friendly name contains it (case-insensitive) instead of asking Windows for
/// its `eCommunications` default. This exists because that default is not
/// always trustworthy: a real case surfaced Windows silently holding the
/// render communications role on a monitor's embedded HDMI audio device
/// despite the user's actual speakers being marked default in Sound
/// settings, with no visible conflict in the UI to explain it. An explicit
/// override in `uia.toml` (`audio.input_device`/`audio.output_device`)
/// sidesteps trusting that role assignment at all.
///
/// Must be called on the thread that will drive the client: COM is
/// apartment-scoped, and WASAPI does not promise that concurrent calls into
/// one client are safe.
///
/// # Safety
/// The caller must have initialised COM on this thread (`CoInitializeEx`) and
/// must keep it initialised for as long as the returned client is used.
/// `reuse_event`, when `Some`, attaches that existing wake event to the new
/// client instead of creating one.
///
/// This is what lets a worker reopen a device that went away without its
/// wake handle ever changing. The shutdown protocol in [`super::stop_worker`]
/// signals a handle the *struct* holds and closes it after joining; if a
/// reopen swapped in a fresh handle, that signal would land on the old one
/// and the worker would sleep through its own shutdown.
pub(crate) unsafe fn open_communications_client(
    direction: EDataFlow,
    device_name_filter: Option<&str>,
    reuse_event: Option<EventHandle>,
) -> Result<OpenedClient, AudioError> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| device_error("could not create the device enumerator", e))?;

        let device = match device_name_filter {
            Some(name) => find_device_by_name(&enumerator, direction, name)?,
            // `eCommunications`, not `eConsole`: this is the role Windows lets
            // a user point at their headset for calls, and it is the role the
            // OS audio processing is keyed to. It may well resolve to a
            // *different* physical device than the console default cpal
            // opens — that is intended, and called out for testers in
            // docs/MANUAL-TEST.md.
            None => enumerator
                .GetDefaultAudioEndpoint(direction, eCommunications)
                .map_err(|e| device_error("no default communications endpoint", e))?,
        };
        let device_name = friendly_name(&device);
        let client: IAudioClient2 = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| device_error("could not activate the audio client", e))?;

        // THE load-bearing call. Before `Initialize`, always.
        client
            .SetClientProperties(&AudioClientProperties {
                cbSize: size_of::<AudioClientProperties>() as u32,
                bIsOffload: false.into(),
                eCategory: AudioCategory_Communications,
                Options: AUDCLNT_STREAMOPTIONS_NONE,
            })
            .map_err(|e| device_error("could not request the Communications category", e))?;

        let mix_format = client
            .GetMixFormat()
            .map_err(|e| device_error("could not read the mix format", e))?;
        validate_float_format(mix_format)?;
        let sample_rate_hz = (*mix_format).nSamplesPerSec;
        let channels = (*mix_format).nChannels;

        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                BUFFER_DURATION_HNS,
                // Periodicity must be 0 in shared mode.
                0,
                mix_format,
                None,
            )
            .map_err(|e| device_error("could not initialise the audio client", e))?;

        let event = match reuse_event {
            Some(existing) => existing.0,
            None => CreateEventW(None, false, false, PCWSTR::null())
                .map_err(|e| device_error("could not create the wake event", e))?,
        };
        client
            .SetEventHandle(event)
            .map_err(|e| device_error("could not attach the wake event", e))?;

        Ok(OpenedClient {
            client,
            event: EventHandle(event),
            sample_rate_hz,
            channels,
            device_name,
        })
    }
}

/// Find the first active `direction` endpoint whose friendly name contains
/// `name` (case-insensitive substring, not exact match — so a user can paste
/// the name Sound settings shows without worrying about exact punctuation).
///
/// Fails loudly with the list of what *is* available on a miss: a config
/// typo silently falling back to some other device would recreate exactly
/// the "which device is this actually using" confusion `friendly_name`
/// logging exists to solve, and a caller deserves to be told rather than to
/// debug silence.
unsafe fn find_device_by_name(
    enumerator: &IMMDeviceEnumerator,
    direction: EDataFlow,
    name: &str,
) -> Result<IMMDevice, AudioError> {
    unsafe {
        let collection = enumerator
            .EnumAudioEndpoints(direction, DEVICE_STATE_ACTIVE)
            .map_err(|e| device_error("could not enumerate audio endpoints", e))?;
        let count = collection
            .GetCount()
            .map_err(|e| device_error("could not count audio endpoints", e))?;

        let mut available = Vec::with_capacity(count as usize);
        for i in 0..count {
            let device = collection
                .Item(i)
                .map_err(|e| device_error("could not read an audio endpoint", e))?;
            let device_name = friendly_name(&device);
            if crate::device_name_matches(&device_name, name) {
                return Ok(device);
            }
            available.push(device_name);
        }

        Err(AudioError::DeviceUnavailable(format!(
            "wasapi: no audio device matching {name:?} among the active endpoints: {available:?}"
        )))
    }
}

/// Best-effort endpoint name for diagnostics — never fails the open. A
/// device whose name can't be read still works fine for audio; only the log
/// line loses detail, which is not worth failing over.
unsafe fn friendly_name(device: &IMMDevice) -> String {
    unsafe {
        let Ok(props) = device.OpenPropertyStore(STGM_READ) else {
            return "<unknown device>".to_string();
        };
        let Ok(variant) = props.GetValue(&PKEY_Device_FriendlyName) else {
            return "<unknown device>".to_string();
        };
        let Ok(pwstr) = PropVariantToStringAlloc(&variant) else {
            return "<unknown device>".to_string();
        };
        let name = pwstr
            .to_string()
            .unwrap_or_else(|_| "<unreadable device name>".to_string());
        CoTaskMemFree(Some(pwstr.0 as _));
        name
    }
}

/// Reject anything that is not 32-bit IEEE float, at open time.
///
/// Both example spikes instead branch on `wBitsPerSample == 32` inside their
/// per-packet loop and silently treat anything else as "no data". That is
/// fine for a diagnostic that prints levels; here it would mean a stream of
/// zeroed or misread audio forever, with no error anywhere. Shared-mode WASAPI
/// hands out float on every machine this has been checked against — so this
/// should never fire, and if it does the caller deserves to be told rather
/// than to debug silence.
unsafe fn validate_float_format(format: *const WAVEFORMATEX) -> Result<(), AudioError> {
    unsafe {
        let bits = (*format).wBitsPerSample;
        if bits != 32 {
            return Err(AudioError::DeviceUnavailable(format!(
                "wasapi: expected a 32-bit float mix format, got {bits}-bit"
            )));
        }
        if (*format).wFormatTag as u32 == WAVE_FORMAT_EXTENSIBLE {
            let extensible = format as *const WAVEFORMATEXTENSIBLE;
            // `WAVEFORMATEXTENSIBLE` is packed, so the GUID cannot be
            // referenced in place (comparing by `!=` would take a reference to
            // a possibly-misaligned field, which is UB even unread) — copy it
            // out first.
            let subformat = std::ptr::addr_of!((*extensible).SubFormat).read_unaligned();
            if subformat != KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                return Err(AudioError::DeviceUnavailable(
                    "wasapi: expected an IEEE-float mix format subtype".to_string(),
                ));
            }
        }
        Ok(())
    }
}

fn device_error(what: &str, e: windows::core::Error) -> AudioError {
    AudioError::DeviceUnavailable(format!("wasapi: {what}: {e}"))
}
