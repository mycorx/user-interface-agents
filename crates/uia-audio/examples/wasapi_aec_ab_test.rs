// Copyright (c) 2026 MycorX (Daniel, Sole Trader). All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! A/B test: does WASAPI's `AudioCategory_Communications` actually suppress
//! *known* playback from the same app's capture, or did the earlier POC's
//! ducking-based smoke test just pick an unreliable OS side-effect to watch?
//!
//! Unlike `wasapi_aec_poc`, this opens BOTH a render stream (playing a pure
//! tone) and a capture stream at once, in two phases:
//!   1. Both streams opened with `AudioCategory_Communications` against the
//!      `eCommunications` role — how a real VoIP app would request AEC.
//!   2. Both streams opened with `AudioCategory_Other` against the `eConsole`
//!      role — an ordinary app with no special processing, as a baseline.
//!
//! It measures the RMS of what the mic captures while the tone plays in
//! each phase. If Windows is genuinely cancelling its own known playback,
//! phase 1's captured RMS should be meaningfully lower than phase 2's, even
//! though the same tone plays at the same volume in both.
//!
//! Caveat printed at the end, not papered over: if Windows Sound Settings has
//! a *different* physical default device configured for the Communications
//! role than for the regular Console role, this comparison is confounded by
//! that device difference too, not just AEC. Check
//! Settings > System > Sound > that both roles point at the same device
//! before trusting a large gap here as proof.
//!
//! `cargo run -p uia-audio --example wasapi_aec_ab_test --features wasapi-aec`

#[cfg(all(windows, feature = "wasapi-aec"))]
fn main() -> windows::core::Result<()> {
    wasapi_ab::run()
}

#[cfg(not(all(windows, feature = "wasapi-aec")))]
fn main() {
    eprintln!(
        "this example only builds on Windows with --features wasapi-aec: \
         cargo run -p uia-audio --example wasapi_aec_ab_test --features wasapi-aec"
    );
}

#[cfg(all(windows, feature = "wasapi-aec"))]
mod wasapi_ab {
    use std::f32::consts::PI;
    use std::mem::size_of;
    use std::time::{Duration, Instant};
    use windows::Win32::Media::Audio::{
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMOPTIONS_NONE, AUDIO_STREAM_CATEGORY,
        AudioCategory_Communications, AudioCategory_Other, AudioClientProperties, ERole,
        IAudioCaptureClient, IAudioClient2, IAudioRenderClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, eCapture, eCommunications, eConsole, eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };

    const PHASE_DURATION: Duration = Duration::from_secs(8);
    const TONE_HZ: f32 = 1000.0;
    const TONE_AMPLITUDE: f32 = 0.3;
    const BUFFER_DURATION_HNS: i64 = 200 * 10_000;

    pub fn run() -> windows::core::Result<()> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            let result = run_ab_test();
            CoUninitialize();
            result
        }
    }

    unsafe fn run_ab_test() -> windows::core::Result<()> {
        unsafe {
            println!(
                "Phase 1: Communications category + role — how a real VoIP app requests AEC.\n\
                 Stay quiet near the mic; only the {TONE_HZ:.0} Hz tone should be present."
            );
            let with_aec = run_phase(
                AudioCategory_Communications,
                eCommunications,
                PHASE_DURATION,
            )?;
            println!("  captured RMS: {with_aec:.6}\n");

            std::thread::sleep(Duration::from_secs(1));

            println!(
                "Phase 2: baseline, ordinary category + role — no special processing requested."
            );
            let baseline = run_phase(AudioCategory_Other, eConsole, PHASE_DURATION)?;
            println!("  captured RMS: {baseline:.6}\n");

            println!("=== Result ===");
            println!("with AEC category (Communications): RMS = {with_aec:.6}");
            println!("baseline (Other/Console):            RMS = {baseline:.6}");
            if baseline > 1e-6 {
                let ratio = with_aec / baseline;
                println!("ratio (lower = more suppression):    {ratio:.3}");
                if ratio < 0.5 {
                    println!(
                        "=> Windows is measurably suppressing its own known playback under \
                         the Communications category. Worth building the real integration."
                    );
                } else {
                    println!(
                        "=> No strong evidence of suppression here. Could mean this \
                         driver/hardware doesn't provide a Communications-role AEC APO, or \
                         the two roles use different physical devices (see the caveat above) \
                         confounding the comparison."
                    );
                }
            } else {
                println!(
                    "baseline RMS was ~0 — the tone likely wasn't audible to the mic at all in \
                     phase 2 (volume/device routing issue), so this comparison isn't valid. \
                     Check speaker volume and try again."
                );
            }
            println!(
                "\nCaveat: if Settings > System > Sound has different physical default devices \
                 for the Communications role vs. the regular role, this result mixes AEC \
                 effectiveness with device differences. Verify they match before trusting a big \
                 gap here as proof."
            );

            Ok(())
        }
    }

    /// Runs one phase: opens a render stream (playing a continuous tone) and a
    /// capture stream, both under `category`/`role`, for `duration`, and
    /// returns the RMS of everything the capture stream picked up. Polling
    /// (no event handles) on purpose — this is a diagnostic run over several
    /// seconds, not a latency-sensitive real-time path, so the simpler loop
    /// is worth the small amount of jitter it costs.
    unsafe fn run_phase(
        category: AUDIO_STREAM_CATEGORY,
        role: ERole,
        duration: Duration,
    ) -> windows::core::Result<f32> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;

            let render_device = enumerator.GetDefaultAudioEndpoint(eRender, role)?;
            let render_client: IAudioClient2 = render_device.Activate(CLSCTX_ALL, None)?;
            render_client.SetClientProperties(&AudioClientProperties {
                cbSize: size_of::<AudioClientProperties>() as u32,
                bIsOffload: false.into(),
                eCategory: category,
                Options: AUDCLNT_STREAMOPTIONS_NONE,
            })?;
            let render_format = render_client.GetMixFormat()?;
            let render_channels = (*render_format).nChannels as u32;
            let render_rate = (*render_format).nSamplesPerSec;
            render_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0,
                BUFFER_DURATION_HNS,
                0,
                render_format,
                None,
            )?;
            let render_service: IAudioRenderClient = render_client.GetService()?;

            let capture_device = enumerator.GetDefaultAudioEndpoint(eCapture, role)?;
            let capture_client: IAudioClient2 = capture_device.Activate(CLSCTX_ALL, None)?;
            capture_client.SetClientProperties(&AudioClientProperties {
                cbSize: size_of::<AudioClientProperties>() as u32,
                bIsOffload: false.into(),
                eCategory: category,
                Options: AUDCLNT_STREAMOPTIONS_NONE,
            })?;
            let capture_format = capture_client.GetMixFormat()?;
            let capture_channels = (*capture_format).nChannels as u32;
            let capture_bits = (*capture_format).wBitsPerSample;
            capture_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0,
                BUFFER_DURATION_HNS,
                0,
                capture_format,
                None,
            )?;
            let capture_service: IAudioCaptureClient = capture_client.GetService()?;

            let mut phase = 0.0f32;
            let phase_inc = 2.0 * PI * TONE_HZ / render_rate as f32;
            fill_render_buffer(
                &render_client,
                &render_service,
                render_channels,
                &mut phase,
                phase_inc,
            )?;

            render_client.Start()?;
            capture_client.Start()?;

            let mut sum_sq = 0f64;
            let mut count = 0u64;
            let start = Instant::now();
            while start.elapsed() < duration {
                fill_render_buffer(
                    &render_client,
                    &render_service,
                    render_channels,
                    &mut phase,
                    phase_inc,
                )?;

                loop {
                    let packet_frames = capture_service.GetNextPacketSize()?;
                    if packet_frames == 0 {
                        break;
                    }
                    let mut data_ptr = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    capture_service.GetBuffer(
                        &mut data_ptr,
                        &mut frames,
                        &mut flags,
                        None,
                        None,
                    )?;
                    if capture_bits == 32 && !data_ptr.is_null() {
                        let samples = std::slice::from_raw_parts(
                            data_ptr as *const f32,
                            (frames * capture_channels) as usize,
                        );
                        for &s in samples {
                            sum_sq += (s as f64) * (s as f64);
                            count += 1;
                        }
                    }
                    capture_service.ReleaseBuffer(frames)?;
                }
                std::thread::sleep(Duration::from_millis(10));
            }

            render_client.Stop()?;
            capture_client.Stop()?;

            Ok(if count > 0 {
                (sum_sq / count as f64).sqrt() as f32
            } else {
                0.0
            })
        }
    }

    /// Tops up the render ring buffer with as much of the continuous tone as
    /// there is currently free space for, advancing `phase` across calls so
    /// the waveform stays continuous rather than restarting each call.
    unsafe fn fill_render_buffer(
        render_client: &IAudioClient2,
        render_service: &IAudioRenderClient,
        channels: u32,
        phase: &mut f32,
        phase_inc: f32,
    ) -> windows::core::Result<()> {
        unsafe {
            let buffer_frames = render_client.GetBufferSize()?;
            let padding = render_client.GetCurrentPadding()?;
            let available = buffer_frames.saturating_sub(padding);
            if available == 0 {
                return Ok(());
            }
            let ptr = render_service.GetBuffer(available)?;
            let samples =
                std::slice::from_raw_parts_mut(ptr as *mut f32, (available * channels) as usize);
            for frame in samples.chunks_mut(channels as usize) {
                let s = phase.sin() * TONE_AMPLITUDE;
                for out in frame {
                    *out = s;
                }
                *phase += phase_inc;
                if *phase > 2.0 * PI {
                    *phase -= 2.0 * PI;
                }
            }
            render_service.ReleaseBuffer(available, 0)
        }
    }
}
