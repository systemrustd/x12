# Rendering re-architecture — phase 1 implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up the frame-ownership primitives (`OutputDamageState`, `InFlight`, `PaintBatch`, `OutputFrame`, `RenderScheduler`) from the HLD, replace the global `screen_dirty: bool` with per-output dirty generations (with old∪new geometry propagation), and route composite scheduling through `InFlight` with two-stage retirement. Paint recorders and hot-path `vkQueueWaitIdle` calls stay unchanged.

**Architecture:** New module `crates/yserver/src/kms/scheduler/` hosts five pure(ish) types. `KmsBackend` gains a `scheduler: RenderScheduler` field; `OutputLayout` gains a `damage: OutputDamageState` field. `composite_and_flip` keeps its current per-output skip logic but reads per-output damage, wraps `try_vulkan_composite_flip` in `OutputFrame` creation, pushes the frame into `InFlight`, and `InFlight` is polled non-blocking at every quiescent point for two-stage retirement (composite `VkFence` for GPU, KMS release fd for scanout).

**Tech Stack:** Rust 2021, `ash` for Vulkan, `drm-rs` for KMS, existing `vk/` and `drm/` submodules. No new external dependencies.

**Reference:** `docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md` is the source of truth. `dead-ends/POSTMORTEM.md` explains why phase 1 deliberately stops short of sync rework or recorder migration.

---

## Pre-task: global checks

Every task ends with the same three commands before commit:

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Fix all warnings (clippy pedantic is project policy). Tests must be green. Don't `--no-verify`.

## File structure

New files:

- `crates/yserver/src/kms/scheduler/mod.rs` — module declaration and `RenderScheduler` struct.
- `crates/yserver/src/kms/scheduler/damage.rs` — `OutputDamageState`.
- `crates/yserver/src/kms/scheduler/paint_batch.rs` — `PaintBatch` shell.
- `crates/yserver/src/kms/scheduler/output_frame.rs` — `OutputFrame`.
- `crates/yserver/src/kms/scheduler/in_flight.rs` — `InFlight` and `InFlightFrame`.
- `docs/superpowers/specs/2026-05-12-waitidle-catalogue.md` — pure documentation; lifetime classification of every current `vkQueueWaitIdle` site.

Modified files:

- `crates/yserver/src/kms/mod.rs` — add `pub mod scheduler;`.
- `crates/yserver/src/kms/backend.rs` — replace `screen_dirty: bool` with per-output damage; add `scheduler` field; cut over `composite_and_flip`; update `mark_dirty`; geometry-change call sites get old∪new propagation; rewrite the `screen_dirty_*` tests against the new model.

---

## Task 1: Catalogue every `vkQueueWaitIdle` site

**Files:**
- Create: `docs/superpowers/specs/2026-05-12-waitidle-catalogue.md`

Goal: a current-tree inventory that phase 3/4 uses as a target list. Each site classified as **sync** (hot-path; the rework removes this), **readback** (CPU needs GPU output; targeted fence wait replaces it), **teardown** (lifetime; stays), or **temporary** (compatibility scaffolding; remove later).

- [ ] **Step 1: Enumerate sites**

Run:

```bash
rg -n 'queue_wait_idle' crates/yserver/
```

Classify **every** hit. Do not skip any — even one-liners in `Drop` impls or destructors. Spot-check by also running:

```bash
rg -n 'vkQueueWaitIdle|device_wait_idle' crates/yserver/
```

The catalogue is the input list for phase 3 and phase 4; under-counting now produces gaps later.

- [ ] **Step 2: Write the catalogue**

For each hit, read the surrounding 10-20 lines. Classify each into one of:

- **sync** — the wait gates a *frame's* GPU work so the next CPU step can proceed (the thing the rework kills).
- **readback** — the host needs to read GPU-written bytes synchronously (`GetImage` and friends).
- **teardown** — the wait gates an object's *lifetime* end (`Drop`, pipeline cache rebuild, image destroy on resize). Stays.
- **temporary** — placeholder scaffolding that exists only because of the current eager-submit cadence (e.g. `OpsStaging::ensure` resize waits because there's no in-flight resource queue yet). Replaced by `ResourceRetireQueue`-like bookkeeping in a later phase.

Use this table shape:

```markdown
| File:Line | Surrounding function | Classification | Removal phase | Notes |
|---|---|---|---|---|
| `vk/ops/mod.rs:100` | `run_one_shot_op` | sync | phase 4 | The canonical hot-path drain. |
| `vk/ops/mod.rs:59` | `OpsCommandPool::drop` | teardown | stays | Pool drop must wait; CBs live across frames. |
| ... | ... | ... | ... | ... |
```

Include the existing dead-end spec's enumeration as a starting point (`docs/superpowers/specs/dead-ends/2026-05-12-paint-composite-sync-design.md`, "Queue drain surface" section) but verify each line against the current tree — line numbers may have drifted.

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/specs/2026-05-12-waitidle-catalogue.md
git commit -m "docs: catalogue vkQueueWaitIdle sites with lifetime classification"
```

---

## Task 2: `OutputDamageState` type

**Files:**
- Create: `crates/yserver/src/kms/scheduler/mod.rs`
- Create: `crates/yserver/src/kms/scheduler/damage.rs`
- Modify: `crates/yserver/src/kms/mod.rs` (add `pub mod scheduler;`)

- [ ] **Step 1: Write the failing tests**

Create `crates/yserver/src/kms/scheduler/damage.rs`:

```rust
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
}
```

- [ ] **Step 2: Add the module wiring**

Create `crates/yserver/src/kms/scheduler/mod.rs`:

```rust
//! Frame-ownership and scheduling primitives.
//!
//! See `docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md`.
//! Phase 1 lands the types with minimal behavior; recorders and the
//! hot-path `vkQueueWaitIdle` calls are unchanged.

pub mod damage;
```

Edit `crates/yserver/src/kms/mod.rs`. Find the existing module list and add:

```rust
pub mod scheduler;
```

- [ ] **Step 3: Run tests to verify they fail**

```bash
cargo test -p yserver kms::scheduler::damage
```

Expected: compilation error — the methods `dirty_gen`, `bump_dirty`, `needs_composite`, `record_submit`, `record_present`, `flip_pending`, `last_submitted_gen`, `last_presented_gen` don't exist yet.

- [ ] **Step 4: Implement the methods**

Append to `crates/yserver/src/kms/scheduler/damage.rs`:

```rust
impl OutputDamageState {
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
    pub fn record_submit(&mut self) {
        self.last_submitted_gen = self.dirty_gen;
        self.flip_pending = true;
    }

    /// The pageflip-complete event fired for this output. The
    /// presented generation advances to whatever was last submitted;
    /// any bumps that arrived between submit and present remain in
    /// `dirty_gen` and re-arm `needs_composite`.
    pub fn record_present(&mut self) {
        self.last_presented_gen = self.last_submitted_gen;
        self.flip_pending = false;
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test -p yserver kms::scheduler::damage
```

Expected: 6 tests pass.

- [ ] **Step 6: Format, clippy, commit**

```bash
cargo +nightly fmt
cargo clippy
cargo test
git add crates/yserver/src/kms/scheduler/ crates/yserver/src/kms/mod.rs
git commit -m "feat(scheduler): add OutputDamageState (per-output dirty generations)"
```

---

## Task 3: `InFlight` queue with two-stage retirement

**Files:**
- Create: `crates/yserver/src/kms/scheduler/in_flight.rs`
- Modify: `crates/yserver/src/kms/scheduler/mod.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/yserver/src/kms/scheduler/in_flight.rs`:

```rust
//! Server-wide in-flight frame retirement queue.
//!
//! Tracks `OutputFrame`s that have been submitted but not yet
//! fully retired. Each frame has two retirement points:
//!
//! 1. GPU retirement — composite work complete on the GPU.
//!    Releases composite command buffer, descriptors, scratch
//!    bound to this frame. Phase 1 uses a `VkFence` polled via
//!    `vkGetFenceStatus`; later phases may switch to a timeline
//!    counter.
//!
//! 2. Scanout retirement — KMS has released the scanout BO via
//!    pageflip-complete (the BO is no longer on-screen). Releases
//!    the BO slot back to the pool.
//!
//! A frame is fully retired (and removed from the queue) only when
//! both bools are true. Phase 1 may unify the implementation; the
//! two-point split is the invariant.

use std::collections::VecDeque;

use ash::vk;

/// A single in-flight `OutputFrame`'s retirement bookkeeping.
///
/// The fields are public to the scheduler module so the polling
/// code (which lives in `KmsBackend` because it owns `VkContext`
/// and the BO pools) can set the bools directly.
#[derive(Debug)]
pub struct InFlightFrame {
    pub output_idx: usize,
    pub frame_id: u64,
    pub submitted_gen: u64,
    pub composite_fence: vk::Fence,
    pub bo_slot: Option<usize>,
    pub gpu_retired: bool,
    pub scanout_retired: bool,
}

impl InFlightFrame {
    pub fn fully_retired(&self) -> bool {
        self.gpu_retired && self.scanout_retired
    }
}

#[derive(Debug, Default)]
pub struct InFlight {
    frames: VecDeque<InFlightFrame>,
    next_frame_id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_frame(id: u64, output: usize) -> InFlightFrame {
        InFlightFrame {
            output_idx: output,
            frame_id: id,
            submitted_gen: id,
            composite_fence: vk::Fence::null(),
            bo_slot: Some(0),
            gpu_retired: false,
            scanout_retired: false,
        }
    }

    #[test]
    fn new_queue_is_empty() {
        let q = InFlight::default();
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn next_frame_id_is_monotonic() {
        let mut q = InFlight::default();
        let a = q.allocate_frame_id();
        let b = q.allocate_frame_id();
        assert_eq!(b, a + 1);
    }

    #[test]
    fn push_grows_queue() {
        let mut q = InFlight::default();
        q.push(mk_frame(1, 0));
        q.push(mk_frame(2, 1));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn drain_retired_removes_only_fully_retired_frames() {
        let mut q = InFlight::default();
        q.push(mk_frame(1, 0));
        q.push(mk_frame(2, 0));
        // GPU done on frame 1, scanout not yet.
        q.frames_mut().next().unwrap().gpu_retired = true;
        let drained = q.drain_retired();
        assert_eq!(drained, 0, "GPU-only retirement is not enough");
        // Scanout also done on frame 1.
        q.frames_mut().next().unwrap().scanout_retired = true;
        let drained = q.drain_retired();
        assert_eq!(drained, 1);
        assert_eq!(q.len(), 1);
        assert_eq!(q.frames().next().unwrap().frame_id, 2);
    }

    #[test]
    fn drain_retired_only_drains_prefix() {
        // Frames retire in submission order on the same output. The
        // queue is FIFO; out-of-order retirement (e.g. a later
        // frame's GPU work completing before an earlier frame's)
        // does not cause a hole — the later frame waits for the
        // earlier to drain.
        let mut q = InFlight::default();
        q.push(mk_frame(1, 0));
        q.push(mk_frame(2, 0));
        // Mark frame 2 fully retired, frame 1 not.
        let mut iter = q.frames_mut();
        let _f1 = iter.next();
        let f2 = iter.next().unwrap();
        f2.gpu_retired = true;
        f2.scanout_retired = true;
        let drained = q.drain_retired();
        assert_eq!(
            drained, 0,
            "must not drain frame 2 while frame 1 is still in-flight"
        );
        assert_eq!(q.len(), 2);
    }
}
```

- [ ] **Step 2: Register the module**

Edit `crates/yserver/src/kms/scheduler/mod.rs`:

```rust
pub mod damage;
pub mod in_flight;
```

- [ ] **Step 3: Run tests to verify they fail**

```bash
cargo test -p yserver kms::scheduler::in_flight
```

Expected: compilation error — `len`, `allocate_frame_id`, `push`, `frames`, `frames_mut`, `drain_retired` don't exist.

- [ ] **Step 4: Implement the methods**

Append to `crates/yserver/src/kms/scheduler/in_flight.rs`:

```rust
impl InFlight {
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn allocate_frame_id(&mut self) -> u64 {
        self.next_frame_id += 1;
        self.next_frame_id
    }

    pub fn push(&mut self, frame: InFlightFrame) {
        self.frames.push_back(frame);
    }

    pub fn frames(&self) -> impl Iterator<Item = &InFlightFrame> {
        self.frames.iter()
    }

    pub fn frames_mut(&mut self) -> impl Iterator<Item = &mut InFlightFrame> {
        self.frames.iter_mut()
    }

    /// Drain the prefix of fully-retired frames. Returns how many
    /// were drained. Stops at the first non-retired frame —
    /// out-of-order retirement is allowed in the bools but not in
    /// the queue, because resource lifetimes are layered on
    /// submission order.
    pub fn drain_retired(&mut self) -> usize {
        let mut drained = 0;
        while let Some(front) = self.frames.front() {
            if front.fully_retired() {
                self.frames.pop_front();
                drained += 1;
            } else {
                break;
            }
        }
        drained
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cargo test -p yserver kms::scheduler::in_flight
```

Expected: 5 tests pass.

- [ ] **Step 6: Format, clippy, commit**

```bash
cargo +nightly fmt
cargo clippy
cargo test
git add crates/yserver/src/kms/scheduler/
git commit -m "feat(scheduler): add InFlight queue with two-stage retirement"
```

---

## Task 4: `PaintBatch` and `OutputFrame` shells

**Files:**
- Create: `crates/yserver/src/kms/scheduler/paint_batch.rs`
- Create: `crates/yserver/src/kms/scheduler/output_frame.rs`
- Modify: `crates/yserver/src/kms/scheduler/mod.rs`

Phase 1 keeps these minimal — named placeholders with the fields phase-2/3/4 will fill. The test asserts the construction shape so later phases adding real content don't accidentally change the public surface.

- [ ] **Step 1: Write the failing tests**

Create `crates/yserver/src/kms/scheduler/paint_batch.rs`:

```rust
//! A frame's accumulated paint work.
//!
//! Phase 1: shell. The batch is opened at the start of a composite
//! cycle and closed at the end. Recorders still call
//! `run_one_shot_op` directly in phase 1, so the batch carries no
//! Vulkan work yet — only the frame_id and the set of outputs that
//! will composite this cycle.
//!
//! Phase 2 fills in the per-frame primary command buffer,
//! descriptor pool, and scratch arena. Phase 3 migrates recorders
//! to append into the batch instead of submitting directly.

#[derive(Debug)]
pub struct PaintBatch {
    pub frame_id: u64,
    /// Outputs that will composite from this batch (`C(F)` in the
    /// HLD). Captured at batch close time. Phase 1: populated for
    /// shape but not yet load-bearing.
    pub dirty_outputs: Vec<usize>,
}

impl PaintBatch {
    pub fn new(frame_id: u64) -> Self {
        Self {
            frame_id,
            dirty_outputs: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_batch_has_empty_output_set() {
        let b = PaintBatch::new(42);
        assert_eq!(b.frame_id, 42);
        assert!(b.dirty_outputs.is_empty());
    }
}
```

Create `crates/yserver/src/kms/scheduler/output_frame.rs`:

```rust
//! One composited frame for one output.
//!
//! Phase 1 carries just enough state to push into `InFlight`. The
//! command buffer, descriptors, and the wait dependency on
//! `PaintBatch` arrive in phase 2/3/4.

use ash::vk;

#[derive(Debug)]
pub struct OutputFrame {
    pub output_idx: usize,
    pub frame_id: u64,
    pub submitted_gen: u64,
    pub composite_fence: vk::Fence,
    pub bo_slot: Option<usize>,
}

impl OutputFrame {
    pub fn new(
        output_idx: usize,
        frame_id: u64,
        submitted_gen: u64,
        composite_fence: vk::Fence,
        bo_slot: Option<usize>,
    ) -> Self {
        Self {
            output_idx,
            frame_id,
            submitted_gen,
            composite_fence,
            bo_slot,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_frame_records_all_fields() {
        let f = OutputFrame::new(0, 1, 7, vk::Fence::null(), Some(2));
        assert_eq!(f.output_idx, 0);
        assert_eq!(f.frame_id, 1);
        assert_eq!(f.submitted_gen, 7);
        assert_eq!(f.bo_slot, Some(2));
    }
}
```

- [ ] **Step 2: Register the modules**

Edit `crates/yserver/src/kms/scheduler/mod.rs`:

```rust
pub mod damage;
pub mod in_flight;
pub mod output_frame;
pub mod paint_batch;
```

- [ ] **Step 3: Run tests to verify they pass**

```bash
cargo test -p yserver kms::scheduler
```

Expected: tests for `paint_batch` and `output_frame` pass; the earlier `damage` and `in_flight` tests still pass.

- [ ] **Step 4: Format, clippy, commit**

```bash
cargo +nightly fmt
cargo clippy
cargo test
git add crates/yserver/src/kms/scheduler/
git commit -m "feat(scheduler): add PaintBatch and OutputFrame shells"
```

---

## Task 5: `RenderScheduler` shell

**Files:**
- Modify: `crates/yserver/src/kms/scheduler/mod.rs`

- [ ] **Step 1: Write the failing tests**

Append to `crates/yserver/src/kms/scheduler/mod.rs`:

```rust
use self::in_flight::InFlight;
use self::paint_batch::PaintBatch;

/// Server-wide scheduling state. Owned as a single field on
/// `KmsBackend`. Per-output damage lives on `OutputLayout`, not
/// here — `OutputLayout` is the natural home for per-output state.
///
/// Phase 1: a shell wrapping the in-flight queue and the
/// current paint batch. The "only layer allowed to submit on the
/// hot path" invariant from the HLD is not yet enforced — phase 1
/// recorders still call `run_one_shot_op` directly.
#[derive(Debug, Default)]
pub struct RenderScheduler {
    pub in_flight: InFlight,
    pub current_paint_batch: Option<PaintBatch>,
}

impl RenderScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a paint batch for this composite cycle if one isn't
    /// already open. Returns the batch's `frame_id`. Phase 1:
    /// called from `composite_and_flip` at the start of each cycle.
    pub fn open_batch(&mut self) -> u64 {
        if let Some(batch) = self.current_paint_batch.as_ref() {
            return batch.frame_id;
        }
        let frame_id = self.in_flight.allocate_frame_id();
        self.current_paint_batch = Some(PaintBatch::new(frame_id));
        frame_id
    }

    /// Close the current batch. Phase 1: nothing to flush; the
    /// batch is just discarded. Phase 2+ submits the per-frame CB.
    pub fn close_batch(&mut self) -> Option<PaintBatch> {
        self.current_paint_batch.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_batch_allocates_monotonic_frame_ids() {
        let mut s = RenderScheduler::new();
        let a = s.open_batch();
        s.close_batch();
        let b = s.open_batch();
        assert!(b > a);
    }

    #[test]
    fn open_batch_is_idempotent_within_a_cycle() {
        let mut s = RenderScheduler::new();
        let a = s.open_batch();
        let b = s.open_batch();
        assert_eq!(a, b, "re-opening without closing returns the same frame_id");
    }

    #[test]
    fn close_batch_drops_current() {
        let mut s = RenderScheduler::new();
        s.open_batch();
        assert!(s.current_paint_batch.is_some());
        s.close_batch();
        assert!(s.current_paint_batch.is_none());
    }
}
```

- [ ] **Step 2: Run tests**

```bash
cargo test -p yserver kms::scheduler
```

Expected: all scheduler tests pass.

- [ ] **Step 3: Format, clippy, commit**

```bash
cargo +nightly fmt
cargo clippy
cargo test
git add crates/yserver/src/kms/scheduler/mod.rs
git commit -m "feat(scheduler): add RenderScheduler shell"
```

---

## Task 6: Wire `damage` into `OutputLayout`, `scheduler` into `KmsBackend`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs:594` (OutputLayout struct + new())
- Modify: `crates/yserver/src/kms/backend.rs:614` (KmsBackend struct)
- Modify: `crates/yserver/src/kms/backend.rs:1045` (KmsBackend::new for the for_tests path) and the production `open_with_commit`-style constructor wherever it builds `KmsBackend`.

No behavior change yet — adds fields, initializes them. The existing `screen_dirty: bool` stays in place; cutover is task 7.

- [ ] **Step 1: Add `damage` to `OutputLayout`**

Find `pub(crate) struct OutputLayout` (around line 594). Add the field:

```rust
pub(crate) struct OutputLayout {
    pub output: crate::drm::modeset::Output,
    pub swapchain: crate::drm::Swapchain,
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    pub damage: crate::kms::scheduler::damage::OutputDamageState,
}
```

Find every site that constructs `OutputLayout { ... }` (use `rg 'OutputLayout \{' crates/yserver/src/kms/`) and add `damage: crate::kms::scheduler::damage::OutputDamageState::new(),` to each literal.

- [ ] **Step 2: Add `scheduler` to `KmsBackend`**

In the `pub struct KmsBackend` definition (around line 614), add the field just before `screen_dirty`:

```rust
    pub(crate) scheduler: crate::kms::scheduler::RenderScheduler,
```

In every place that constructs `KmsBackend { ... }` (the `for_tests` path around 1045 and the production path — find with `rg 'KmsBackend \{' crates/yserver/src/`), add:

```rust
            scheduler: crate::kms::scheduler::RenderScheduler::new(),
```

- [ ] **Step 3: Verify the tree still compiles and tests still pass**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Expected: clean. The new fields are unused (clippy may warn — add `#[allow(dead_code)]` on the fields if needed; remove the allow in task 7).

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): wire OutputDamageState and RenderScheduler into backend"
```

---

## Task 7: Cut over `screen_dirty` to per-output damage (with idle-gate preserved)

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` — all `screen_dirty` write sites, the read sites in `composite_and_flip` and `maybe_composite`, the `mark_dirty` trait method, the per-output composite-submit site (to call `record_submit`), the pageflip-complete handler at `drain_page_flips_and_composite` (line 6784, to call `record_present`), and the tests at 11116-11147.

This is the atomic switch: writers, reader, field, **and the per-output advance points** (`record_submit` on successful submit; `record_present` on pageflip-complete) all change in one commit. Without the advance points, `dirty_gen > last_presented_gen` permanently and the idle gate is gone — `composite_and_flip` would run every event-loop cycle at idle. The advance points are what make `needs_composite()` correctly return `false` between presents.

Note: this task does *not* yet introduce `OutputFrame` or `InFlight` — that's task 9. Phase 1 deliberately lands the per-output damage state with its complete submit/present life-cycle first, so the in-flight queue in task 9 has a stable foundation to build on.

- [ ] **Step 1: Rewrite the existing dirty-flag tests against the new model**

Find the tests at `crates/yserver/src/kms/backend.rs:11116-11147` (`screen_dirty_initialises_true_so_first_frame_paints`, `composite_and_flip_clears_screen_dirty`, `mark_dirty_re_arms_screen_dirty`). Replace with:

```rust
    // ---------------------------------------------------------------------------
    // Per-output dirty generations: composite_and_flip skips outputs whose
    // last_presented_gen == dirty_gen, and clears nothing globally. The
    // previous `screen_dirty: bool` collapsed "anything anywhere changed"
    // with "this output needs a composite this tick"; per-output state
    // separates them so a skipped output catches up cleanly.
    // ---------------------------------------------------------------------------

    #[test]
    fn fresh_backend_has_every_output_dirty() {
        let backend = make_test_backend();
        for layout in &backend.outputs {
            assert!(
                layout.damage.needs_composite(),
                "fresh output must paint on first frame"
            );
        }
    }

    #[test]
    fn composite_and_flip_advances_submitted_gen() {
        let mut backend = make_test_backend();
        let before: Vec<u64> = backend
            .outputs
            .iter()
            .map(|l| l.damage.dirty_gen())
            .collect();
        backend.composite_and_flip().unwrap();
        for (i, layout) in backend.outputs.iter().enumerate() {
            // In the for_tests path there's no real Vulkan path, so
            // the submit may not occur — but if it did, submitted_gen
            // catches up to dirty_gen at the time of submit.
            assert!(
                layout.damage.last_submitted_gen() <= before[i],
                "submitted_gen never overruns dirty_gen at submit time"
            );
        }
    }

    #[test]
    fn mark_dirty_bumps_every_output() {
        use yserver_core::backend::Backend as _;
        let mut backend = make_test_backend();
        let before: Vec<u64> = backend
            .outputs
            .iter()
            .map(|l| l.damage.dirty_gen())
            .collect();
        backend.mark_dirty();
        for (i, layout) in backend.outputs.iter().enumerate() {
            assert!(
                layout.damage.dirty_gen() > before[i],
                "mark_dirty (no-arg) bumps every output"
            );
        }
    }
```

The first test replaces `screen_dirty_initialises_true_so_first_frame_paints`. The second replaces `composite_and_flip_clears_screen_dirty` — note the assertion shape is weaker because `for_tests` may not hit the full composite path; that's fine, the test guards the invariant, not the path coverage. The third replaces `mark_dirty_re_arms_screen_dirty` and gains the "every output" assertion.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p yserver fresh_backend_has_every_output_dirty composite_and_flip_advances_submitted_gen mark_dirty_bumps_every_output
```

Expected: compilation failures or assertion failures — the wiring isn't in place yet.

- [ ] **Step 3: Remove the `screen_dirty` field and writers**

Delete the field declaration at `crates/yserver/src/kms/backend.rs:760`:

```rust
    // (delete)
    pub(crate) screen_dirty: bool,
```

Find and delete every initialiser line `screen_dirty: true,` (current sites: 1318, 1882).

At `backend.rs:5849` find `self.screen_dirty = true;`. Replace with a call to a new helper:

```rust
        self.mark_all_outputs_dirty();
```

Add the helper method on `KmsBackend`:

```rust
    /// Bump dirty_gen on every output. Used by call sites that
    /// don't yet know which outputs were affected (any
    /// non-window-scoped change). Phase 1: equivalent in blast
    /// radius to the old global `screen_dirty = true`. Phase 2+
    /// can narrow producers that have window/region context.
    fn mark_all_outputs_dirty(&mut self) {
        for layout in &mut self.outputs {
            layout.damage.bump_dirty();
        }
    }
```

- [ ] **Step 4: Update `mark_dirty` trait method**

At `crates/yserver/src/kms/backend.rs:7613`, replace:

```rust
    fn mark_dirty(&mut self) {
        self.screen_dirty = true;
    }
```

with:

```rust
    fn mark_dirty(&mut self) {
        self.mark_all_outputs_dirty();
    }
```

- [ ] **Step 5: Update the `composite_and_flip` and `maybe_composite` readers**

Add a helper method on `KmsBackend`:

```rust
    fn any_output_needs_composite(&self) -> bool {
        self.outputs.iter().any(|l| l.damage.needs_composite())
    }
```

At `crates/yserver/src/kms/backend.rs:6186`, replace:

```rust
        if !self.screen_dirty {
            return Ok(());
        }
        self.screen_dirty = false;
```

with:

```rust
        if !self.any_output_needs_composite() {
            return Ok(());
        }
```

(No global clear — per-output state is advanced by `record_submit` / `record_present` in steps 6 and 7.)

At `crates/yserver/src/kms/backend.rs:7617` (the `maybe_composite` method), replace `if !self.screen_dirty` with `if !self.any_output_needs_composite()`.

- [ ] **Step 6: Per-output skip logic + `record_submit` on successful submit**

In `composite_and_flip` at `crates/yserver/src/kms/backend.rs:6209-6262`, the existing `vk_flip_pending` / `dumb_flip_pending` check stays. Before the existing check, add a per-output dirty filter:

```rust
            if !self.outputs[layout_idx].damage.needs_composite() {
                continue;
            }
```

After the call to `try_vulkan_composite_flip` returns `true` (the successful-submit branch), advance the output's damage state:

```rust
            if !self.try_vulkan_composite_flip(layout_idx, visible) {
                log::debug!(
                    "composite: deferring frame on output {} until a Free bo is available",
                    self.outputs[layout_idx].output.connector_name
                );
            } else {
                self.outputs[layout_idx].damage.record_submit();
                log::debug!(
                    "composite: submitted flip on output {} (visible={}, submitted_gen={})",
                    self.outputs[layout_idx].output.connector_name,
                    visible.len(),
                    self.outputs[layout_idx].damage.last_submitted_gen(),
                );
            }
```

This is what makes `needs_composite()` return `false` after submit (because `flip_pending = true`) and unblocks correctly when the pageflip-complete event clears it. Without it, the idle gate would be broken between this commit and task 9.

- [ ] **Step 7: Advance damage on pageflip-complete**

In `drain_page_flips_and_composite` at `crates/yserver/src/kms/backend.rs:6784-6847`, just after the `was_vk_flip` branch resolves (i.e. after `advance_pool_on_pageflip_complete(pool)` for the VK path, or after `layout.swapchain.complete(idx)` for the dumb path), call `record_present` for the output:

```rust
            self.outputs[output_idx].damage.record_present();
```

Place this inside the `for c in flipped { ... }` loop, after both the VK and dumb branches have settled the BO/swapchain state. After this commit, the dirty-generation life-cycle is closed: submit → flip_pending → present → next composite for that output is again eligible iff a producer bumped during the flip.

- [ ] **Step 8: Run all tests**

```bash
cargo test
```

Expected: the three new tests pass; the rest of the suite still passes. Watch for any test that relied on `backend.screen_dirty` — there should be none after step 1.

- [ ] **Step 9: Format, clippy, commit**

```bash
cargo +nightly fmt
cargo clippy
cargo test
git add crates/yserver/src/kms/backend.rs
git commit -m "refactor(kms): replace screen_dirty bool with per-output damage state"
```

---

## Task 8: Geometry-change old∪new dirty propagation

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` — every site that moves, resizes, unmaps, restacks, reparents, destroys, or maps a window. Today these implicitly rely on `mark_dirty` (which now bumps every output, so correctness is unchanged); this task narrows them to the right output set and adds the old∪new union rule.

This is the first task that *narrows* dirty propagation. Phase 1 covers geometry changes; paint ops keep using `mark_all_outputs_dirty` via the existing helper. Sub-output region tracking is a follow-up.

Strategy: land the helper with its direct unit tests first (independent of any configure-window plumbing), then migrate call sites in small groups. A high-level "move a real window across outputs" integration test is optional and only worth adding if the existing test fixture trivially supports two outputs — don't build new fixture scaffolding here.

- [ ] **Step 1: Write failing direct unit tests for the helper**

Append to the `tests` module in `backend.rs`:

```rust
    fn push_extra_output(backend: &mut KmsBackend, x: i32, width: u16) {
        // Build a second OutputLayout by cloning the construction
        // shape used inside make_test_backend(). The exact field
        // values come from for_tests() — only `x` and `width`
        // differ. If for_tests() doesn't expose a way to construct
        // a second OutputLayout, skip these tests with a
        // #[cfg_attr(not(feature = "fixture-two-outputs"), ignore)]
        // attribute and capture the limitation in the commit
        // message — phase 1 doesn't need this fixture to land.
        // ...
    }

    #[test]
    fn mark_window_dirty_with_old_rect_bumps_old_and_new_outputs() {
        let mut backend = make_test_backend();
        push_extra_output(&mut backend, /*x=*/ 1920, /*width=*/ 1920);
        let gen_a_before = backend.outputs[0].damage.dirty_gen();
        let gen_b_before = backend.outputs[1].damage.dirty_gen();
        // Output A at x=0..1920, output B at x=1920..3840.
        let old_rect = Rect { x: 50, y: 50, w: 100, h: 100 };   // on A
        let new_rect = Rect { x: 2000, y: 50, w: 100, h: 100 }; // on B
        backend.mark_window_dirty_with_old_rect(old_rect, new_rect);
        assert!(backend.outputs[0].damage.dirty_gen() > gen_a_before);
        assert!(backend.outputs[1].damage.dirty_gen() > gen_b_before);
    }

    #[test]
    fn mark_window_dirty_with_old_rect_does_not_bump_uninvolved_outputs() {
        let mut backend = make_test_backend();
        push_extra_output(&mut backend, 1920, 1920);
        push_extra_output(&mut backend, 3840, 1920);
        let gens_before: Vec<u64> = backend
            .outputs
            .iter()
            .map(|l| l.damage.dirty_gen())
            .collect();
        // Move within A — only A bumps.
        let old_rect = Rect { x: 50, y: 50, w: 100, h: 100 };
        let new_rect = Rect { x: 200, y: 50, w: 100, h: 100 };
        backend.mark_window_dirty_with_old_rect(old_rect, new_rect);
        assert!(backend.outputs[0].damage.dirty_gen() > gens_before[0]);
        assert_eq!(backend.outputs[1].damage.dirty_gen(), gens_before[1]);
        assert_eq!(backend.outputs[2].damage.dirty_gen(), gens_before[2]);
    }

    #[test]
    fn mark_window_dirty_with_old_rect_handles_empty_rect_as_no_bump() {
        // Empty rect (e.g. map: old = empty, new = current) overlaps nothing.
        let mut backend = make_test_backend();
        let gen_before = backend.outputs[0].damage.dirty_gen();
        let empty = Rect { x: 0, y: 0, w: 0, h: 0 };
        let new_rect = Rect { x: 50, y: 50, w: 100, h: 100 };
        backend.mark_window_dirty_with_old_rect(empty, new_rect);
        assert!(backend.outputs[0].damage.dirty_gen() > gen_before);
        // And the reverse — unmap with empty new.
        let gen_mid = backend.outputs[0].damage.dirty_gen();
        backend.mark_window_dirty_with_old_rect(new_rect, empty);
        assert!(backend.outputs[0].damage.dirty_gen() > gen_mid);
    }
```

If `push_extra_output` cannot be implemented without restructuring `make_test_backend`, drop the two/three-output tests for now and keep only the empty-rect test plus a single-output sanity test. Capture the fixture limitation in the commit message; the helper's *correctness* doesn't depend on multi-output fixtures, only on the iteration-and-intersection logic, which is verified by the empty-rect case.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -p yserver mark_window_dirty_with_old_rect
```

Expected: compilation error (`mark_window_dirty_with_old_rect` doesn't exist).

- [ ] **Step 3: Implement the helper**

Add to `KmsBackend`:

```rust
    /// Bump dirty on every output that intersects `old` ∪ `new`.
    /// Use this from geometry-change call sites (configure, restack,
    /// reparent, unmap, destroy, map). `old == new` is fine — bumps
    /// only the intersecting outputs.
    fn mark_window_dirty_with_old_rect(&mut self, old: Rect, new: Rect) {
        for layout in &mut self.outputs {
            let lr = layout.rect();
            if rects_overlap_axis_aligned(lr, old) || rects_overlap_axis_aligned(lr, new) {
                layout.damage.bump_dirty();
            }
        }
    }
```

(`rects_overlap_axis_aligned` already exists at `backend.rs:158`.)

- [ ] **Step 4: Verify helper tests pass; commit the helper**

```bash
cargo +nightly fmt
cargo clippy
cargo test -p yserver mark_window_dirty_with_old_rect
git add crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): add mark_window_dirty_with_old_rect helper for old∪new propagation"
```

Splitting call-site migration into its own commit keeps the helper landing reviewable on its own and bounds the next commit's diff.

- [ ] **Step 5: Migrate ConfigureWindow (move/resize)**

Find with `rg -n 'fn configure_window\b' crates/yserver/src/kms/backend.rs`. For each move/resize path: snapshot the pre-change rect from `WindowState`, apply the mutation, compute the post-change rect, then:

```rust
        self.mark_window_dirty_with_old_rect(pre, post);
```

Remove the corresponding `self.mark_dirty()` / `mark_all_outputs_dirty()` call that previously bumped every output. Run `cargo test`; commit:

```bash
cargo +nightly fmt && cargo clippy && cargo test
git commit -am "feat(kms): narrow ConfigureWindow dirty propagation to old∪new"
```

- [ ] **Step 6: Migrate Map / Unmap / Destroy**

- **MapWindow**: `mark_window_dirty_with_old_rect(empty, current_rect)`. Empty rect overlaps nothing; only `new` bumps. Use `Rect { x: 0, y: 0, w: 0, h: 0 }` for empty.
- **UnmapWindow** of a previously-mapped window: snapshot the current rect before unmap, then `mark_window_dirty_with_old_rect(current, empty)`.
- **DestroyWindow** of a mapped window: same as Unmap — snapshot before destroy.

Remove corresponding global dirty calls. Test + commit:

```bash
cargo +nightly fmt && cargo clippy && cargo test
git commit -am "feat(kms): narrow Map/Unmap/Destroy dirty propagation to old∪new"
```

- [ ] **Step 7: Migrate Reparent / Restack**

- **ReparentWindow**: pre = old absolute rect in screen coords, new = new absolute rect. Both via the helper.
- **Restack** (`ConfigureWindow` with `stack_mode`): the window doesn't move, but stacking under another window changes which pixels are visible. Use `mark_window_dirty_with_old_rect(current, current)` — the helper handles `old == new` by bumping every output intersecting that one rect.

Sites where the pre-change rect isn't trivially available (window already destroyed mid-handler, or some intermediate teardown path): fall back to `self.mark_all_outputs_dirty()` and leave a `// TODO(narrowing): compute pre-change rect` comment. Phase-1 correctness contract: "no fewer outputs are dirtied than before."

Test + commit:

```bash
cargo +nightly fmt && cargo clippy && cargo test
git commit -am "feat(kms): narrow Reparent/Restack dirty propagation to old∪new"
```

- [ ] **Step 8: Final test sweep**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Expected: clean. All four commits from this task (helper, ConfigureWindow, Map/Unmap/Destroy, Reparent/Restack) are now in.

---

## Task 9: Route composite through `OutputFrame` and `InFlight`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` — `composite_and_flip` (6176), `try_vulkan_composite_flip` (6274), and the pageflip-complete handler `drain_page_flips_and_composite` (6784).

This task wraps the existing composite work in the new types without changing what the GPU does and without adding any GPU sync. After this task: every **successful Vulkan composite submit** produces an `OutputFrame` pushed into `self.scheduler.in_flight`; skipped outputs and the no-Vulkan test path do not push. Every quiescent point polls the queue; fully-retired frames are drained.

**Phase-1 retirement is "two-stage shaped," not truly two-stage yet.** The GPU side uses a *placeholder* `vk::Fence::null()` — `null` is treated as "already GPU-retired" in `poll_in_flight`. This is correct in phase 1 because the existing `vkQueueWaitIdle` inside `try_vulkan_composite_flip` *has already drained the queue* by the time `record_submit` runs; there is no pending GPU work to wait for. The two-stage shape (separate GPU and scanout checks, both ANDed for retirement) is in place so phase 4 can swap `null` for a real signalled-by-composite-submit fence (or a timeline counter) without touching the queue plumbing.

Rationale for the `null` choice over a real `VkFence`: a real fence introduces ownership and destruction obligations (`vkDestroyFence` per frame, or pool reuse). In phase 1 those obligations have no payoff because the fence is immediately signalled. Phase 4 owns the fence/timeline lifecycle as a deliberate design choice tied to the same change that removes `vkQueueWaitIdle`.

- [ ] **Step 1: Open a paint batch at the start of `composite_and_flip`**

Just after the `any_dirty` check in `composite_and_flip`, open the batch:

```rust
        let frame_id = self.scheduler.open_batch();
```

- [ ] **Step 2: Change `try_vulkan_composite_flip` to return the BO slot it used**

`try_vulkan_composite_flip` today returns `bool`. Change it to return `Option<usize>` where `Some(bo_idx)` is the index inside the output's `ScanoutBoPool::bos` that the composite targeted, and `None` is the deferred-frame case. The existing code already computes this index at `crates/yserver/src/kms/backend.rs:6286` (`pool.bos.iter().position(...)`); propagate it out.

Update the caller in `composite_and_flip` to match the new return type. The two `log::debug!` arms become:

```rust
            let bo_slot = self.try_vulkan_composite_flip(layout_idx, visible);
            if bo_slot.is_none() {
                log::debug!(
                    "composite: deferring frame on output {} until a Free bo is available",
                    self.outputs[layout_idx].output.connector_name
                );
                continue;
            }
            // record_submit was added in task 7; keep it here.
            self.outputs[layout_idx].damage.record_submit();
            log::debug!(
                "composite: submitted flip on output {} (visible={}, submitted_gen={})",
                self.outputs[layout_idx].output.connector_name,
                visible.len(),
                self.outputs[layout_idx].damage.last_submitted_gen(),
            );
```

(The `record_submit()` call already exists from task 7 step 6 — leave it in place; only the surrounding bool-vs-Option logic changes.)

- [ ] **Step 3: Push an `InFlightFrame` after each successful submit**

After the `record_submit()` call above, push to `InFlight`. The composite fence is `vk::Fence::null()` in phase 1 (see the rationale at the top of this task):

```rust
            let submitted_gen = self.outputs[layout_idx].damage.last_submitted_gen();
            self.scheduler.in_flight.push(InFlightFrame {
                output_idx: layout_idx,
                frame_id,
                submitted_gen,
                composite_fence: ash::vk::Fence::null(),
                bo_slot,
                gpu_retired: false,
                scanout_retired: false,
            });
```

- [ ] **Step 4: Close the batch after the per-output loop**

After the `for layout_idx` loop:

```rust
        let _batch = self.scheduler.close_batch();
        // Phase 1: discard. Phase 2 submits its CB here.
```

- [ ] **Step 5: Add `poll_in_flight` and call it at quiescent points**

Add a `poll_in_flight` method on `KmsBackend`. Note the null-fence path: phase 1 treats `vk::Fence::null()` as "already GPU-retired" because `vkQueueWaitIdle` has already drained the queue by the time the frame was pushed. Phase 4 replaces this with a real fence/timeline poll.

```rust
    /// Non-blocking poll of the in-flight queue. Marks GPU- and
    /// scanout-retirement bits and drains fully-retired frames.
    /// Called at the top of `composite_and_flip` and from the
    /// pageflip-complete handler.
    ///
    /// Uses index-based access via `InFlight::get_mut` rather than
    /// `frames_mut()`: the loop body reads `self.vk` and
    /// `self.scanout_pools`, which can't coexist with a held
    /// `&mut self.scheduler.in_flight` borrow. The two-pass pattern
    /// (snapshot via get_mut, compute, write back via get_mut)
    /// avoids the borrow split.
    fn poll_in_flight(&mut self) {
        let n = self.scheduler.in_flight.len();
        for i in 0..n {
            // Pass 1: snapshot the polling inputs.
            let (composite_fence, output_idx, bo_slot, gpu_done, scanout_done) = {
                let f = self.scheduler.in_flight.get_mut(i).unwrap();
                (
                    f.composite_fence,
                    f.output_idx,
                    f.bo_slot,
                    f.gpu_retired,
                    f.scanout_retired,
                )
            };

            // Compute the new bools outside the borrow.
            //
            // GPU retirement. Phase 1: null fence is the "already
            // retired" sentinel (vkQueueWaitIdle inside the submit
            // path drained the queue). Phase 4 replaces the null
            // with a real signalled-by-submit fence or timeline
            // value, and this branch becomes a true non-blocking
            // status check.
            let new_gpu = if gpu_done {
                true
            } else if composite_fence == ash::vk::Fence::null() {
                true
            } else if let Some(vk) = self.vk.as_ref() {
                let status = unsafe { vk.device.get_fence_status(composite_fence) };
                matches!(status, Ok(true))
            } else {
                gpu_done
            };

            // Scanout retirement. The BoPhase machine in `vk/scanout.rs`
            // transitions the BO to Free on the pageflip-complete event.
            // A frame whose bo_slot is `None` (no-VK test path) is
            // trivially scanout-retired.
            let new_scanout = if scanout_done {
                true
            } else {
                self.scanout_pools
                    .get(output_idx)
                    .and_then(|p| p.as_ref())
                    .and_then(|p| bo_slot.and_then(|s| p.bos.get(s)))
                    .map(|b| matches!(b.state.phase, crate::kms::vk::scanout::BoPhase::Free))
                    .unwrap_or(true)
            };

            // Pass 2: write back.
            let f = self.scheduler.in_flight.get_mut(i).unwrap();
            f.gpu_retired = new_gpu;
            f.scanout_retired = new_scanout;
        }

        let drained = self.scheduler.in_flight.drain_retired();
        if drained > 0 {
            log::trace!("in_flight: drained {} fully-retired frame(s)", drained);
        }
    }
```

Call `self.poll_in_flight()`:
- at the top of `composite_and_flip` (before `any_output_needs_composite`).
- inside `drain_page_flips_and_composite` (line 6784), at the end of the `for c in flipped` loop and before the trailing `self.composite_and_flip()` call.

- [ ] **Step 6: Verify `record_present` already in place (from task 7)**

Confirm that task 7 step 7 added `self.outputs[output_idx].damage.record_present();` inside `drain_page_flips_and_composite`. If a merge or rebase dropped it, add it back. This call is what makes scanout retirement actually observable to `needs_composite()` — without it, the new in-flight queue would still drain on BoPhase transitions, but the damage state would stay flip_pending forever.

- [ ] **Step 7: Run the existing test suite**

```bash
cargo test
```

Expected: no regressions. The unit tests on the new types still pass; the backend's existing tests pass; the `composite_and_flip` skip logic still works (no Vulkan in `for_tests`, so the early return kicks in).

- [ ] **Step 8: Format, clippy, commit**

```bash
cargo +nightly fmt
cargo clippy
cargo test
git add crates/yserver/src/kms/backend.rs crates/yserver/src/kms/scheduler/
git commit -m "feat(kms): route composite through OutputFrame + InFlight queue"
```

---

## Task 10: Invariant assertions and logs

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`
- Modify: `crates/yserver/src/kms/scheduler/in_flight.rs` (debug_assert in `push`)

Add asserts that catch the failure modes phase 1 is supposed to make unreachable. None should fire in correct code; firing in tests or smoke is a bug.

- [ ] **Step 1: Add invariant checks**

In `crates/yserver/src/kms/scheduler/in_flight.rs`, modify `push`:

```rust
    pub fn push(&mut self, frame: InFlightFrame) {
        // Submission order is monotonic per output. A new frame
        // for the same output must have a higher frame_id than
        // the youngest in-flight frame for that output. Out-of-
        // order pushes would break drain ordering and resource
        // lifetime reasoning.
        debug_assert!(
            self.frames
                .iter()
                .rev()
                .find(|f| f.output_idx == frame.output_idx)
                .map_or(true, |prev| prev.frame_id < frame.frame_id),
            "InFlight::push: out-of-order frame_id for output {}",
            frame.output_idx,
        );
        self.frames.push_back(frame);
    }
```

In `composite_and_flip`, after `record_submit()`, log per-output state once per cycle when in debug:

```rust
        log::debug!(
            "composite cycle frame_id={} in_flight_len={}",
            frame_id,
            self.scheduler.in_flight.len()
        );
```

In `poll_in_flight`, log when GPU retirement is observed (helps verify that the fence-based path is actually firing — currently phase 1 trivially fires due to the `vkQueueWaitIdle` still being in place):

```rust
                if matches!(status, Ok(true)) && !frame.gpu_retired {
                    frame.gpu_retired = true;
                    log::trace!(
                        "in_flight: gpu_retired frame_id={} output_idx={}",
                        frame.frame_id,
                        frame.output_idx,
                    );
                }
```

In `composite_and_flip`'s per-output loop, after the existing skip-if-pending check, add:

```rust
            // Invariant: a skipped output keeps its dirty state.
            // The check below documents the contract.
            debug_assert!(
                self.outputs[layout_idx].damage.needs_composite() || vk_flip_pending || dumb_flip_pending,
                "output dirty state lost across skip on output {}",
                self.outputs[layout_idx].output.connector_name
            );
```

(Place this just before `if vk_flip_pending || dumb_flip_pending { continue; }`.)

- [ ] **Step 2: Run all tests**

```bash
cargo test
```

Expected: clean. The debug_asserts only fire on misuse.

- [ ] **Step 3: Format, clippy, commit**

```bash
cargo +nightly fmt
cargo clippy
cargo test
git add crates/yserver/src/kms/
git commit -m "feat(kms): invariant assertions and logs for in-flight + damage state"
```

---

## Task 11: Smoke validation

**Files:** (none — this task is observation and recording)

The unit tests cover the new types. End-to-end validation is XTS, rendercheck, and a couple of named real-world workloads. None of the symptom-level issues (GPU saturation, dual-output flicker) are expected to improve in phase 1 — that's phase 4. The validation here is "no regression."

- [ ] **Step 0: Pre-flight checks**

```bash
cargo +nightly fmt --check
cargo clippy
cargo test
```

Expected: all three exit 0.

Confirm the cutover is actually complete:

```bash
# Must return ZERO hits.
rg -n 'screen_dirty' crates/yserver/

# Must return at least one hit (phase 1 keeps hot-path waitIdle).
rg -n 'queue_wait_idle' crates/yserver/src/kms/vk/ops/mod.rs
```

If either command returns the wrong result, the cutover is incomplete — go back to T7 (for `screen_dirty`) or audit T9 (no accidental removal of `run_one_shot_op` drains).

- [ ] **Step 1: Run XTS against ynest and yserver**

```bash
just xts-ynest
just xts-yserver
```

Compare scores to the baseline in `docs/xts-baseline.md` / `docs/xts-baseline-summary.txt`. Phase 1 must not regress. Capture the result in a phase-1 status note.

- [ ] **Step 2: Run rendercheck**

```bash
just rendercheck-yserver
```

Same expectation: no regression.

- [ ] **Step 3: Smoke a WM session against yserver-hw**

Start `yserver-hw` and one of the supported WMs (marco, fvwm3, openbox). Open mate-control-center; hover over rows. Verify:

- The GPU saturation is still present (it's phase 4 that fixes this).
- The screen does not freeze.
- Dirty propagation works: moving a window between outputs leaves no stale pixels on the source output.

Capture any new misbehavior. If a window-move test fails the "no stale pixels on source" check, the most likely cause is a geometry-change call site that still uses `mark_dirty()` (whole-screen, fine) but whose pre-change rect was computed wrong — re-audit task 8's call-site migrations.

- [ ] **Step 4: Record the phase-1 baseline**

Append a section to `docs/status.md` (or a new `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase1-results.md`) with:

- XTS scenario pass/fail diff vs baseline.
- Rendercheck output diff.
- Smoke notes (which WM, observed behavior, any anomalies).
- Any debug logs that confirmed `in_flight` is being drained (`drained N fully-retired frame(s)` traces).

- [ ] **Step 5: Commit**

```bash
git add docs/
git commit -m "docs: phase-1 rendering re-architecture validation results"
```

---

## Done conditions

Phase 1 is complete when:

1. All 11 tasks above are committed and the tree is green
   (`cargo test`, `cargo clippy`, `cargo +nightly fmt --check`).
2. `screen_dirty: bool` no longer exists in the codebase.
3. The new scheduler types exist, are tested, and are wired into the backend.
4. Geometry-change paths use `mark_window_dirty_with_old_rect`.
5. Every **successful Vulkan composite submit** produces an `OutputFrame` pushed into `self.scheduler.in_flight`. Skipped outputs and the no-Vulkan `for_tests` path do not push.
6. The in-flight queue is polled non-blocking at every quiescent point; fully-retired frames (both GPU and scanout flags set) are drained.
7. XTS and rendercheck scores match baseline.
8. The `vkQueueWaitIdle` catalogue exists at `docs/superpowers/specs/2026-05-12-waitidle-catalogue.md` and every hit from `rg queue_wait_idle crates/yserver/` is classified.
9. Hot-path `vkQueueWaitIdle` calls in `vk/ops/mod.rs::run_one_shot_op` and friends are **still in place** — phase 1 deliberately stops short of removing them. Verify with `rg -n 'queue_wait_idle' crates/yserver/src/kms/vk/ops/mod.rs` — must still return a hit.

## What's next

The phase-2 plan is written *after* phase 1 lands, against the real shape of the code phase 1 produces. Don't pre-plan phase 2 from this document. The expected phase-2 scope is "frame-owned composite descriptor pools" — moving the single shared `CompositorPipeline.descriptor_pool` to a per-`OutputFrame` arena, with synchronous paint still in place. Phase 3 = recorder migration to `PaintBatch`. Phase 4 = wait-removal and semaphore handoff.
