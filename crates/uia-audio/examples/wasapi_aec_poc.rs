// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Standalone proof-of-concept: does opening the microphone with WASAPI's
//! `AudioCategory_Communications` actually engage Windows' own AEC/AGC/NS,
//! with none of `webrtc-audio-processing`'s bundled C++ toolchain in the way?
//!
//! Deliberately NOT wired into `uia-audio`'s public API yet — see
//! `LEDGER.md`'s S15 update (2026-08-20) for why `--features aec` doesn't
//! build on Windows at all right now. This is step one of evaluating the
//! WASAPI-native alternative before committing to building a full
//! `AudioSource`/`AudioSink` pair around it.
//!
//! Run it, then talk near your speakers while something else plays audio
//! (music, a video) in another app: if Windows' Settings → System → Sound →
//! Communications is set to reduce other apps' volume during a call, that
//! ducking kicking in the moment this starts capturing is the observable
//! proof the Communications category was actually applied. AEC quality
//! itself isn't something a console print can show — this only confirms the
//! platform is treating the stream as a call at all.
//!
//! `cargo run -p uia-audio --example wasapi_aec_poc --features wasapi-aec`

#[cfg(all(windows, feature = "wasapi-aec"))]
fn main() -> windows::core::Result<()> {
    wasapi_poc::run()
}

#[cfg(not(all(windows, feature = "wasapi-aec")))]
fn main() {
    eprintln!(
        "this example only builds on Windows with --features wasapi-aec: \
         cargo run -p uia-audio --example wasapi_aec_poc --features wasapi-aec"
    );
}

#[cfg(all(windows, feature = "wasapi-aec"))]
mod wasapi_poc {
    use std::mem::size_of;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Media::Audio::{
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMOPTIONS_NONE,
        AudioCategory_Communications, AudioClientProperties, IAudioCaptureClient, IAudioClient2,
        IMMDeviceEnumerator, MMDeviceEnumerator, eCapture, eCommunications,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows::core::PCWSTR;

    const RUN_FOR: Duration = Duration::from_secs(15);
    /// WASAPI wants buffer duration in 100-ns units; 200 ms is a generous,
    /// unremarkable choice for a diagnostic that isn't chasing low latency.
    const BUFFER_DURATION_HNS: i64 = 200 * 10_000;

    pub fn run() -> windows::core::Result<()> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            let result = run_capture();
            CoUninitialize();
            result
        }
    }

    unsafe fn run_capture() -> windows::core::Result<()> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eCapture, eCommunications)?;
            let audio_client: IAudioClient2 = device.Activate(CLSCTX_ALL, None)?;

            // The step that matters: request the Communications category
            // BEFORE Initialize. This is what makes Windows treat this
            // stream as a call and apply its own AEC/AGC/NS ahead of
            // anything uia-audio does itself.
            let props = AudioClientProperties {
                cbSize: size_of::<AudioClientProperties>() as u32,
                bIsOffload: false.into(),
                eCategory: AudioCategory_Communications,
                Options: AUDCLNT_STREAMOPTIONS_NONE,
            };
            audio_client.SetClientProperties(&props)?;
            println!("SetClientProperties(AudioCategory_Communications): ok");

            let mix_format = audio_client.GetMixFormat()?;
            let sample_rate = (*mix_format).nSamplesPerSec;
            let channels = (*mix_format).nChannels;
            let bits = (*mix_format).wBitsPerSample;
            println!("negotiated mix format: {sample_rate} Hz, {channels} ch, {bits}-bit");

            audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                BUFFER_DURATION_HNS,
                0,
                mix_format,
                None,
            )?;

            let event = CreateEventW(None, false, false, PCWSTR::null())?;
            audio_client.SetEventHandle(event)?;
            let capture_client: IAudioCaptureClient = audio_client.GetService()?;

            audio_client.Start()?;
            println!(
                "capturing for {}s — talk near your speakers while something else \
                 plays audio in another app. Watch for Windows to duck that other \
                 app's volume (Settings > System > Sound > Communications) as the \
                 sign the platform is treating this as a call.",
                RUN_FOR.as_secs()
            );

            let start = Instant::now();
            while start.elapsed() < RUN_FOR {
                let _ = WaitForSingleObject(event, 2000);
                loop {
                    let packet_frames = capture_client.GetNextPacketSize()?;
                    if packet_frames == 0 {
                        break;
                    }
                    let mut data_ptr = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    capture_client.GetBuffer(&mut data_ptr, &mut frames, &mut flags, None, None)?;
                    // Mix format is float in shared mode on every machine this
                    // has been checked against; anything else just prints
                    // nothing rather than misreading the buffer.
                    let peak = if bits == 32 && !data_ptr.is_null() {
                        let samples = std::slice::from_raw_parts(
                            data_ptr as *const f32,
                            (frames * channels as u32) as usize,
                        );
                        samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()))
                    } else {
                        0.0
                    };
                    capture_client.ReleaseBuffer(frames)?;
                    if peak > 0.02 {
                        println!("level: {peak:.3}");
                    }
                }
            }

            audio_client.Stop()?;
            CloseHandle(event)?;
            println!("done.");
            Ok(())
        }
    }
}
