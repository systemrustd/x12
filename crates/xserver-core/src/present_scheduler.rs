//! X11 Present extension scheduler (Phase 4.2 design §3.3).
//!
//! Per-window FIFO of queued `PresentPixmap` / `PresentPixmapSynced`
//! requests. At each output's vblank the scheduler:
//! 1. Computes `next_msc` from the queued frame's
//!    `target_msc / divisor / remainder` (§3.3.3).
//! 2. Picks the latest frame whose schedule predicate is satisfied;
//!    earlier frames on the same window are marked `Skip` and emit
//!    `CompleteNotify { mode: Skip }`.
//! 3. Submits the chosen frame's path (Copy / Flip / DirectScanout).
//!
//! Phase 4.2.3 first cut wires the data structures; the
//! vblank-driven submission machinery hooks in via
//! `kms::backend::KmsBackend::on_page_flip_ready` (Tasks 26-27).

use std::collections::{HashMap, VecDeque};

use yserver_protocol::x11::ResourceId;

/// Path the Present scheduler picked for a queued frame. Phase 4.2.3
/// only exercises `Copy`; `Flip` / `DirectScanout` arrive in 4.2.4.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentPath {
    /// Composite-pass copies the imported pixmap into the window's
    /// existing scanout. `IdleNotify` fires at GPU-read-completion;
    /// `CompleteNotify { mode: Copy }` fires on pageflip-complete of
    /// the composite frame.
    Copy,
    /// Atomic-flip the alien BO (the imported pixmap). `IdleNotify`
    /// fires when the *next* Present completes on the same window
    /// (presentproto Flip retention rule).
    Flip { alien_bo: u32 },
    /// Same as Flip but skip the composite pass entirely; alien BO
    /// becomes the scanout image directly. Cursor renders on the
    /// cursor plane.
    DirectScanout { alien_bo: u32 },
}

/// Sync-resource handle attached to a queued Present. Carries either
/// a binary `Sync::Fence` XID (PresentPixmap v1.0) or a timeline
/// `Sync::Syncobj` XID + value (PresentPixmapSynced v1.4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentSync {
    Binary {
        /// 0 = unset.
        fence: u32,
    },
    Timeline {
        /// 0 = unset.
        syncobj: u32,
        value: u64,
    },
}

impl PresentSync {
    #[must_use]
    pub fn is_unset(self) -> bool {
        matches!(
            self,
            Self::Binary { fence: 0 } | Self::Timeline { syncobj: 0, .. }
        )
    }
}

/// One queued Present request waiting for a vblank to fire.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedPresent {
    /// Client-supplied serial; echoes back in CompleteNotify /
    /// IdleNotify.
    pub serial: u32,
    /// X resource id of the source pixmap.
    pub pixmap: ResourceId,
    /// X resource id of the destination window.
    pub window: ResourceId,
    /// `PresentOption*` bitmap. AsyncMayTear-silent-clear (Task 30)
    /// happens at request-ingress before the request lands here.
    pub options: u32,
    /// Vblank scheduling parameters per `presentproto`.
    pub target_msc: u64,
    pub divisor: u64,
    pub remainder: u64,
    /// Wait-on-this-before-submit semaphore.
    pub wait: PresentSync,
    /// Signal-this-on-idle semaphore.
    pub idle: PresentSync,
    /// Path the selector picked at queue time.
    pub path: PresentPath,
    /// `valid_region` xid (0 = none).
    pub valid_region: u32,
    /// `update_region` xid (0 = none).
    pub update_region: u32,
}

/// `PresentOptionCopy` bit per `presentproto`.
pub const PRESENT_OPTION_COPY: u32 = 0x2;

/// Inputs to [`choose_path`]. Sourced by the dispatcher from the
/// parsed request + the backend's pixmap/window/output state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PathSelectorInputs<'a> {
    /// `PresentOption*` bitmap from the request (after Task 30
    /// silent-clear of AsyncMayTear).
    pub options: u32,
    /// `valid_region` xid; `0` means "full window".
    pub valid_region: u32,
    /// `update_region` xid; `0` means "full window".
    pub update_region: u32,
    /// Pixmap dimensions in pixels.
    pub pixmap_w: u16,
    pub pixmap_h: u16,
    /// `(format_fourcc, modifier)` — the scanout-compat lookup key.
    /// Phase 4.2 only sets these for imported pixmaps; server-
    /// allocated pixmaps default to `(0, 0)` and never go on the
    /// flip path because that pair will never be in the scanout-
    /// compat set.
    pub pixmap_format: u32,
    pub pixmap_modifier: u64,
    /// Destination window's outer extent in pixels.
    pub window_w: u16,
    pub window_h: u16,
    /// Whether the window's geometry covers its output exactly.
    /// Required for DirectScanout. Phase 4.2 first cut treats this
    /// as a single-output approximation: window dims match output dims.
    pub window_covers_output: bool,
    /// Output mode dimensions in pixels.
    pub output_w: u16,
    pub output_h: u16,
    /// `(format, modifier)` pairs the kernel accepted via `add_fb2`
    /// at backend init. Empty when the kernel rejected the probe.
    pub output_scanout_format_set: &'a [(u32, u64)],
}

/// Pick the Present path for a queued frame per design §3.3.1.
///
/// `caps.flip_path == false` short-circuits to `Copy` regardless —
/// the explicit-fence flip handshake is required for client-imported
/// BO scanout and there's no degraded variant.
#[must_use]
pub fn choose_path(
    inputs: &PathSelectorInputs,
    flip_path_supported: bool,
    alien_bo_for: impl FnOnce() -> Option<u32>,
) -> PresentPath {
    if !flip_path_supported {
        return PresentPath::Copy;
    }
    if (inputs.options & PRESENT_OPTION_COPY) != 0 {
        return PresentPath::Copy;
    }
    let scanout_compat = inputs
        .output_scanout_format_set
        .contains(&(inputs.pixmap_format, inputs.pixmap_modifier));
    let regions_full = inputs.valid_region == 0 && inputs.update_region == 0;
    let fullscreen_exact = inputs.pixmap_w == inputs.output_w
        && inputs.pixmap_h == inputs.output_h
        && inputs.window_covers_output
        && regions_full
        && scanout_compat;
    match (fullscreen_exact, scanout_compat) {
        (true, _) => alien_bo_for().map_or(PresentPath::Copy, |alien_bo| {
            PresentPath::DirectScanout { alien_bo }
        }),
        (false, true) => {
            alien_bo_for().map_or(PresentPath::Copy, |alien_bo| PresentPath::Flip { alien_bo })
        }
        (false, false) => PresentPath::Copy,
    }
}

/// Per-window FIFO of queued Presents. Keyed by destination window xid.
/// Inserted-in-order; the scheduler walks each FIFO at vblank.
#[derive(Default, Debug)]
pub struct PresentScheduler {
    queues: HashMap<ResourceId, VecDeque<QueuedPresent>>,
    /// `last_flipped` per window: handle of the most recent Flipped /
    /// DirectScanouted alien BO. Per `presentproto` the previous
    /// flipped pixmap stays in use until the *next* Present completes
    /// on the same window — at which point the previous's IdleNotify
    /// fires and the import release happens.
    last_flipped: HashMap<ResourceId, u32>,
}

impl PresentScheduler {
    /// Push a new queued frame at the back of the per-window FIFO.
    pub fn enqueue(&mut self, frame: QueuedPresent) {
        self.queues
            .entry(frame.window)
            .or_default()
            .push_back(frame);
    }

    /// Drain all queued frames for a destroyed window (no events
    /// emitted — the window is gone). Per design §3.3.2 teardown
    /// rules. Returns the drained frames so the caller can signal
    /// any attached idle-syncobj semaphores.
    pub fn drain_window(&mut self, window: ResourceId) -> Vec<QueuedPresent> {
        let mut out = Vec::new();
        if let Some(q) = self.queues.remove(&window) {
            out.extend(q);
        }
        self.last_flipped.remove(&window);
        out
    }

    /// Drain all queued frames belonging to a closing client. Returns
    /// every frame the client queued (potentially against other
    /// clients' windows). Caller signals idle semaphores.
    pub fn drain_client(&mut self, _client_id: u32) -> Vec<QueuedPresent> {
        // Phase 4.2.3 first cut tracks owner via the client_id stored
        // on the queue entry — but `QueuedPresent` is a copy of the
        // wire request and doesn't carry that today. Task 32 widens
        // the struct as part of teardown rules. Stub for now.
        Vec::new()
    }

    /// Whether a queued frame's schedule predicate is satisfied for a
    /// given `current_msc`. Implements the §3.3.3 formula:
    /// - `divisor == 0`: ready when `current_msc >= target_msc`.
    /// - `divisor > 0`: ready when `current_msc >= target_msc` *and*
    ///   `current_msc % divisor == remainder`.
    #[must_use]
    pub fn schedule_satisfied(frame: &QueuedPresent, current_msc: u64) -> bool {
        if current_msc < frame.target_msc {
            return false;
        }
        if frame.divisor == 0 {
            return true;
        }
        current_msc % frame.divisor == frame.remainder
    }

    /// Walk the queue for `window` at `current_msc`. Returns:
    /// - `chosen`: the latest frame whose schedule predicate is
    ///   satisfied (the one to submit), if any.
    /// - `skipped`: every earlier frame on the same window — these
    ///   should emit `CompleteNotify { mode: Skip }`.
    pub fn pick_at_vblank(
        &mut self,
        window: ResourceId,
        current_msc: u64,
    ) -> (Option<QueuedPresent>, Vec<QueuedPresent>) {
        let Some(q) = self.queues.get_mut(&window) else {
            return (None, Vec::new());
        };
        // Find the index of the latest frame whose schedule is
        // satisfied. Walk back-to-front so we pick the latest.
        let chosen_idx = q
            .iter()
            .enumerate()
            .rev()
            .find(|(_, f)| Self::schedule_satisfied(f, current_msc))
            .map(|(i, _)| i);
        let Some(idx) = chosen_idx else {
            return (None, Vec::new());
        };
        // Drain everything up to and including idx.
        let mut skipped: Vec<QueuedPresent> = q.drain(..idx).collect();
        let chosen = q.pop_front();
        // The "chosen" frame is the last of skipped+chosen; everything
        // earlier is skipped.
        if let Some(frame) = chosen {
            (Some(frame), skipped)
        } else {
            // Shouldn't happen — chosen_idx was Some so q had >= idx+1
            // entries — but be defensive.
            (None, std::mem::take(&mut skipped))
        }
    }

    /// Update `last_flipped` for a window after a successful Flip /
    /// DirectScanout. Returns the previously flipped handle (if any)
    /// — caller fires its IdleNotify per the retention rule.
    pub fn record_flipped(&mut self, window: ResourceId, alien_bo: u32) -> Option<u32> {
        self.last_flipped.insert(window, alien_bo)
    }

    /// Whether a window currently retains a flipped alien BO.
    #[must_use]
    pub fn last_flipped(&self, window: ResourceId) -> Option<u32> {
        self.last_flipped.get(&window).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(serial: u32, target_msc: u64, divisor: u64, remainder: u64) -> QueuedPresent {
        QueuedPresent {
            serial,
            pixmap: ResourceId(0xCAFE),
            window: ResourceId(0xBEEF),
            options: 0,
            target_msc,
            divisor,
            remainder,
            wait: PresentSync::Binary { fence: 0 },
            idle: PresentSync::Binary { fence: 0 },
            path: PresentPath::Copy,
            valid_region: 0,
            update_region: 0,
        }
    }

    #[test]
    fn schedule_satisfied_target_msc_zero() {
        // target_msc=0 means "next vblank"; satisfied at any current_msc.
        let f = frame(1, 0, 0, 0);
        assert!(PresentScheduler::schedule_satisfied(&f, 0));
        assert!(PresentScheduler::schedule_satisfied(&f, 100));
    }

    #[test]
    fn schedule_satisfied_divisor_constraint() {
        // divisor=2, remainder=0: only even MSCs.
        let f = frame(1, 10, 2, 0);
        assert!(!PresentScheduler::schedule_satisfied(&f, 9));
        assert!(!PresentScheduler::schedule_satisfied(&f, 11));
        assert!(PresentScheduler::schedule_satisfied(&f, 10));
        assert!(PresentScheduler::schedule_satisfied(&f, 12));
    }

    #[test]
    fn pick_collapses_to_latest_with_skips() {
        let mut sched = PresentScheduler::default();
        sched.enqueue(frame(1, 5, 0, 0));
        sched.enqueue(frame(2, 6, 0, 0));
        sched.enqueue(frame(3, 7, 0, 0));
        let (chosen, skipped) = sched.pick_at_vblank(ResourceId(0xBEEF), 100);
        assert_eq!(chosen.unwrap().serial, 3);
        let serials: Vec<u32> = skipped.iter().map(|f| f.serial).collect();
        assert_eq!(serials, vec![1, 2]);
    }

    #[test]
    fn pick_none_when_nothing_satisfied() {
        let mut sched = PresentScheduler::default();
        sched.enqueue(frame(1, 100, 0, 0));
        let (chosen, skipped) = sched.pick_at_vblank(ResourceId(0xBEEF), 50);
        assert!(chosen.is_none());
        assert!(skipped.is_empty());
    }

    #[test]
    fn drain_window_returns_all_queued() {
        let mut sched = PresentScheduler::default();
        sched.enqueue(frame(1, 0, 0, 0));
        sched.enqueue(frame(2, 0, 0, 0));
        let drained = sched.drain_window(ResourceId(0xBEEF));
        assert_eq!(drained.len(), 2);
        assert!(sched.queues.is_empty());
    }

    fn default_inputs<'a>() -> PathSelectorInputs<'a> {
        PathSelectorInputs {
            options: 0,
            valid_region: 0,
            update_region: 0,
            pixmap_w: 1920,
            pixmap_h: 1080,
            pixmap_format: 0x3443_3258, // X8R8G8B8 fourcc 'XR24' (placeholder)
            pixmap_modifier: 0,
            window_w: 1920,
            window_h: 1080,
            window_covers_output: true,
            output_w: 1920,
            output_h: 1080,
            output_scanout_format_set: &[(0x3443_3258, 0)],
        }
    }

    #[test]
    fn choose_path_present_option_copy_forces_copy() {
        let mut inputs = default_inputs();
        inputs.options = PRESENT_OPTION_COPY;
        let path = choose_path(&inputs, true, || Some(0x1000));
        assert_eq!(path, PresentPath::Copy);
    }

    #[test]
    fn choose_path_fullscreen_picks_direct_scanout() {
        let inputs = default_inputs();
        let path = choose_path(&inputs, true, || Some(0x1000));
        assert_eq!(path, PresentPath::DirectScanout { alien_bo: 0x1000 });
    }

    #[test]
    fn choose_path_non_fullscreen_compatible_picks_flip() {
        let mut inputs = default_inputs();
        inputs.window_w = 800;
        inputs.window_h = 600;
        inputs.pixmap_w = 800;
        inputs.pixmap_h = 600;
        inputs.window_covers_output = false;
        let path = choose_path(&inputs, true, || Some(0x1000));
        assert_eq!(path, PresentPath::Flip { alien_bo: 0x1000 });
    }

    #[test]
    fn choose_path_incompatible_format_picks_copy() {
        let mut inputs = default_inputs();
        inputs.output_scanout_format_set = &[]; // nothing accepted
        let path = choose_path(&inputs, true, || Some(0x1000));
        assert_eq!(path, PresentPath::Copy);
    }

    #[test]
    fn choose_path_no_flip_path_short_circuits_to_copy() {
        let inputs = default_inputs();
        // flip_path_supported = false → Copy regardless.
        let path = choose_path(&inputs, false, || Some(0x1000));
        assert_eq!(path, PresentPath::Copy);
    }

    #[test]
    fn choose_path_missing_alien_bo_falls_back_to_copy() {
        // alien_bo_for returns None → fall back to Copy even for
        // fullscreen-exact (defensive: no scanout buffer to actually
        // flip to).
        let inputs = default_inputs();
        let path = choose_path(&inputs, true, || None);
        assert_eq!(path, PresentPath::Copy);
    }

    #[test]
    fn record_flipped_returns_previous_handle() {
        let mut sched = PresentScheduler::default();
        let prev = sched.record_flipped(ResourceId(0xBEEF), 0x1000);
        assert!(prev.is_none());
        let prev2 = sched.record_flipped(ResourceId(0xBEEF), 0x2000);
        assert_eq!(prev2, Some(0x1000));
    }
}
