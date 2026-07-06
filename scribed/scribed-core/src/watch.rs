//! Session watcher: silent finished-writing/drawing detection from pen
//! proximity, per DESIGN.md §5.2. Pure state machine — no evdev here, so it
//! is fully unit-testable without a device. Time is passed in explicitly
//! (monotonic seconds) rather than read from the clock, for determinism.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenEvent {
    /// BTN_TOOL_PEN 1 — pen enters hover/proximity range.
    ToolIn,
    /// BTN_TOOL_PEN 0 — pen leaves hover/proximity range.
    ToolOut,
    /// BTN_TOUCH 1 — nib contacts the screen (real ink).
    TouchDown,
    /// BTN_TOUCH 0 — nib lifts off the screen.
    TouchUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherState {
    Idle,
    Asking,
    Dissolving,
    Drawing,
}

#[derive(Debug, Clone)]
pub struct SessionWatcher {
    dwell_s: f64,
    rate_limit_s: f64,
    state: WatcherState,
    ink_dirty: bool,
    pen_in_range: bool,
    last_activity: Option<f64>,
    last_request_at: Option<f64>,
}

impl SessionWatcher {
    pub fn new(dwell_s: f64, rate_limit_s: f64) -> Self {
        Self {
            dwell_s,
            rate_limit_s,
            state: WatcherState::Idle,
            ink_dirty: false,
            pen_in_range: false,
            last_activity: None,
            last_request_at: None,
        }
    }

    pub fn state(&self) -> WatcherState {
        self.state
    }

    /// Feed a real-pen evdev event at time `now` (monotonic seconds).
    pub fn on_pen_event(&mut self, event: PenEvent, now: f64) {
        self.last_activity = Some(now);
        match event {
            PenEvent::ToolIn => self.pen_in_range = true,
            PenEvent::ToolOut => self.pen_in_range = false,
            PenEvent::TouchDown => {
                self.ink_dirty = true;
                // Any real pen-down while we're mid-cycle aborts the request/
                // injection — the page is never touched on a resumed thought.
                if self.state != WatcherState::Idle {
                    self.state = WatcherState::Idle;
                }
            }
            PenEvent::TouchUp => {}
        }
    }

    /// True if the finished-writing/drawing condition is met and we're free
    /// to start a new cycle (idle, past the dwell, past the rate limit).
    pub fn should_trigger(&self, now: f64) -> bool {
        if self.state != WatcherState::Idle || !self.ink_dirty || self.pen_in_range {
            return false;
        }
        let Some(last) = self.last_activity else { return false };
        if now - last < self.dwell_s {
            return false;
        }
        if let Some(last_req) = self.last_request_at {
            if now - last_req < self.rate_limit_s {
                return false;
            }
        }
        true
    }

    /// Transition Idle -> Asking (call once should_trigger() is true and the
    /// capture has been taken).
    pub fn begin_asking(&mut self, now: f64) {
        self.state = WatcherState::Asking;
        self.last_request_at = Some(now);
    }

    pub fn begin_dissolving(&mut self) {
        debug_assert_eq!(self.state, WatcherState::Asking);
        self.state = WatcherState::Dissolving;
    }

    pub fn begin_drawing(&mut self) {
        debug_assert_eq!(self.state, WatcherState::Dissolving);
        self.state = WatcherState::Drawing;
    }

    /// Cycle finished successfully (or failed) — back to Idle. Clears
    /// ink_dirty only on success, since a failed request leaves the page's
    /// ink exactly as the user left it (still "dirty" / unanswered).
    pub fn complete_cycle(&mut self, success: bool) {
        self.state = WatcherState::Idle;
        if success {
            self.ink_dirty = false;
        }
    }

    /// Real pen-down during Dissolving/Drawing aborts injection cleanly.
    pub fn abort_injection(&mut self) {
        if matches!(self.state, WatcherState::Dissolving | WatcherState::Drawing) {
            self.state = WatcherState::Idle;
        }
    }

    /// Page-turn / notebook-switch heuristic (DESIGN.md §5.2): call with the
    /// fraction of previously-inked pixels that vanished/moved without our
    /// own involvement. Returns true if this looks like a page change, in
    /// which case the caller should clear ink_dirty and (Phase 2) reset
    /// conversation history.
    pub fn is_page_change(vanished_fraction: f64) -> bool {
        vanished_fraction > 0.60
    }

    pub fn clear_ink_dirty(&mut self) {
        self.ink_dirty = false;
    }
}

/// Total changed pixels (either direction) — used for the page-change heuristic.
pub fn count_changed_pixels(prev: &[u8], curr: &[u8], threshold: u8) -> usize {
    prev.iter().zip(curr.iter()).filter(|(a, b)| a.abs_diff(**b) > threshold).count()
}

/// New-ink gate (DESIGN.md §5.2): counts only pixels that got *darker* — i.e.
/// fresh ink laid down — so that erasing existing ink (which lightens pixels)
/// does NOT read as new ink and trigger a cycle on the leftovers. Gray is
/// 0=black..255=white, so "darker" means `curr` is lower than `prev`.
pub fn count_new_ink(prev: &[u8], curr: &[u8], threshold: u8) -> usize {
    prev.iter()
        .zip(curr.iter())
        .filter(|(p, c)| (**p).saturating_sub(**c) > threshold)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn does_not_trigger_before_dwell_elapses() {
        let mut w = SessionWatcher::new(5.0, 10.0);
        w.on_pen_event(PenEvent::TouchDown, 0.0);
        w.on_pen_event(PenEvent::TouchUp, 0.5);
        w.on_pen_event(PenEvent::ToolOut, 0.6);
        assert!(!w.should_trigger(3.0), "only 2.4s of quiet have passed");
        assert!(w.should_trigger(5.7), "5.1s of quiet have passed");
    }

    #[test]
    fn hover_holds_off_trigger_even_past_dwell() {
        let mut w = SessionWatcher::new(5.0, 10.0);
        w.on_pen_event(PenEvent::ToolIn, 0.0);
        w.on_pen_event(PenEvent::TouchDown, 0.1);
        w.on_pen_event(PenEvent::TouchUp, 0.5);
        // Pen still hovering (no ToolOut) — never trigger no matter how long we wait.
        assert!(!w.should_trigger(100.0));
    }

    #[test]
    fn no_trigger_without_any_ink() {
        let mut w = SessionWatcher::new(5.0, 10.0);
        w.on_pen_event(PenEvent::ToolIn, 0.0);
        w.on_pen_event(PenEvent::ToolOut, 0.1);
        assert!(!w.should_trigger(50.0), "hovering with no touch-down is not ink");
    }

    #[test]
    fn resume_during_asking_aborts_and_page_stays_untouched() {
        let mut w = SessionWatcher::new(5.0, 10.0);
        w.on_pen_event(PenEvent::TouchDown, 0.0);
        w.on_pen_event(PenEvent::TouchUp, 0.2);
        w.on_pen_event(PenEvent::ToolOut, 0.3);
        assert!(w.should_trigger(6.0));
        w.begin_asking(6.0);
        assert_eq!(w.state(), WatcherState::Asking);

        // User picks the pen back up mid-request.
        w.on_pen_event(PenEvent::TouchDown, 6.5);
        assert_eq!(w.state(), WatcherState::Idle, "pen-down must cancel the in-flight request");

        // ink_dirty is still set (never cleared on abort), so a later quiet
        // period retriggers — the half-question isn't lost. (Past both the
        // dwell from 6.7 and the 10s rate limit from the first request at 6.0.)
        w.on_pen_event(PenEvent::TouchUp, 6.6);
        w.on_pen_event(PenEvent::ToolOut, 6.7);
        assert!(w.should_trigger(17.0));
    }

    #[test]
    fn rate_limit_prevents_immediate_retrigger() {
        let mut w = SessionWatcher::new(1.0, 20.0);
        w.on_pen_event(PenEvent::TouchDown, 0.0);
        w.on_pen_event(PenEvent::TouchUp, 0.1);
        w.on_pen_event(PenEvent::ToolOut, 0.2);
        assert!(w.should_trigger(2.0));
        w.begin_asking(2.0);
        w.begin_dissolving();
        w.begin_drawing();
        w.complete_cycle(true);
        assert_eq!(w.state(), WatcherState::Idle);

        // New ink right after completion, but within the rate-limit window.
        w.on_pen_event(PenEvent::TouchDown, 3.0);
        w.on_pen_event(PenEvent::TouchUp, 3.1);
        w.on_pen_event(PenEvent::ToolOut, 3.2);
        assert!(!w.should_trigger(5.0), "still inside the 20s rate-limit window");
        assert!(w.should_trigger(23.0));
    }

    #[test]
    fn failed_cycle_keeps_ink_dirty_so_it_retries() {
        let mut w = SessionWatcher::new(1.0, 1.0);
        w.on_pen_event(PenEvent::TouchDown, 0.0);
        w.on_pen_event(PenEvent::TouchUp, 0.1);
        w.on_pen_event(PenEvent::ToolOut, 0.2);
        assert!(w.should_trigger(2.0));
        w.begin_asking(2.0);
        w.complete_cycle(false); // network failure — page untouched
        assert!(w.should_trigger(4.0), "failed cycle must not clear ink_dirty");
    }

    #[test]
    fn pen_down_during_drawing_aborts_injection() {
        let mut w = SessionWatcher::new(1.0, 1.0);
        w.on_pen_event(PenEvent::TouchDown, 0.0);
        w.on_pen_event(PenEvent::TouchUp, 0.1);
        w.on_pen_event(PenEvent::ToolOut, 0.2);
        w.begin_asking(2.0);
        w.begin_dissolving();
        w.begin_drawing();
        w.on_pen_event(PenEvent::TouchDown, 2.5);
        assert_eq!(w.state(), WatcherState::Idle);
    }

    #[test]
    fn page_change_heuristic_threshold() {
        assert!(!SessionWatcher::is_page_change(0.59));
        assert!(SessionWatcher::is_page_change(0.61));
    }

    #[test]
    fn changed_pixel_count_ignores_small_deltas() {
        let prev = vec![255u8, 255, 0, 100];
        let curr = vec![255u8, 250, 0, 200];
        assert_eq!(count_changed_pixels(&prev, &curr, 10), 1);
    }
}
