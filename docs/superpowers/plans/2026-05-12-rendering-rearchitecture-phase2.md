# Rendering re-architecture — phase 2 implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move `CompositorPipeline.descriptor_pool` ownership from the shared compositor pipeline to a per-output ring of pools (N=2 fixed). Restructure `InFlightFrame` to embed `OutputFrame` (resource ownership vs retirement bookkeeping separation). After this phase, each in-flight composite owns its own descriptor pool — phase 4 can remove `vkQueueWaitIdle` without invalidating descriptor sets still being read by the GPU.

**Architecture:** New `CompositePoolRing` type in `kms/scheduler/` holds two `vk::DescriptorPool`s + in-use bits per output. `OutputFrame` (currently a shell) gains a real constructor with a `composite_pool_slot` field. `InFlightFrame { output_frame, gpu_retired, scanout_retired }` — composition rather than duplication. `try_vulkan_composite_flip` acquires a pool slot from the per-output ring, passes it through `compositor::record_and_present_composite`, and returns it for `InFlightFrame` construction. `poll_in_flight` releases the pool slot on retirement (via `vkResetDescriptorPool`).

**Tech Stack:** Rust 2021, `ash` for Vulkan, no new external dependencies. Existing `kms/scheduler/` module + `kms/vk/pipeline.rs` + `kms/vk/compositor.rs` + `kms/backend.rs`.

**Reference:**
- `docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md` — HLD; particularly the `OutputFrame` and `InFlight` subsections.
- `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase1-results.md` — phase 1 outcome; the deferred items.
- Architectural decision recorded in brainstorming: codex recommended Option C (embed OutputFrame in InFlightFrame); user picked per-output ring of N=2 pools.

---

## Pre-task: global checks

Every task ends with:

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Use explicit `git add` per file — **do NOT use `git commit -am`**. Phase 1 had one accidental sweep-in of `docs/status.md`; explicit staging prevents recurrence.

## File structure

New files:

- `crates/yserver/src/kms/scheduler/composite_pool_ring.rs` — `CompositePoolRing` with `acquire`/`release` semantics.

Modified files:

- `crates/yserver/src/kms/scheduler/mod.rs` — register new module.
- `crates/yserver/src/kms/scheduler/output_frame.rs` — add `composite_pool_slot` field, real constructor.
- `crates/yserver/src/kms/scheduler/in_flight.rs` — restructure `InFlightFrame` to embed `OutputFrame`; update tests.
- `crates/yserver/src/kms/backend.rs` — add `composite_pools: Option<CompositePoolRing>` to `OutputLayout`; rework `composite_and_flip` + `try_vulkan_composite_flip` + `poll_in_flight`.
- `crates/yserver/src/kms/vk/pipeline.rs` — remove `descriptor_pool` field, `reset_descriptors`, `allocate_descriptor_for_view`. Keep `MAX_DESCRIPTOR_SETS_PER_FRAME` as a public constant (CompositePoolRing uses it).
- `crates/yserver/src/kms/vk/compositor.rs` — `record_and_present_composite` takes the target `DescriptorPool` as a parameter; pool reset moves into the recorder; descriptor allocation goes through the passed pool.

---

## Task 1: `CompositePoolRing` type with unit tests

**Files:**
- Create: `crates/yserver/src/kms/scheduler/composite_pool_ring.rs`
- Modify: `crates/yserver/src/kms/scheduler/mod.rs`

- [ ] **Step 1: Write the failing tests**

Create `crates/yserver/src/kms/scheduler/composite_pool_ring.rs`. The slot-tracking state machine is extracted into a `SlotTracker` inner struct that's testable without `VkContext`:

```rust
//! Per-output ring of two descriptor pools for composite recording.
//!
//! Phase 2: replaces the single shared `CompositorPipeline.descriptor_pool`
//! that was reset at the start of every composite pass. With per-frame
//! ownership of a pool, multiple per-output composites can be in flight
//! simultaneously without one's reset invalidating another's sets.
//!
//! Sizing rationale: at most one flip in flight per output (existing
//! flip-pending skip), plus one frame being recorded. Ring of 2 is
//! sufficient; backpressure (acquire returning None) only fires if the
//! ring is exhausted, which the existing skip logic prevents.

use std::sync::Arc;

use ash::vk;

use crate::kms::vk::device::VkContext;

pub const RING_LEN: usize = 2;

/// Slot-tracking state. Extracted from `CompositePoolRing` so the
/// state machine can be unit-tested without a real `VkContext`.
#[derive(Debug, Default)]
struct SlotTracker {
    in_use: [bool; RING_LEN],
}

impl SlotTracker {
    fn acquire(&mut self) -> Option<usize> {
        for i in 0..RING_LEN {
            if !self.in_use[i] {
                self.in_use[i] = true;
                return Some(i);
            }
        }
        None
    }

    fn release(&mut self, slot: usize) {
        debug_assert!(slot < RING_LEN, "SlotTracker::release: slot out of range");
        debug_assert!(self.in_use[slot], "SlotTracker::release: slot not in use");
        self.in_use[slot] = false;
    }

    fn slots_in_use(&self) -> usize {
        self.in_use.iter().filter(|&&b| b).count()
    }
}

#[derive(Debug)]
pub struct CompositePoolRing {
    pools: [vk::DescriptorPool; RING_LEN],
    tracker: SlotTracker,
    vk: Arc<VkContext>,
}

impl CompositePoolRing {
    /// Create a new ring with `RING_LEN` descriptor pools, each
    /// sized for `max_sets_per_pool` COMBINED_IMAGE_SAMPLER sets.
    pub fn new(
        vk: Arc<VkContext>,
        max_sets_per_pool: u32,
    ) -> Result<Self, vk::Result> {
        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(max_sets_per_pool)];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(max_sets_per_pool)
            .pool_sizes(&pool_sizes);

        let mut pools = [vk::DescriptorPool::null(); RING_LEN];
        for i in 0..RING_LEN {
            pools[i] = match unsafe { vk.device.create_descriptor_pool(&pool_info, None) } {
                Ok(p) => p,
                Err(e) => {
                    // Roll back previously created pools.
                    for &p in &pools[..i] {
                        unsafe { vk.device.destroy_descriptor_pool(p, None) };
                    }
                    return Err(e);
                }
            };
        }

        Ok(Self {
            pools,
            tracker: SlotTracker::default(),
            vk,
        })
    }

    /// Borrow a free pool slot. Returns its index, or `None` if all
    /// `RING_LEN` slots are in use (backpressure). Caller is
    /// responsible for calling `release(slot)` when the slot's
    /// frame fully retires.
    pub fn acquire(&mut self) -> Option<usize> {
        self.tracker.acquire()
    }

    /// Return a pool slot to the ring. Resets the pool (invalidates
    /// any descriptor sets allocated from it) and marks the slot
    /// available for the next `acquire`.
    pub fn release(&mut self, slot: usize) {
        unsafe {
            let _ = self.vk.device.reset_descriptor_pool(
                self.pools[slot],
                vk::DescriptorPoolResetFlags::empty(),
            );
        }
        self.tracker.release(slot);
    }

    /// Return the raw `VkDescriptorPool` handle for `slot`. Caller
    /// uses this with `vkAllocateDescriptorSets` against the shared
    /// `descriptor_set_layout`.
    pub fn pool_at(&self, slot: usize) -> vk::DescriptorPool {
        self.pools[slot]
    }

    /// Number of slots currently in use. Test/diagnostic helper.
    pub fn slots_in_use(&self) -> usize {
        self.tracker.slots_in_use()
    }
}

impl Drop for CompositePoolRing {
    fn drop(&mut self) {
        // The frames that owned these pools are gone; destroy them.
        // Drop runs only when the parent OutputLayout is itself
        // being torn down (hotplug-remove or server shutdown).
        unsafe {
            let _ = self.vk.device.device_wait_idle();
            for &p in &self.pools {
                if p != vk::DescriptorPool::null() {
                    self.vk.device.destroy_descriptor_pool(p, None);
                }
            }
        }
    }
}

// CompositePoolRing handles VkDescriptorPool which is !Send/!Sync
// in ash's safe shim, but the single-threaded-core invariant
// (Phase 6.8) means the backend never crosses threads with this.
// Mark Send so KmsBackend stays Send for the existing trait.
unsafe impl Send for CompositePoolRing {}

#[cfg(test)]
mod tests {
    use super::*;

    // `SlotTracker` is exercised here; the full `CompositePoolRing`
    // needs a real `VkContext` (for both `new` and `Drop`) and is
    // validated by the hardware smoke in T8.

    #[test]
    fn fresh_tracker_has_no_slots_in_use() {
        let t = SlotTracker::default();
        assert_eq!(t.slots_in_use(), 0);
    }

    #[test]
    fn acquire_returns_monotonic_slots_until_full() {
        let mut t = SlotTracker::default();
        let a = t.acquire();
        let b = t.acquire();
        let c = t.acquire();
        assert_eq!(a, Some(0));
        assert_eq!(b, Some(1));
        assert_eq!(c, None, "third acquire on a RING_LEN=2 ring must return None");
        assert_eq!(t.slots_in_use(), 2);
    }

    #[test]
    fn release_frees_slot_for_reuse() {
        let mut t = SlotTracker::default();
        let _ = t.acquire();
        let _ = t.acquire();
        t.release(0);
        let reacquired = t.acquire();
        assert_eq!(reacquired, Some(0), "released slot must be re-acquirable");
    }

    #[test]
    #[should_panic(expected = "slot not in use")]
    fn release_of_already_free_slot_panics_in_debug() {
        let mut t = SlotTracker::default();
        t.release(0);
    }

    #[test]
    fn slots_in_use_reflects_acquire_and_release() {
        let mut t = SlotTracker::default();
        assert_eq!(t.slots_in_use(), 0);
        t.acquire();
        assert_eq!(t.slots_in_use(), 1);
        t.acquire();
        assert_eq!(t.slots_in_use(), 2);
        t.release(0);
        assert_eq!(t.slots_in_use(), 1);
        t.release(1);
        assert_eq!(t.slots_in_use(), 0);
    }
}
```

The `SlotTracker` extraction lets unit tests cover the load-bearing acquire/release/exhaustion semantics without any `VkContext` dependency. `CompositePoolRing` itself delegates to it for the state-machine half, leaving only the Vulkan pool allocation/reset/destruction concerns in the outer type — which the hardware smoke (T8) validates.

- [ ] **Step 2: Register the module**

Edit `crates/yserver/src/kms/scheduler/mod.rs`:

```rust
pub mod composite_pool_ring;
pub mod damage;
pub mod in_flight;
pub mod output_frame;
pub mod paint_batch;
```

(Alphabetical order; insert before `damage`.)

- [ ] **Step 3: Run tests**

```bash
cargo test -p yserver kms::scheduler::composite_pool_ring
```

Expected: 5 tests pass (`fresh_tracker_has_no_slots_in_use`, `acquire_returns_monotonic_slots_until_full`, `release_frees_slot_for_reuse`, `release_of_already_free_slot_panics_in_debug`, `slots_in_use_reflects_acquire_and_release`).

- [ ] **Step 4: Format, clippy, commit**

```bash
cargo +nightly fmt
cargo clippy
cargo test
git add crates/yserver/src/kms/scheduler/composite_pool_ring.rs \
        crates/yserver/src/kms/scheduler/mod.rs
git commit -m "feat(scheduler): add CompositePoolRing for per-output composite descriptor pools"
```

---

## Task 2: Restructure `InFlightFrame` to embed `OutputFrame`

**Files:**
- Modify: `crates/yserver/src/kms/scheduler/output_frame.rs`
- Modify: `crates/yserver/src/kms/scheduler/in_flight.rs`

This task changes the struct shape. The `mk_frame` test helper and all field accesses in tests change accordingly. After this task the rest of the codebase still uses the OLD `InFlightFrame` field paths (`frame.output_idx` etc) — those break and must be updated in T6. Between T2 and T6 the tree won't compile; that's the cost of the restructure.

Wait — that's a flag-day diff across files. We don't want a non-compiling intermediate. Better strategy: do T2 in a way where the existing callers still work. Approach: keep the existing `pub` fields on `InFlightFrame` as `Deref`-like accessors, or do T2 + T6 together as a single atomic switch.

The cleanest atomic switch: T2 only adds the new shape and migrates `in_flight.rs` itself (struct + tests). The compile-time errors at callers are localized to `backend.rs::composite_and_flip` (the push site) and `poll_in_flight` (the read sites). Step 4 of this task does those updates in `backend.rs` too — making T2 a slightly larger commit that covers all field-access call sites.

- [ ] **Step 1: Update `OutputFrame` with the phase-2 shape**

Replace `crates/yserver/src/kms/scheduler/output_frame.rs`:

```rust
//! One composited frame for one output.
//!
//! Phase 2 owns the renderable state: the BO slot the composite
//! targets, the descriptor pool slot allocated for it, and the
//! sync primitives (composite_fence). Phase 3 adds the per-frame
//! command buffer and scratch arena. `InFlightFrame` (in
//! `in_flight.rs`) wraps an `OutputFrame` with retirement
//! bookkeeping.

use ash::vk;

#[derive(Debug)]
pub struct OutputFrame {
    pub output_idx: usize,
    pub frame_id: u64,
    pub submitted_gen: u64,
    pub bo_slot: Option<usize>,
    /// Slot in the per-output `CompositePoolRing`. Released on
    /// retirement.
    pub composite_pool_slot: usize,
    /// Phase-1 sentinel `vk::Fence::null()`; phase 4 replaces with
    /// a real fence signalled by the composite submit.
    pub composite_fence: vk::Fence,
}

impl OutputFrame {
    pub fn new(
        output_idx: usize,
        frame_id: u64,
        submitted_gen: u64,
        bo_slot: Option<usize>,
        composite_pool_slot: usize,
        composite_fence: vk::Fence,
    ) -> Self {
        Self {
            output_idx,
            frame_id,
            submitted_gen,
            bo_slot,
            composite_pool_slot,
            composite_fence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_frame_records_all_fields() {
        let f = OutputFrame::new(0, 1, 7, Some(2), 1, vk::Fence::null());
        assert_eq!(f.output_idx, 0);
        assert_eq!(f.frame_id, 1);
        assert_eq!(f.submitted_gen, 7);
        assert_eq!(f.bo_slot, Some(2));
        assert_eq!(f.composite_pool_slot, 1);
    }
}
```

The phase-1 doc-comment "## Phase-2 follow-up" section is **removed** because the reconciliation it predicted is now happening in this commit.

- [ ] **Step 2: Restructure `InFlightFrame` to embed `OutputFrame`**

Edit `crates/yserver/src/kms/scheduler/in_flight.rs`. Replace the existing `InFlightFrame` struct, its `fully_retired` impl, the `mk_frame` test helper, and the test bodies that touch the old fields:

```rust
use crate::kms::scheduler::output_frame::OutputFrame;

/// A single in-flight `OutputFrame`'s retirement bookkeeping.
///
/// `output_frame` owns the renderable resources (BO slot, descriptor
/// pool slot, composite fence). `gpu_retired` / `scanout_retired`
/// track the two retirement observations.
///
/// The fields are public to the scheduler module so the polling
/// code (which lives in `KmsBackend` because it owns `VkContext`
/// and the BO pools) can read/write them directly.
#[derive(Debug)]
pub struct InFlightFrame {
    pub output_frame: OutputFrame,
    pub gpu_retired: bool,
    pub scanout_retired: bool,
}

impl InFlightFrame {
    pub fn fully_retired(&self) -> bool {
        self.gpu_retired && self.scanout_retired
    }
}
```

Remove the old `pub output_idx`, `pub frame_id`, `pub submitted_gen`, `pub composite_fence`, `pub bo_slot` fields — those are reached now via `frame.output_frame.<field>`.

Update the existing test helper `mk_frame` and tests in `in_flight.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use ash::vk;

    fn mk_frame(id: u64, output: usize) -> InFlightFrame {
        InFlightFrame {
            output_frame: OutputFrame::new(
                output,
                id,
                id,            // submitted_gen
                Some(0),       // bo_slot
                0,             // composite_pool_slot
                vk::Fence::null(),
            ),
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
        q.frames_mut().next().unwrap().gpu_retired = true;
        let drained = q.drain_retired();
        assert_eq!(drained, 0, "GPU-only retirement is not enough");
        q.frames_mut().next().unwrap().scanout_retired = true;
        let drained = q.drain_retired();
        assert_eq!(drained, 1);
        assert_eq!(q.len(), 1);
        assert_eq!(q.frames().next().unwrap().output_frame.frame_id, 2);
    }

    #[test]
    fn drain_retired_only_drains_prefix() {
        let mut q = InFlight::default();
        q.push(mk_frame(1, 0));
        q.push(mk_frame(2, 0));
        {
            let mut iter = q.frames_mut();
            let _f1 = iter.next();
            let f2 = iter.next().unwrap();
            f2.gpu_retired = true;
            f2.scanout_retired = true;
        }
        let drained = q.drain_retired();
        assert_eq!(
            drained, 0,
            "must not drain frame 2 while frame 1 is still in-flight"
        );
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn is_empty_reflects_push_and_drain() {
        let mut q = InFlight::default();
        assert!(q.is_empty());
        q.push(mk_frame(1, 0));
        assert!(!q.is_empty());
        let mut iter = q.frames_mut();
        let f = iter.next().unwrap();
        f.gpu_retired = true;
        f.scanout_retired = true;
        drop(iter);
        q.drain_retired();
        assert!(q.is_empty());
    }

    #[test]
    fn drain_retired_on_empty_queue_returns_zero() {
        let mut q = InFlight::default();
        assert_eq!(q.drain_retired(), 0);
    }

    #[test]
    fn drain_retired_blocks_on_scanout_only_retirement() {
        let mut q = InFlight::default();
        q.push(mk_frame(1, 0));
        let mut iter = q.frames_mut();
        let f = iter.next().unwrap();
        f.scanout_retired = true;
        drop(iter);
        assert_eq!(
            q.drain_retired(),
            0,
            "scanout-only retirement is not enough"
        );
    }

    #[test]
    fn get_mut_returns_frame_at_index() {
        let mut q = InFlight::default();
        q.push(mk_frame(1, 0));
        q.push(mk_frame(2, 1));
        assert_eq!(q.get_mut(0).unwrap().output_frame.frame_id, 1);
        assert_eq!(q.get_mut(1).unwrap().output_frame.frame_id, 2);
        assert!(q.get_mut(2).is_none());
    }
}
```

Update the `push` debug_assert to use the new field path:

```rust
    pub fn push(&mut self, frame: InFlightFrame) {
        debug_assert!(
            self.frames
                .iter()
                .rev()
                .find(|f| f.output_frame.output_idx == frame.output_frame.output_idx)
                .is_none_or(|prev| prev.output_frame.frame_id < frame.output_frame.frame_id),
            "InFlight::push: out-of-order frame_id for output {}",
            frame.output_frame.output_idx,
        );
        self.frames.push_back(frame);
    }
```

- [ ] **Step 3: Update `backend.rs` field accesses**

`composite_and_flip` currently constructs `InFlightFrame { output_idx, frame_id, submitted_gen, composite_fence, bo_slot, gpu_retired, scanout_retired }`. Change to:

```rust
            self.scheduler
                .in_flight
                .push(crate::kms::scheduler::in_flight::InFlightFrame {
                    output_frame: crate::kms::scheduler::output_frame::OutputFrame::new(
                        layout_idx,
                        frame_id,
                        submitted_gen,
                        bo_slot,
                        // composite_pool_slot: PLACEHOLDER until T5.
                        // Must not be read by any caller before T5 lands the
                        // real value from `try_vulkan_composite_flip`.
                        // T6 starts reading this field (release-on-retirement)
                        // and must not land before T5.
                        0,
                        ash::vk::Fence::null(),
                    ),
                    gpu_retired: false,
                    scanout_retired: false,
                });
```

The placeholder `0` is intentional and called out in a comment. T5 + T6 fix it.

`poll_in_flight` reads `frame.composite_fence`, `frame.output_idx`, `frame.bo_slot`, `frame.gpu_retired`, `frame.scanout_retired`. Update each reference to go through `frame.output_frame.<field>` for the OutputFrame-owned fields:

```rust
            let (composite_fence, output_idx, bo_slot, gpu_done, scanout_done) = {
                let f = self.scheduler.in_flight.get_mut(i).unwrap();
                (
                    f.output_frame.composite_fence,
                    f.output_frame.output_idx,
                    f.output_frame.bo_slot,
                    f.gpu_retired,
                    f.scanout_retired,
                )
            };
            // ...compute new_gpu / new_scanout outside the borrow...
            let f = self.scheduler.in_flight.get_mut(i).unwrap();
            let prev_gpu = f.gpu_retired;
            f.gpu_retired = new_gpu;
            f.scanout_retired = new_scanout;
            if !prev_gpu && new_gpu && composite_fence != ash::vk::Fence::null() {
                log::trace!(
                    "in_flight: gpu_retired (fence) frame_id={} output_idx={}",
                    f.output_frame.frame_id,
                    f.output_frame.output_idx,
                );
            }
```

(The `log::trace!` keeps its current shape; only the field paths change.)

- [ ] **Step 4: Run tests**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Expected: all tests still pass, both the in_flight unit tests with the new struct shape and the existing backend tests. The placeholder `composite_pool_slot: 0` doesn't break anything because nothing reads it yet (T6 will).

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/scheduler/output_frame.rs \
        crates/yserver/src/kms/scheduler/in_flight.rs \
        crates/yserver/src/kms/backend.rs
git commit -m "refactor(scheduler): embed OutputFrame in InFlightFrame; add composite_pool_slot"
```

---

## Task 3: Wire `composite_pools` field into `OutputLayout`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`

No behavior change yet — only adds the field with lazy-init `None`.

- [ ] **Step 1: Add `composite_pools` field to `OutputLayout`**

Find the `pub(crate) struct OutputLayout` (around line 594). Append the new field:

```rust
pub(crate) struct OutputLayout {
    pub output: crate::drm::modeset::Output,
    pub swapchain: crate::drm::Swapchain,
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    pub damage: crate::kms::scheduler::damage::OutputDamageState,
    /// Per-output ring of composite descriptor pools. Lazy-init on
    /// first composite for this output (requires `vk` + the
    /// compositor pipeline's `descriptor_set_layout`). `None` on
    /// the `for_tests` path (no Vulkan).
    pub composite_pools: Option<crate::kms::scheduler::composite_pool_ring::CompositePoolRing>,
}
```

Add `#[allow(dead_code)]` on the new field — clippy will warn that nothing reads it yet. T6 removes the allow.

- [ ] **Step 2: Initialize in every constructor**

Find every `OutputLayout { ... }` literal (`rg 'OutputLayout \{' crates/yserver/src/kms/`). Add `composite_pools: None,` to each. There are 2 sites (for_tests + production).

- [ ] **Step 3: Verify**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Expected: clean. The new field is unused (clippy allow keeps it quiet).

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): wire CompositePoolRing field into OutputLayout (lazy-init, no callers yet)"
```

---

## Task 4: Update `compositor::record_and_present_composite` to take a `DescriptorPool` parameter

**Files:**
- Modify: `crates/yserver/src/kms/vk/compositor.rs`
- Modify: `crates/yserver/src/kms/vk/pipeline.rs` (export `descriptor_set_layout` for the recorder to use; keep `allocate_descriptor_for_view` for now — T7 removes it)

The recorder currently calls `pipeline.reset_descriptors()` then `pipeline.allocate_descriptor_for_view()` per draw. Both methods work against the pipeline's owned pool. Change the recorder to take the target pool as a parameter; allocate sets directly via `vkAllocateDescriptorSets` against that pool.

The pool reset happens inside `CompositePoolRing::acquire` — when a slot was previously released, its `release()` did the reset. Acquire just hands back an empty pool. So the recorder doesn't reset; it just allocates.

- [ ] **Step 1: Modify the signature**

In `crates/yserver/src/kms/vk/compositor.rs`, change `record_and_present_composite`:

```rust
pub fn record_and_present_composite(
    vk: &VkContext,
    drm: &DrmDevice,
    output: &Output,
    bo: &mut ScanoutBo,
    pipeline: &CompositorPipeline,
    descriptor_pool: vk::DescriptorPool,
    scene: &CompositeScene,
) -> Result<(), PresentError> {
```

- [ ] **Step 2: Update the body**

Replace the descriptor-handling block (lines ~144-161 currently):

```rust
    // Reset is unnecessary — CompositePoolRing::release does the
    // reset when a slot is returned. Caller hands us an empty pool.

    let mut descriptors: Vec<vk::DescriptorSet> = Vec::with_capacity(scene.draws.len());
    for draw in &scene.draws {
        let layouts = [pipeline.descriptor_set_layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&layouts);
        let set = match unsafe { vk.device.allocate_descriptor_sets(&alloc_info) } {
            Ok(sets) => sets[0],
            Err(e) => {
                log::warn!(
                    "composite: descriptor allocation failed ({e:?}) at draw {} of {} — \
                     remaining draws skipped this frame",
                    descriptors.len(),
                    scene.draws.len()
                );
                break;
            }
        };
        let image_info = [vk::DescriptorImageInfo::default()
            .image_view(draw.image_view)
            .sampler(pipeline.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let writes = [vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&image_info)];
        unsafe { vk.device.update_descriptor_sets(&writes, &[]) };
        descriptors.push(set);
    }
```

(`pipeline.descriptor_set_layout` and `pipeline.sampler` were already `pub` — the recorder reads them directly. No changes to `CompositorPipeline` itself yet; T7 deletes `descriptor_pool` + `reset_descriptors` + `allocate_descriptor_for_view`.)

- [ ] **Step 3: Update the docstring**

The big sequence comment at the top of `record_and_present_composite` mentions "Reset descriptor pool". Update step 2 to:

```
//    2. Allocate one descriptor set per draw from the passed
//       `descriptor_pool`. The pool was reset by the ring's
//       `release()` when this slot was last returned; it's empty.
```

- [ ] **Step 4: Update callers (only one for now)**

`try_vulkan_composite_flip` is the only caller. Update its call to pass the pool. **At this task it does NOT yet acquire from the ring — that's T5. As a transitional bridge, it passes `pipeline.descriptor_pool` (the existing single pool that T7 will delete).** This keeps behavior identical to before T4 while the recorder signature change lands.

In `backend.rs::try_vulkan_composite_flip`, change the call:

```rust
        match compositor::record_and_present_composite(
            vkctx,
            &self.device,
            &self.outputs[layout_idx].output,
            bo,
            pipeline,
            pipeline.descriptor_pool, // T5 replaces with ring slot
            &scene,
        ) {
```

Add `pub descriptor_pool` visibility on `CompositorPipeline` if it isn't already (it's `pub(crate)` today via the struct declaration; verify) — the call needs to read the field externally. Inspect `pipeline.rs`; the field is declared `descriptor_pool: vk::DescriptorPool` (line 114), which defaults to module-private. Make it `pub` temporarily:

```rust
pub descriptor_pool: vk::DescriptorPool,  // T7 removes
```

The `reset_descriptors()` call inside `compositor::record_and_present_composite` is removed (step 2 above), so the single pool gets reset every time only by `pipeline.reset_descriptors()` calls from the test/production composite path — i.e., never inside the recorder anymore. But: with the recorder no longer resetting, the single pool fills up after MAX_DESCRIPTOR_SETS_PER_FRAME total allocations across all frames (instead of per-frame). To preserve pre-T4 behavior identically, add an explicit `pipeline.reset_descriptors()` call in `try_vulkan_composite_flip` *just before* the `record_and_present_composite` call:

```rust
        pipeline.reset_descriptors().ok(); // transitional shared-pool reset; T5 replaces
        match compositor::record_and_present_composite(...) { ... }
```

This preserves the **pre-existing** single-pool per-cycle reset semantic during the T4-only commit. It is **not** phase-4-safe (it keeps the shared pool that phase 2 is removing); T5 replaces the explicit reset and the field-access with the per-output ring acquire, and T7 deletes the field entirely.

- [ ] **Step 5: Verify**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Expected: clean. Behavior unchanged — single pool, reset once per composite cycle (now from the caller instead of inside the recorder).

- [ ] **Step 6: Commit**

```bash
git add crates/yserver/src/kms/vk/compositor.rs \
        crates/yserver/src/kms/vk/pipeline.rs \
        crates/yserver/src/kms/backend.rs
git commit -m "refactor(compositor): record_and_present_composite takes descriptor pool parameter"
```

---

## Task 5: `try_vulkan_composite_flip` acquires from the per-output ring

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`

Replace the transitional `pipeline.descriptor_pool` argument with an acquired ring slot. Add lazy-init for the ring. Change return type to carry the pool slot.

- [ ] **Step 1: Add lazy-init helper for the per-output ring**

Add a method to `KmsBackend`:

```rust
    /// Lazy-init the composite pool ring for this output. Returns
    /// `None` if Vulkan or the compositor pipeline isn't up
    /// (test path).
    fn ensure_composite_pools(
        &mut self,
        layout_idx: usize,
    ) -> Option<&mut crate::kms::scheduler::composite_pool_ring::CompositePoolRing> {
        if self.outputs[layout_idx].composite_pools.is_none() {
            let vk = self.vk.as_ref()?.clone();
            // descriptor_set_layout isn't actually used by CompositePoolRing
            // any more — it's only needed at allocate_descriptor_sets time
            // by the recorder. Pass the per-pool set count cap.
            match crate::kms::scheduler::composite_pool_ring::CompositePoolRing::new(
                vk,
                crate::kms::vk::pipeline::MAX_DESCRIPTOR_SETS_PER_FRAME,
            ) {
                Ok(ring) => {
                    self.outputs[layout_idx].composite_pools = Some(ring);
                }
                Err(e) => {
                    log::warn!(
                        "composite: failed to create descriptor pool ring for output {}: {e:?}",
                        self.outputs[layout_idx].output.connector_name
                    );
                    return None;
                }
            }
        }
        self.outputs[layout_idx].composite_pools.as_mut()
    }
```

- [ ] **Step 2: Change `try_vulkan_composite_flip` return type**

Was `Option<usize>` (returns bo_slot). Becomes `Option<(usize, usize)>` returning `(bo_slot, pool_slot)`.

**Two correctness pitfalls to avoid:**
1. **Borrow split.** Holding `let vkctx = self.vk.as_ref()?;` across the later `self.ensure_composite_pools(layout_idx)?` (which is `&mut self`) is a compile error. Clone the `Arc<VkContext>` early so the immutable `self.vk` borrow ends before the mutable backend work begins.
2. **Post-submit error safety.** `record_and_present_composite` can fail either pre-submit (layout/fb/record errors) or **post-submit** (`export_signaled_fd`, `submit_flip_with_fences`). On post-submit failure, the GPU may still be reading descriptor sets allocated from `pool_slot` — resetting the pool then would invalidate sets in active use, which is phase-4-unsafe. The error path is off the hot path (atomic-commit rejection is the typical trigger), so a non-hot-path `vkQueueWaitIdle` before pool release is the conservative correct fix.

```rust
    fn try_vulkan_composite_flip(
        &mut self,
        layout_idx: usize,
        visible: &[u32],
    ) -> Option<(usize, usize)> {
        use crate::kms::vk::{compositor, scanout::BoPhase};

        // Clone the Arc immediately so the immutable self.vk borrow
        // doesn't extend across the mutable ensure_composite_pools
        // call below.
        let vkctx = self.vk.as_ref()?.clone();
        self.compositor_pipeline.as_ref()?; // existence check; borrow drops here

        // Read-only check for a Free BO. Mutable re-borrow happens
        // later (for record_and_present_composite).
        let bo_idx = {
            let pool = self
                .scanout_pools
                .get(layout_idx)
                .and_then(|p| p.as_ref())?;
            pool.bos.iter().position(|b| b.state.phase == BoPhase::Free)
        };
        let Some(bo_idx) = bo_idx else {
            log::warn!(
                "vk composite: no Free bo in pool for output {} — deferring frame",
                self.outputs[layout_idx].output.connector_name
            );
            return None;
        };

        // Acquire a descriptor pool slot from this output's ring.
        let pool_slot = {
            let ring = self.ensure_composite_pools(layout_idx)?;
            ring.acquire()
        };
        let Some(pool_slot) = pool_slot else {
            log::warn!(
                "vk composite: descriptor pool ring exhausted for output {} — deferring frame",
                self.outputs[layout_idx].output.connector_name
            );
            return None;
        };

        let descriptor_pool = self.outputs[layout_idx]
            .composite_pools
            .as_ref()
            .expect("ensure_composite_pools just succeeded")
            .pool_at(pool_slot);

        let scene = self.build_composite_scene(layout_idx, visible);

        // Take the pipeline reference here; it's independent of
        // self.scanout_pools and self.outputs[idx].
        let pipeline = self.compositor_pipeline.as_ref()?;
        let pool_mut = self
            .scanout_pools
            .get_mut(layout_idx)
            .and_then(|p| p.as_mut())?;
        let bo = &mut pool_mut.bos[bo_idx];
        let result = compositor::record_and_present_composite(
            &vkctx,
            &self.device,
            &self.outputs[layout_idx].output,
            bo,
            pipeline,
            descriptor_pool,
            &scene,
        );

        match result {
            Ok(()) => Some((bo_idx, pool_slot)),
            Err(e) => {
                log::warn!(
                    "vk composite: record_and_present_composite failed on output {}: {e} \
                     — skipping frame",
                    self.outputs[layout_idx].output.connector_name
                );
                // Error-path pool release. `record_and_present_composite`
                // can fail BEFORE or AFTER `vkQueueSubmit2` (see
                // `vk/compositor.rs` — pre-submit: layout / fb / record
                // errors; post-submit: `export_signaled_fd` /
                // `submit_flip_with_fences`). Post-submit, the GPU may
                // still be reading descriptor sets allocated from
                // `pool_slot`. Resetting the pool then would invalidate
                // sets in active use — phase-4-unsafe.
                //
                // Conservative fix: drain the queue before releasing.
                // Atomic-commit rejection is the typical trigger and is
                // rare; a `queue_wait_idle` here is acceptable. The hot
                // path (Ok branch) is unchanged.
                unsafe {
                    let _ = vkctx.device.queue_wait_idle(vkctx.graphics_queue);
                }
                if let Some(ring) = self.outputs[layout_idx].composite_pools.as_mut() {
                    ring.release(pool_slot);
                }
                None
            }
        }
    }
```

Also delete the transitional `pipeline.reset_descriptors().ok();` line added in T4 (the recorder now takes its pool from the ring, which is already empty on acquire).

Also delete the transitional `pipeline.reset_descriptors().ok();` line added in T4 (the recorder now takes its pool from the ring, which is already empty on acquire).

- [ ] **Step 3: Update caller in `composite_and_flip`**

The caller was destructuring `Option<usize>` → now it's `Option<(usize, usize)>`. Find:

```rust
            let bo_slot = self.try_vulkan_composite_flip(layout_idx, visible);
            if bo_slot.is_none() {
                ...
                continue;
            }
            ...
            self.scheduler.in_flight.push(InFlightFrame {
                output_frame: OutputFrame::new(
                    layout_idx, frame_id, submitted_gen,
                    bo_slot, 0, vk::Fence::null(),  // placeholder pool_slot=0 from T2
                ),
                ...
            });
```

Change to:

```rust
            let Some((bo_idx, pool_slot)) = self.try_vulkan_composite_flip(layout_idx, visible) else {
                log::debug!(
                    "composite: deferring frame on output {} until a Free bo is available",
                    self.outputs[layout_idx].output.connector_name
                );
                continue;
            };
            self.outputs[layout_idx].damage.record_submit();
            log::debug!(
                "composite: submitted flip on output {} (visible={}, submitted_gen={})",
                self.outputs[layout_idx].output.connector_name,
                visible.len(),
                self.outputs[layout_idx].damage.last_submitted_gen(),
            );
            let submitted_gen = self.outputs[layout_idx].damage.last_submitted_gen();
            self.scheduler.in_flight.push(crate::kms::scheduler::in_flight::InFlightFrame {
                output_frame: crate::kms::scheduler::output_frame::OutputFrame::new(
                    layout_idx,
                    frame_id,
                    submitted_gen,
                    Some(bo_idx),
                    pool_slot,           // real pool slot now, not placeholder
                    ash::vk::Fence::null(),
                ),
                gpu_retired: false,
                scanout_retired: false,
            });
```

- [ ] **Step 4: Verify**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): try_vulkan_composite_flip acquires per-output composite pool slot"
```

---

## Task 6: `poll_in_flight` releases pool slot on retirement

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`

When a frame fully retires (`drain_retired` would pop it), call `release(pool_slot)` on its ring. Drain has to happen BEFORE the slot info is lost.

- [ ] **Step 1: Update `poll_in_flight` to release slots before drain**

In `poll_in_flight`, change the end-of-method block:

```rust
        // Release pool slots for frames that are about to be drained.
        // We must do this BEFORE drain_retired() because drain pops
        // the frames and we'd lose access to their pool_slot info.
        let to_release: Vec<(usize, usize)> = self
            .scheduler
            .in_flight
            .frames()
            .take_while(|f| f.fully_retired())
            .map(|f| (f.output_frame.output_idx, f.output_frame.composite_pool_slot))
            .collect();
        for (output_idx, pool_slot) in to_release {
            if let Some(ring) = self.outputs.get_mut(output_idx)
                .and_then(|o| o.composite_pools.as_mut())
            {
                ring.release(pool_slot);
            }
        }

        let drained = self.scheduler.in_flight.drain_retired();
        if drained > 0 {
            log::trace!("in_flight: drained {} fully-retired frame(s)", drained);
        }
```

The `take_while(|f| f.fully_retired())` iteration matches `drain_retired`'s FIFO-prefix semantic — we release the same prefix that's about to be popped.

- [ ] **Step 2: Remove the dead_code allow on `composite_pools`**

In `OutputLayout`, remove the `#[allow(dead_code)]` attribute on `composite_pools` — the field now has real callers (`ensure_composite_pools` and the release loop).

- [ ] **Step 3: Verify**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Expected: clean. clippy: no new warnings; the `#[allow(dead_code)]` removal is safe.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): poll_in_flight releases composite pool slot on retirement"
```

---

## Task 7: Remove `descriptor_pool` from `CompositorPipeline`

**Files:**
- Modify: `crates/yserver/src/kms/vk/pipeline.rs`
- Modify: `crates/yserver/src/kms/backend.rs` (no callers should remain; this catches strays)

The old `pipeline.descriptor_pool`, `pipeline.reset_descriptors()`, and `pipeline.allocate_descriptor_for_view()` are no longer called by the production composite path. Delete them.

- [ ] **Step 1: Audit for stray callers**

```bash
rg -n 'reset_descriptors|allocate_descriptor_for_view|descriptor_pool' \
   crates/yserver/src/kms/ | grep -v 'render_pipeline\|composite_pool_ring'
```

Expected: only hits should be inside `pipeline.rs` (the field declaration + the methods themselves + the constructor). If any other file still references these, fix it before continuing.

- [ ] **Step 2: Remove the field, methods, constructor lines**

In `crates/yserver/src/kms/vk/pipeline.rs`:

- Delete the `descriptor_pool: vk::DescriptorPool,` field (around line 114 — was `pub` in T4 transitional).
- Delete the constructor block that creates the descriptor pool (`pool_sizes`, `pool_info`, `create_descriptor_pool` call, lines ~226-247).
- Delete `pub fn reset_descriptors` (around line 275).
- Delete `pub fn allocate_descriptor_for_view` (around line 287).
- Update the `Drop` impl to no longer destroy the pool (was at line ~319).
- Keep `MAX_DESCRIPTOR_SETS_PER_FRAME` as a public constant (used by `ensure_composite_pools`).
- Keep `descriptor_set_layout` and `sampler` — they're still used by the recorder.

The `Self { ... descriptor_pool, ... }` construction at the end of `CompositorPipeline::new` also loses its `descriptor_pool` field.

The constructor's error-path goto cleanups (rolling back partial state on error) lose the descriptor-pool destruction call.

- [ ] **Step 3: Verify**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Expected: clean. `cargo clippy` may flag `MAX_DESCRIPTOR_SETS_PER_FRAME`'s doc comment if it still mentions "the descriptor pool"; update the doc to reflect the new per-output ring instead.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/vk/pipeline.rs crates/yserver/src/kms/backend.rs
git commit -m "refactor(kms): remove descriptor_pool from CompositorPipeline (moved to per-output ring)"
```

---

## Task 8: Smoke validation

**Files:** (none — observation)

End-to-end validation against XTS, rendercheck, and a real WM session.

- [ ] **Step 1: Pre-flight checks**

```bash
cargo +nightly fmt --check
cargo clippy
cargo test
```

Expected: all three exit 0.

Confirm the cutover is complete:

```bash
# Must return zero hits — descriptor_pool field is gone.
rg 'descriptor_pool' crates/yserver/src/kms/vk/pipeline.rs

# Must return at least one hit — CompositePoolRing still owns pools.
rg 'create_descriptor_pool' crates/yserver/src/kms/scheduler/composite_pool_ring.rs

# Hot-path waitIdle is still in place per phase-2 scope (phase 4 removes).
rg -n 'queue_wait_idle' crates/yserver/src/kms/vk/ops/mod.rs
```

- [ ] **Step 2: Hardware smoke**

Start `yserver` against real hardware (likely `just yserver-mate-hw-release` or similar). Verify:
- Desktop session comes up.
- Moving / resizing windows works.
- No descriptor-pool exhaustion warnings in the log (search for "descriptor pool ring exhausted" or "descriptor allocation failed").
- No regression vs phase 1's "slightly more responsive" baseline.

- [ ] **Step 3: Record results**

Append a section to `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase2-results.md`:

- Pre-flight check results.
- Cutover grep results.
- HW smoke notes.
- Any unexpected warnings in the logs.

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase2-results.md
git commit -m "docs: phase-2 rendering re-architecture validation results"
```

---

## Done conditions

Phase 2 is complete when:

1. All 8 tasks committed; tree green (`cargo test`, `cargo clippy`, `cargo +nightly fmt --check`).
2. `CompositorPipeline` no longer owns a `descriptor_pool` field.
3. `CompositePoolRing` exists at `crates/yserver/src/kms/scheduler/composite_pool_ring.rs` with `acquire`/`release` semantics, tested.
4. `OutputLayout` has a lazy-init `composite_pools: Option<CompositePoolRing>` field.
5. `InFlightFrame { output_frame: OutputFrame, gpu_retired, scanout_retired }` is the new shape — no duplicated fields.
6. `OutputFrame::new` is called with a real `composite_pool_slot` from the ring.
7. `try_vulkan_composite_flip` returns `Option<(bo_slot, pool_slot)>` and releases the slot on the error path.
8. `poll_in_flight` releases pool slots before drain.
9. Hardware smoke passes; no descriptor-pool warnings in the log.
10. Hot-path `vkQueueWaitIdle` in `vk/ops/mod.rs::run_one_shot_op` still present (phase 4 removes it).

## What's next

Phase 3 (recorder migration to `PaintBatch`) is the natural next step. The HLD names the family-by-family migration order (fill → copy → image → render → text → traps). Plan to be written after phase 2 lands.
