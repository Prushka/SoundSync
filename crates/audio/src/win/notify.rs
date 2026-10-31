//! Device change notifications (IMMNotificationClient via wasapi).

use std::sync::Arc;

use wasapi::{DeviceEnumerator, DeviceEventCallbacks, DeviceEventRegistration, DeviceState};

use super::com::{ensure_com, os_err};
use crate::{AudioError, DeviceEvent, Direction};

/// Keeps the registration alive; dropping it unregisters.
struct Registration(#[allow(dead_code)] DeviceEventRegistration);

// SAFETY: the registration holds MTA COM interfaces (enumerator + notification client), which may
// be used and released from any thread in the multithreaded apartment.
unsafe impl Send for Registration {}

pub(crate) fn watch(on_event: Box<dyn Fn(DeviceEvent) + Send + Sync>) -> Result<Box<dyn Send>, AudioError> {
    ensure_com();
    let on_event: Arc<dyn Fn(DeviceEvent) + Send + Sync> = Arc::from(on_event);
    let mut cb = DeviceEventCallbacks::new();
    let f = on_event.clone();
    cb.set_device_added_callback(move |id| f(DeviceEvent::Added(id)));
    let f = on_event.clone();
    cb.set_device_removed_callback(move |id| f(DeviceEvent::Removed(id)));
    let f = on_event.clone();
    cb.set_device_state_callback(move |id, state| {
        f(if state == DeviceState::Active { DeviceEvent::Added(id.clone()) } else { DeviceEvent::Removed(id.clone()) });
        f(DeviceEvent::StateChanged(id));
    });
    let f = on_event;
    cb.set_default_device_callback(move |dir, _role, _id| {
        f(DeviceEvent::DefaultChanged(match dir {
            wasapi::Direction::Render => Direction::Render,
            wasapi::Direction::Capture => Direction::Capture,
        }))
    });
    let enumerator = DeviceEnumerator::new().map_err(|e| os_err("device enumerator", e))?;
    let reg = enumerator.register_notification_callback(cb).map_err(|e| os_err("register notifications", e))?;
    Ok(Box::new(Registration(reg)))
}
