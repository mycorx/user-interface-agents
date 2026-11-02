// Copyright (c) 2026 MycorX. All rights reserved.
// PolyForm Internal Use 1.0.0 OR PolyForm Noncommercial 1.0.0 — see LICENSE.md.

//! Telling an unpinned worker that Windows' communications default moved.
//!
//! WASAPI does not move an open stream to a new default, and changing the
//! default does not always invalidate the old client either: a worker on the
//! previous device would keep playing there. So an unpinned worker registers
//! an `IMMNotificationClient`, and the callback wakes it to reopen. Whether a
//! given change counts is [`crate::follow::reopen_for_default_change`]'s
//! decision; this module only maps Windows' types onto it.

use super::com::EventHandle;
use crate::follow::{self, Flow, Role};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, PROPERTYKEY,
};
use windows::Win32::Media::Audio::{
    DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, eCapture, eCommunications, eConsole,
    eMultimedia, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Threading::{GetCurrentProcess, SetEvent};
use windows::core::PCWSTR;

/// A registered default-device notification for one flow, unregistered on
/// drop.
///
/// Registered and dropped on the worker thread, after `CoInitializeEx` and
/// before `CoUninitialize`. The callback signals its own duplicate of the
/// worker's wake event ([`OwnedWake`]), so [`super::stop_worker`] closing
/// the worker's handle never leaves the callback holding a dead one.
pub(crate) struct DefaultChangeWatch {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
    changed: Arc<AtomicBool>,
}

impl DefaultChangeWatch {
    /// Wake `wake` whenever the communications default for `flow` changes.
    /// COM must be initialised on this thread; if it is not,
    /// `CoCreateInstance` says so as an error.
    pub(crate) fn register(flow: Flow, wake: EventHandle) -> windows::core::Result<Self> {
        let changed = Arc::new(AtomicBool::new(false));
        let client: IMMNotificationClient = Notification {
            flow,
            wake: OwnedWake::duplicate(wake)?,
            changed: Arc::clone(&changed),
        }
        .into();
        // SAFETY: plain COM calls on an initialised apartment; `client` is a
        // live COM object kept alive by `self` until it is unregistered.
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            enumerator.RegisterEndpointNotificationCallback(&client)?;
            Ok(Self {
                enumerator,
                client,
                changed,
            })
        }
    }

    /// Did the default change since the last call? Clears the flag.
    pub(crate) fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::SeqCst)
    }
}

impl Drop for DefaultChangeWatch {
    fn drop(&mut self) {
        // SAFETY: the same enumerator and client that were registered.
        // Unregistering waits for a callback already running to finish. If
        // it fails, Windows keeps the client and may still call it; that
        // only signals the client's own duplicate, which stays open until
        // the last reference to the client goes.
        unsafe {
            if let Err(e) = self
                .enumerator
                .UnregisterEndpointNotificationCallback(&self.client)
            {
                eprintln!("uia-audio: wasapi could not unregister the default-device watch: {e}");
            }
        }
    }
}

/// The callback's own handle to the worker's wake event, closed when the
/// callback object is released.
///
/// Duplicated rather than borrowed: if unregistering fails, Windows can
/// call the callback after the worker has closed its handle, and `SetEvent`
/// on a closed handle value signals whatever object reuses it.
struct OwnedWake(HANDLE);

// SAFETY: a kernel event handle is not thread affine. This one is only
// signalled (`SetEvent`, from any notification thread) and closed once, in
// `Drop`, after the last reference to the callback is gone.
unsafe impl Send for OwnedWake {}
unsafe impl Sync for OwnedWake {}

impl OwnedWake {
    fn duplicate(wake: EventHandle) -> windows::core::Result<Self> {
        let mut copy = HANDLE::default();
        // SAFETY: `wake` is the worker's open event, which outlives this
        // call; `copy` receives a new handle this value owns and closes.
        unsafe {
            let process = GetCurrentProcess();
            DuplicateHandle(
                process,
                wake.0,
                process,
                &mut copy,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )?;
        }
        Ok(Self(copy))
    }
}

impl Drop for OwnedWake {
    fn drop(&mut self) {
        // SAFETY: the handle `duplicate` created, closed exactly once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[windows::core::implement(IMMNotificationClient)]
struct Notification {
    flow: Flow,
    wake: OwnedWake,
    changed: Arc<AtomicBool>,
}

fn flow_of(flow: EDataFlow) -> Option<Flow> {
    match flow {
        f if f == eCapture => Some(Flow::Capture),
        f if f == eRender => Some(Flow::Render),
        _ => None,
    }
}

fn role_of(role: ERole) -> Option<Role> {
    match role {
        r if r == eConsole => Some(Role::Console),
        r if r == eMultimedia => Some(Role::Multimedia),
        r if r == eCommunications => Some(Role::Communications),
        _ => None,
    }
}

impl IMMNotificationClient_Impl for Notification_Impl {
    /// Runs on a Windows notification thread. Only sets a flag and signals:
    /// reopening here could deadlock against the worker unregistering this
    /// callback while it runs.
    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _pwstrdefaultdeviceid: &PCWSTR,
    ) -> windows::core::Result<()> {
        let (Some(changed), Some(role)) = (flow_of(flow), role_of(role)) else {
            return Ok(());
        };
        // Only unpinned workers register a watch, hence `pinned: false`.
        if follow::reopen_for_default_change(false, self.flow, changed, role) {
            self.changed.store(true, Ordering::SeqCst);
            // SAFETY: this callback's own duplicate, open for as long as
            // the callback exists; see `OwnedWake`.
            unsafe {
                let _ = SetEvent(self.wake.0);
            }
        }
        Ok(())
    }

    fn OnDeviceStateChanged(
        &self,
        _pwstrdeviceid: &PCWSTR,
        _dwnewstate: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDeviceAdded(&self, _pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnDeviceRemoved(&self, _pwstrdeviceid: &PCWSTR) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _pwstrdeviceid: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}
