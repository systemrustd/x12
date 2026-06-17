//! Pure VT-switch session state machine. No I/O — drives the
//! suspend/resume coordination from libseat enable/disable events.
//!
//! Spec: docs/superpowers/specs/2026-05-27-vt-switching-design.md §"State machine".

// Types and methods in this module are consumed by later tasks (seat/mod.rs,
// kms/v2/backend.rs). Suppress dead-code lint until those callers exist.
#![allow(dead_code)]

/// Server-wide seat session state. `Suspending`/`Resuming` are transient
/// states bracketing the (possibly long) sequences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatState {
    Active,
    Suspending,
    Suspended,
    Resuming,
}

/// Coalesced counter-events. We never queue more than one of each.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeatPending {
    pub pending_enable: bool,
    pub pending_disable: bool,
}

/// A libseat session event surfaced by `seat.dispatch()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatEventKind {
    Enable,
    Disable,
}

/// What the caller must do after applying an event to the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatAction {
    /// Run the suspend sequence (then call [`SeatState::suspend_complete`]).
    BeginSuspend,
    /// Run the resume sequence (then call [`SeatState::resume_complete`]).
    BeginResume,
    /// Do nothing this turn.
    Nothing,
}

impl SeatState {
    /// True only when master-requiring I/O (modeset, pageflip, submit)
    /// is allowed. Gate every such operation on this.
    #[must_use]
    pub fn allows_scanout(self) -> bool {
        matches!(self, SeatState::Active)
    }

    /// Apply a libseat event. Mutates `pending`, returns the action the
    /// caller must perform. Mirrors the spec's event×state matrix.
    pub fn on_event(&mut self, pending: &mut SeatPending, ev: SeatEventKind) -> SeatAction {
        match (*self, ev) {
            (SeatState::Active, SeatEventKind::Disable) => {
                *self = SeatState::Suspending;
                SeatAction::BeginSuspend
            }
            (SeatState::Suspended, SeatEventKind::Enable) => {
                *self = SeatState::Resuming;
                SeatAction::BeginResume
            }
            // Coalesce a counter-event that arrives mid-sequence.
            (SeatState::Suspending, SeatEventKind::Enable)
            | (SeatState::Resuming, SeatEventKind::Enable) => {
                pending.pending_enable = true;
                SeatAction::Nothing
            }
            (SeatState::Resuming, SeatEventKind::Disable) => {
                pending.pending_disable = true;
                SeatAction::Nothing
            }
            // Everything else is a no-op (log warn at the call site):
            // Active+Enable, Suspended+Disable, Suspending+Disable.
            _ => SeatAction::Nothing,
        }
    }

    /// Call after the suspend sequence finishes (libseat ack done).
    /// Commits to `Suspended`. If an enable arrived meanwhile, the
    /// pending flag is left set so the next real `Enable` acts at once.
    pub fn suspend_complete(&mut self, _pending: &SeatPending) {
        debug_assert_eq!(*self, SeatState::Suspending);
        *self = SeatState::Suspended;
    }

    /// Call after the resume sequence finishes but BEFORE committing to
    /// `Active`. If a disable arrived during resume, go straight back
    /// into `Suspending` (returning `BeginSuspend`) without ever
    /// becoming `Active` — avoids a visible "blink". Otherwise commit
    /// to `Active`.
    pub fn resume_complete(&mut self, pending: &mut SeatPending) -> SeatAction {
        debug_assert_eq!(*self, SeatState::Resuming);
        if pending.pending_disable {
            pending.pending_disable = false;
            *self = SeatState::Suspending;
            SeatAction::BeginSuspend
        } else {
            *self = SeatState::Active;
            SeatAction::Nothing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> SeatPending {
        SeatPending::default()
    }

    #[test]
    fn active_disable_begins_suspend() {
        let mut s = SeatState::Active;
        let mut pend = p();
        assert_eq!(
            s.on_event(&mut pend, SeatEventKind::Disable),
            SeatAction::BeginSuspend
        );
        assert_eq!(s, SeatState::Suspending);
    }

    #[test]
    fn suspended_enable_begins_resume() {
        let mut s = SeatState::Suspended;
        let mut pend = p();
        assert_eq!(
            s.on_event(&mut pend, SeatEventKind::Enable),
            SeatAction::BeginResume
        );
        assert_eq!(s, SeatState::Resuming);
    }

    #[test]
    fn enable_during_suspend_is_coalesced() {
        let mut s = SeatState::Suspending;
        let mut pend = p();
        assert_eq!(
            s.on_event(&mut pend, SeatEventKind::Enable),
            SeatAction::Nothing
        );
        assert!(pend.pending_enable);
        assert_eq!(s, SeatState::Suspending);
    }

    #[test]
    fn disable_during_resume_is_coalesced() {
        let mut s = SeatState::Resuming;
        let mut pend = p();
        assert_eq!(
            s.on_event(&mut pend, SeatEventKind::Disable),
            SeatAction::Nothing
        );
        assert!(pend.pending_disable);
    }

    #[test]
    fn double_disable_is_ignored() {
        let mut s = SeatState::Suspending;
        let mut pend = p();
        assert_eq!(
            s.on_event(&mut pend, SeatEventKind::Disable),
            SeatAction::Nothing
        );
        assert_eq!(s, SeatState::Suspending);
    }

    #[test]
    fn active_enable_is_ignored() {
        let mut s = SeatState::Active;
        let mut pend = p();
        assert_eq!(
            s.on_event(&mut pend, SeatEventKind::Enable),
            SeatAction::Nothing
        );
        assert_eq!(s, SeatState::Active);
    }

    #[test]
    fn resume_completion_bypasses_active_when_disable_pending() {
        let mut s = SeatState::Resuming;
        let mut pend = SeatPending {
            pending_disable: true,
            ..p()
        };
        assert_eq!(s.resume_complete(&mut pend), SeatAction::BeginSuspend);
        assert_eq!(s, SeatState::Suspending);
        assert!(!pend.pending_disable, "pending_disable consumed");
    }

    #[test]
    fn resume_completion_commits_active_when_nothing_pending() {
        let mut s = SeatState::Resuming;
        let mut pend = p();
        assert_eq!(s.resume_complete(&mut pend), SeatAction::Nothing);
        assert_eq!(s, SeatState::Active);
    }

    #[test]
    fn only_active_allows_scanout() {
        assert!(SeatState::Active.allows_scanout());
        assert!(!SeatState::Suspending.allows_scanout());
        assert!(!SeatState::Suspended.allows_scanout());
        assert!(!SeatState::Resuming.allows_scanout());
    }
}
