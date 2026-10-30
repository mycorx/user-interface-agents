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
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, eCapture, eCommunications, eConsole,
    eMultimedia, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows::Win32::System::Threading::SetEvent;
use windows::core::PCWSTR;

/// A registered default-device notification for one flow, unregistered on
/// drop.
///
/// Registered and dropped on the worker thread, after `CoInitializeEx` and
/// before `CoUninitialize`. It borrows the worker's wake event rather than
/// owning one: [`super::stop_worker`] closes that event, and only after
/// joining the worker, by which time this has been dropped and unregistered.
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
            wake,
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
        // Unregistering waits for a callback already running to finish, so
        // once this returns nothing signals the wake event on our behalf.
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

#[windows::core::implement(IMMNotificationClient)]
struct Notification {
    flow: Flow,
    wake: EventHandle,
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
            // SAFETY: the wake event outlives this registration; see
            // `DefaultChangeWatch`.
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
