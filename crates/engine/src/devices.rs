//! Device loss and recovery: reopening the output and the capture with backoff, falling back to
//! the default output while the configured one is missing.

use soundsync_audio::{DeviceEvent, DeviceSelector, Direction};
use soundsync_core::{MasterMode, Nanos, Role};

use crate::node::Node;

const BACKOFF_NS: [Nanos; 4] = [500_000_000, 1_000_000_000, 2_000_000_000, 5_000_000_000];
/// How often to look for the configured output while playing on the default instead.
pub(crate) const FALLBACK_RETRY_NS: Nanos = 5_000_000_000;
pub(crate) const OUTPUT_CHANGED: &str = "Output changed — run Sync ▸ Auto-calibrate again for best sync";

/// When to try reopening a lost device: at once, then after 0.5, 1, 2, 5, 5, … s.
#[derive(Debug, Default)]
pub(crate) struct Reopener {
    next: Option<Nanos>,
    failures: usize,
}

impl Reopener {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn due(&mut self, now: Nanos) -> bool {
        self.next.is_none_or(|t| now >= t)
    }

    pub fn failed(&mut self, now: Nanos) {
        self.next = Some(now + BACKOFF_NS[self.failures.min(BACKOFF_NS.len() - 1)]);
        self.failures += 1;
    }

    pub fn succeeded(&mut self) {
        *self = Self::default();
    }
}

pub(crate) fn disconnected(device: &str) -> String {
    format!("{device} disconnected — retrying")
}

impl Node {
    /// Every engine tick: notices lost devices (OS events or dead streams) and reopens them.
    pub(crate) fn devices_tick(&mut self, now: Nanos) {
        let events: Vec<DeviceEvent> = self.device_events.as_ref().map_or_else(Vec::new, |rx| rx.try_iter().collect());
        self.device_event_count += events.len() as u64;
        let mut follow_default = false;
        for e in events {
            match e {
                DeviceEvent::Removed(id) => {
                    if self.media.output.as_ref().is_some_and(|o| o.id == id) {
                        self.output_lost(true);
                    }
                    if self.media.capture.as_ref().is_some_and(|c| c.id == id) {
                        self.capture_lost();
                    }
                }
                DeviceEvent::Added(_) => {
                    // A default output left pending by an incomplete restore may be back.
                    if let Some(msg) = self.defaults.as_mut().and_then(|g| g.retry_pending()) {
                        self.warnings.retain(|w| !w.starts_with(crate::defaults::PENDING_PREFIX));
                        tracing::info!("{msg}");
                    }
                    // Whatever we are waiting for may be back: look now instead of at the next retry.
                    if let Some(r) = &mut self.media.output_reopen {
                        r.succeeded();
                    }
                    if self.media.output_fallback_retry.is_some() {
                        self.media.output_fallback_retry = Some(now);
                    }
                    if let Some(r) = &mut self.media.capture_reopen {
                        r.succeeded();
                    }
                }
                DeviceEvent::DefaultChanged(Direction::Render) => follow_default = true,
                DeviceEvent::DefaultChanged(Direction::Capture) | DeviceEvent::StateChanged(_) => {}
            }
        }
        if follow_default {
            self.default_output_changed();
        }
        if self.media.output.as_ref().is_some_and(|o| !o.stream.is_alive()) {
            self.output_lost(false);
        }
        if self.media.capture.as_ref().is_some_and(|c| !c.stream.is_alive()) {
            self.capture_lost();
        }
        self.retry_output(now);
        self.retry_capture(now);
    }

    /// The OS default output changed: streams opened on "the default" move to the new one.
    fn default_output_changed(&mut self) {
        // A virtual master's monitor keeps its device: the new default is usually the VB-CABLE it
        // captures (switched by us or by the user). Only a monitor that was refused for being the
        // cable tries again (open_output never plays into the captured device).
        if self.capture_mode() == Some(MasterMode::Virtual) {
            if self.media.output.is_none() && self.media.output_reopen.is_none() {
                self.open_output();
            }
            return;
        }
        if self.media.output.is_some() && self.output_on_default() {
            tracing::info!("default output changed; reopening");
            let before = self.media.output.as_ref().map(|o| o.device.clone());
            self.open_output();
            if self.media.output.as_ref().map(|o| o.device.clone()) != before {
                self.warn(OUTPUT_CHANGED);
            }
        }
        if self.media.capture.as_ref().is_some_and(|c| c.selector == DeviceSelector::Default) {
            tracing::info!("default output changed; capturing the new one");
            self.restart_capture();
        }
    }

    fn output_lost(&mut self, removed: bool) {
        let Some(device) = self.media.output.as_ref().map(|o| o.device.clone()) else { return };
        self.drop_all_slots();
        self.media.output = None;
        self.warn(disconnected(&device));
        if self.media.output_lost_device.is_none() {
            self.media.output_lost_device = Some(device);
        }
        self.media.output_removed |= removed;
        self.media.output_reopen = Some(Reopener::new());
    }

    fn retry_output(&mut self, now: Nanos) {
        if let Some(r) = &mut self.media.output_reopen {
            if !r.due(now) {
                return;
            }
            self.open_output();
            if self.media.output.is_none() {
                if let Some(r) = &mut self.media.output_reopen {
                    r.failed(now);
                }
                return;
            }
            self.media.output_reopen = None;
            let lost = self.media.output_lost_device.take();
            let removed = std::mem::take(&mut self.media.output_removed);
            if let Some(l) = &lost {
                self.warnings.retain(|w| *w != disconnected(l));
            }
            let device = self.media.output.as_ref().map(|o| o.device.clone());
            tracing::info!("output back on {}", device.as_deref().unwrap_or("?"));
            if removed || lost != device {
                self.warn(OUTPUT_CHANGED);
            }
        } else if self.media.output_fallback_retry.is_some_and(|t| now >= t) {
            // Playing on the default because the configured output was missing: is it back?
            self.media.output_fallback_retry = Some(now + FALLBACK_RETRY_NS);
            if self.configured_output_present() {
                tracing::info!("configured output is back");
                self.open_output();
                self.warn(OUTPUT_CHANGED);
            }
        }
    }

    fn capture_lost(&mut self) {
        let Some(c) = self.media.capture.take() else { return };
        self.warn(disconnected(&c.device));
        self.media.capture_lost_device = Some(c.device.clone());
        drop(c);
        self.silence_suspect = false;
        if let Some(d) = &mut self.discovery {
            d.update_role(Role::Receiver);
        }
        self.media.capture_reopen = Some(Reopener::new());
    }

    fn retry_capture(&mut self, now: Nanos) {
        let Some(r) = &mut self.media.capture_reopen else { return };
        if !r.due(now) {
            return;
        }
        if !self.config.master.enabled {
            self.clear_capture_recovery();
            return;
        }
        self.start_broadcast();
        if self.media.capture.is_some() {
            tracing::info!("capture back on {}", self.capture_device_name().unwrap_or_default());
            self.clear_capture_recovery();
        } else if let Some(r) = &mut self.media.capture_reopen {
            r.failed(now);
        }
    }

    /// Stops waiting for a lost capture device (it is back, or broadcasting was turned off).
    pub(crate) fn clear_capture_recovery(&mut self) {
        self.media.capture_reopen = None;
        if let Some(l) = self.media.capture_lost_device.take() {
            self.warnings.retain(|w| *w != disconnected(&l));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Nanos = 1_000_000_000;

    #[test]
    fn reopener_backoff_sequence() {
        let mut r = Reopener::new();
        assert!(r.due(0), "the first attempt is immediate");
        let mut t = 0;
        for wait in [S / 2, S, 2 * S, 5 * S, 5 * S, 5 * S] {
            r.failed(t);
            assert!(!r.due(t + wait - 1), "retry before {wait} ns");
            assert!(r.due(t + wait));
            t += wait;
        }
        r.succeeded();
        assert!(r.due(t));
        r.failed(t);
        assert!(!r.due(t + S / 2 - 1), "success restarts the backoff");
        assert!(r.due(t + S / 2));
    }
}
