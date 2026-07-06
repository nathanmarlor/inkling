//! On/off toggle gesture for scribed itself.
//!
//! xochitl is closed-source Qt6 — we cannot add a real button to its side
//! toolbar. The practical equivalent: double-tap a fixed screen zone (placed
//! over/near where xochitl's own tool-icon column sits, so it *feels* like
//! part of the toolbar) to flip scribed's paused/active state. This reuses
//! the pause-file mechanism from DESIGN.md §9 as the actual on/off switch —
//! this detector just decides *when* to flip it from a touch stream.
//!
//! Deliberately separate from `watch::SessionWatcher`: this toggles whether
//! scribed is watching at all, not whether a single request should fire.

use crate::geometry::{PointPx, RectPx};

#[derive(Debug, Clone)]
pub struct ToggleZone {
    pub rect: RectPx,
    pub double_tap_s: f64,
}

#[derive(Debug, Clone)]
pub struct ToggleDetector {
    zone: ToggleZone,
    enabled: bool,
    last_tap_in_zone: Option<f64>,
}

impl ToggleDetector {
    pub fn new(zone: ToggleZone, initially_enabled: bool) -> Self {
        Self { zone, enabled: initially_enabled, last_tap_in_zone: None }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Feed a touch-down tap at `at` (display px) at time `now`. Returns
    /// true if this tap just toggled the enabled state (so the caller can
    /// draw a brief on-screen acknowledgment).
    pub fn on_tap(&mut self, at: PointPx, now: f64) -> bool {
        if !self.zone.rect.contains(at) {
            self.last_tap_in_zone = None;
            return false;
        }
        if let Some(last) = self.last_tap_in_zone {
            if now - last <= self.zone.double_tap_s {
                self.enabled = !self.enabled;
                self.last_tap_in_zone = None;
                return true;
            }
        }
        self.last_tap_in_zone = Some(now);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone() -> ToggleZone {
        ToggleZone { rect: RectPx::new(1250.0, 0.0, 154.0, 200.0), double_tap_s: 0.4 }
    }

    #[test]
    fn double_tap_inside_zone_toggles() {
        let mut d = ToggleDetector::new(zone(), true);
        assert!(d.enabled());
        assert!(!d.on_tap(PointPx::new(1300.0, 100.0), 0.0));
        assert!(d.on_tap(PointPx::new(1300.0, 100.0), 0.2));
        assert!(!d.enabled(), "second tap within window should toggle off");
    }

    #[test]
    fn taps_outside_zone_are_ignored() {
        let mut d = ToggleDetector::new(zone(), true);
        assert!(!d.on_tap(PointPx::new(10.0, 10.0), 0.0));
        assert!(!d.on_tap(PointPx::new(10.0, 10.0), 0.1));
        assert!(d.enabled(), "taps outside the zone must never toggle");
    }

    #[test]
    fn taps_too_far_apart_do_not_toggle() {
        let mut d = ToggleDetector::new(zone(), true);
        assert!(!d.on_tap(PointPx::new(1300.0, 100.0), 0.0));
        assert!(!d.on_tap(PointPx::new(1300.0, 100.0), 5.0));
        assert!(d.enabled(), "taps 5s apart are two separate single-taps, not a double-tap");
    }

    #[test]
    fn toggling_twice_returns_to_original_state() {
        let mut d = ToggleDetector::new(zone(), true);
        d.on_tap(PointPx::new(1300.0, 100.0), 0.0);
        d.on_tap(PointPx::new(1300.0, 100.0), 0.1);
        assert!(!d.enabled());
        d.on_tap(PointPx::new(1300.0, 100.0), 1.0);
        d.on_tap(PointPx::new(1300.0, 100.0), 1.1);
        assert!(d.enabled());
    }
}
