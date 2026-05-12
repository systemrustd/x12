//! Per-output dirty-generation tracking.
//!
//! Replaces the global `screen_dirty: bool`. Each `OutputLayout`
//! owns one of these. Producers (paint, geometry change, hotplug,
//! input fanout) call `bump_dirty`; the composite scheduler reads
//! `needs_composite`.

#[derive(Debug)]
pub struct OutputDamageState {
    dirty_gen: u64,
    last_submitted_gen: u64,
    last_presented_gen: u64,
    flip_pending: bool,
}

impl OutputDamageState {
    pub fn new() -> Self {
        Self {
            dirty_gen: 1,
            last_submitted_gen: 0,
            last_presented_gen: 0,
            flip_pending: false,
        }
    }

    pub fn dirty_gen(&self) -> u64 {
        self.dirty_gen
    }

    pub fn last_submitted_gen(&self) -> u64 {
        self.last_submitted_gen
    }

    pub fn last_presented_gen(&self) -> u64 {
        self.last_presented_gen
    }

    pub fn flip_pending(&self) -> bool {
        self.flip_pending
    }

    /// Bump on any producer event: paint, geometry change, hotplug,
    /// input fanout.
    pub fn bump_dirty(&mut self) {
        self.dirty_gen += 1;
    }

    /// True iff there is unpresented damage and the previous flip
    /// has retired.
    pub fn needs_composite(&self) -> bool {
        self.dirty_gen > self.last_presented_gen && !self.flip_pending
    }

    /// Composite was recorded + submitted for this output. The
    /// dirty generation captured is `dirty_gen` at this moment.
    /// Bumps arriving after this call advance `dirty_gen` past
    /// `last_submitted_gen`, so they will re-arm `needs_composite`
    /// after the flip retires.
    pub fn record_submit(&mut self) {
        debug_assert!(
            !self.flip_pending,
            "record_submit called while flip already pending",
        );
        self.last_submitted_gen = self.dirty_gen;
        self.flip_pending = true;
    }

    /// The pageflip-complete event fired for this output. The
    /// presented generation advances to whatever was last submitted;
    /// any bumps that arrived between submit and present remain in
    /// `dirty_gen` and re-arm `needs_composite`.
    pub fn record_present(&mut self) {
        debug_assert!(
            self.flip_pending,
            "record_present called without prior record_submit",
        );
        self.last_presented_gen = self.last_submitted_gen;
        self.flip_pending = false;
    }
}

impl Default for OutputDamageState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_state_needs_composite() {
        let s = OutputDamageState::new();
        assert!(
            s.needs_composite(),
            "first frame must paint (dirty_gen=1 > last_presented_gen=0)"
        );
    }

    #[test]
    fn bump_advances_dirty_gen() {
        let mut s = OutputDamageState::new();
        let before = s.dirty_gen();
        s.bump_dirty();
        assert_eq!(s.dirty_gen(), before + 1);
    }

    #[test]
    fn record_submit_marks_flip_pending() {
        let mut s = OutputDamageState::new();
        s.record_submit();
        assert!(s.flip_pending());
        assert!(
            !s.needs_composite(),
            "flip_pending blocks composite even if dirty"
        );
    }

    #[test]
    fn record_present_clears_flip_pending_and_advances_presented() {
        let mut s = OutputDamageState::new();
        s.record_submit();
        let submitted = s.last_submitted_gen();
        s.record_present();
        assert!(!s.flip_pending());
        assert_eq!(s.last_presented_gen(), submitted);
    }

    #[test]
    fn skip_then_catch_up_preserves_dirty() {
        // Output goes dirty, gets skipped (flip_pending elsewhere
        // was true), state remains dirty after the pending flip
        // retires. This is the load-bearing invariant: skipping
        // an output must never lose its dirty state.
        let mut s = OutputDamageState::new();
        s.record_submit();
        // While flip is pending, another producer bumps dirty.
        s.bump_dirty();
        assert!(s.flip_pending());
        assert!(!s.needs_composite()); // blocked
        s.record_present();
        assert!(
            s.needs_composite(),
            "post-retire, the bump that arrived during the flip \
             must keep the output dirty"
        );
    }

    #[test]
    fn idle_after_present_does_not_need_composite() {
        let mut s = OutputDamageState::new();
        s.record_submit();
        s.record_present();
        assert!(!s.needs_composite());
    }

    #[test]
    fn multiple_bumps_without_submit_still_need_composite() {
        let mut s = OutputDamageState::new();
        s.bump_dirty();
        s.bump_dirty();
        s.bump_dirty();
        assert!(s.needs_composite());
        assert_eq!(s.dirty_gen(), 4); // initial 1 + three bumps
    }
}
