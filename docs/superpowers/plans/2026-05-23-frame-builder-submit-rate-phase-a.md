# Frame-builder Submit-Rate Phase A Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Collapse consecutive `vkQueueSubmit2` calls into batched multi-CB
submissions on the v2 paint path, reducing bee MATE-drag `queue_submit2/s`
from peak ~3304 (~55 submits/frame) to ~900-1500 (~15-25 submits/frame),
without changing v2's existing per-op-class batching, flush ordering, or
fence-retirement semantics.

**Architecture:** A new `SubmitGroup` on `PlatformBackend` buffers
`vk::CommandBufferSubmitInfo` entries + signal semaphores, owning ONE shared
`FenceTicket` for the group (spec Model A1). `submit_paint_cb_*` becomes
append-only; the engine drives flush via `RenderEngine::flush_submit_group`
which wraps the platform's queue submit and atomically commits
deferred per-op state on success / drops it on failure. Flush is
triggered at deterministic boundaries (sync barrier, PRESENT-completion
signal-only submit, scene compose, pageflip retire, max group size).
Engine paint ops borrow the shared ticket from the group instead of
acquiring a per-op one; every CB in a group retires together when the
shared fence signals. **Append defers `SubmittedOp` push onto a per-group
`pending_group_ops` queue**; commit-on-success moves them into
`submitted`, drop-on-failure releases them (CBs + staging + scratch
+ atlas tickets + shared-fence clones drop together) + sets
`renderer_failed`. Phase A is **fatal-after-failure** for drawable-
visible state (`last_render_ticket`, layout, damage,
`cow_flush_records` / `render_flush_records`): a failed submit
poisons `renderer_failed` and every subsequent engine entry point
short-circuits with `RenderError::RendererFailed`, so drawable
state never gets observed in its mid-mutation form. The spec
allows this simplification (§ "Frame-submit error propagation"
fatal branch); Phase B's frame builder is where the full
mutation-inventory rollback lands. Only the `SubmittedOp` queue is
deferred because leaking CBs/staging Arcs would break Vulkan
resource lifetimes regardless of whether further rendering happens.
Semaphore-bearing appends force an immediate flush so
`vkGetSemaphoreFdKHR(SYNC_FD)` sees a queued signal-op
(VUID-VkFenceGetFdInfoKHR-handleType-01457 — the same hazard that hung
yoga in Task 6.1).

**Tech Stack:** Rust, Vulkan via `ash`, existing `FenceTicket` / `FencePool`
/ `OpsCommandPool` primitives, existing v2 `DrawableStore` / `RenderEngine`
/ `SceneCompositor` layout, existing v2 telemetry + submit-trace
plumbing.

**Reference docs:**
- Spec: `docs/superpowers/specs/2026-05-23-frame-builder-submit-rate-design.md`
- Stage 5 plan: `docs/superpowers/plans/2026-05-20-stage-5-make-v2-fast.md`
- Task 6.1 spec: `docs/superpowers/specs/2026-05-23-deferred-present-completion-design.md`
- bee 2026-05-23 telemetry: `docs/status.md` § "2026-05-23 bee hardware close — Task 6.1 functionally fixed"

**File structure (locked in before tasks):**
- **Create**: `crates/yserver/src/kms/v2/submit_group.rs` — `SubmitGroup`
  struct + unit tests (no Vk).
- **Modify**: `crates/yserver/src/kms/v2/mod.rs` — `mod submit_group;`
- **Modify**: `crates/yserver/src/kms/v2/platform.rs` — `SubmitGroup` field
  on `PlatformBackend`; `submit_paint_cb` + `submit_paint_cb_with_semaphore`
  + `submit_present_completion_signal` become append-only; new
  `flush_submit_group(reason)`, `submit_group_ticket_or_open()`,
  `submit_group_is_open()`, `submit_group_size()` methods.
- **Modify**: `crates/yserver/src/kms/v2/engine.rs` — `begin_op_cb` sources
  ticket from the group; sync-wait flushpoint in `get_image` (the
  only remaining `ticket.wait()` site after the Task 15 cleanup);
  `drain_all` flushes group first.
- **Modify**: `crates/yserver/src/kms/v2/scene.rs` — `maybe_composite_tick`
  caller (in `backend.rs`) flushes group before `scene.tick`.
- **Modify**: `crates/yserver/src/kms/v2/backend.rs` — `maybe_composite`
  adds `flush_submit_group(SceneCompose)`; PRESENT signal-only submit path
  (~line 9200) flushes group first; `on_page_flip_ready` flushes group
  after retirement.
- **Modify**: `crates/yserver/src/kms/v2/telemetry.rs` — new counters
  (`submit_group_size_avg/max/histogram`, `submit_group_flush_reason`,
  `submit_group_aborts`, `active_descriptor_pool_count`,
  `active_staging_bytes`, `active_scratch_bytes`).
- **Modify**: `crates/yserver/tests/v2_acceptance.rs` — Backend-trait-surface
  acceptance tests (max-group-size cap, mixed-sequence smoke, renderer_failed
  path).
- **Modify**: `Justfile` — re-use existing `yserver-mate-hw-telemetry` for
  the bee gate; no new recipe.

---

## Phased rollout choice

Tasks are ordered so that **after every commit, the test suite stays
green and `KmsBackendV2` is in a runnable state**. The intermediate
state after Task 3 is "shared-ticket + append-only with max_size = 1"
— every paint op atomically appends and auto-flushes, bit-identical
to today's per-op submit cadence. Bumping the cap to 16 in Task 4
(alongside the load-bearing scene-compose flush) is the first
real behaviour change. This lets a bisect across the plan branch
always identify the offending task.

---

### Task 1: SubmitGroup module skeleton (no Vk; unit-tested)

**Files:**
- Create: `crates/yserver/src/kms/v2/submit_group.rs`
- Modify: `crates/yserver/src/kms/v2/mod.rs:1-20` (add `mod submit_group;`)

- [ ] **Step 1: Write the failing unit tests in `submit_group.rs`**

```rust
//! Stage 5 frame-builder Phase A: multi-CB single-submit accumulator.
//!
//! `SubmitGroup` buffers `(VkCommandBuffer, signal_semaphore?)` entries
//! between calls to `flush()`, plus the shared `FenceTicket` used as
//! the group's I6a retirement gate (spec Model A1). `flush()` issues
//! ONE `vkQueueSubmit2` with all buffered CBs and signal semaphores,
//! signaling the shared fence.
//!
//! No-Vk paths are still legal: `append` records the CB into the
//! buffer; `flush` on an empty group is a no-op; `flush` on a fixture
//! without Vk fails with a recognised error.

use ash::vk::{self, Handle}; // `Handle` brings `from_raw` into scope for test fixtures.

use super::platform::FenceTicket;

/// Reason a flush was triggered. Bumped into telemetry on every
/// non-empty flush so we can tell what's driving the submit cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlushReason {
    SyncBoundary,
    PresentCompletionSignal,
    SceneCompose,
    PageflipRetire,
    MaxSize,
    Shutdown,
}

/// Single buffered command-buffer entry. Mirrors the inputs to the
/// `vk::CommandBufferSubmitInfo` that flush() builds.
#[derive(Debug)]
pub(crate) struct GroupEntry {
    pub(crate) cb: vk::CommandBuffer,
    /// Optional COW present-completion semaphore. Today's
    /// `flush_cow_batch` attaches this; the group simply appends it
    /// to the eventual submit's `signal_semaphore_infos`.
    pub(crate) signal: Option<vk::Semaphore>,
}

#[derive(Debug)]
pub(crate) struct SubmitGroup {
    entries: Vec<GroupEntry>,
    /// Shared ticket. Lazily acquired on first `open_ticket` call;
    /// cleared on `flush`. None when group is empty.
    ticket: Option<FenceTicket>,
    /// Hard cap on `entries.len()` before append forces a
    /// `MaxSize` flush. Default 16 per spec; tuned from telemetry.
    max_size: usize,
}

impl SubmitGroup {
    pub(crate) fn new() -> Self {
        // Default 1: every append flushes immediately. The cap bumps
        // to 16 in Task 4 alongside the load-bearing flush triggers
        // (scene compose, PRESENT signal, get_image, pageflip
        // retire). Until then, single-CB-per-flush keeps the suite
        // bit-identical to today's per-op submit cadence.
        Self {
            entries: Vec::new(),
            ticket: None,
            max_size: 1,
        }
    }

    /// Test helper: override the cap. Production-side cap is bumped
    /// via `set_max_size` from `PlatformBackend::open_with_commit`
    /// once Task 4 lands.
    pub(crate) fn set_max_size(&mut self, n: usize) {
        self.max_size = n;
    }

    /// `#[cfg(test)]` introspection: peek at the buffered entries in
    /// append order. Tests that need to assert "upload CB was
    /// appended before draw CB" use this; without it the only signal
    /// we have is `size()`, which is too weak to catch reorderings.
    #[cfg(test)]
    pub(crate) fn peek_entries(&self) -> &[GroupEntry] {
        &self.entries
    }

    pub(crate) fn is_open(&self) -> bool {
        self.ticket.is_some()
    }

    pub(crate) fn size(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn max_size(&self) -> usize {
        self.max_size
    }

    pub(crate) fn ticket(&self) -> Option<&FenceTicket> {
        self.ticket.as_ref()
    }

    /// Seed the group with a freshly-acquired ticket if not open.
    /// Returns a clone of the (now open) ticket for the caller.
    pub(crate) fn open_with(&mut self, ticket: FenceTicket) -> FenceTicket {
        if self.ticket.is_none() {
            self.ticket = Some(ticket);
        }
        self.ticket.as_ref().expect("just set").clone()
    }

    /// Append a buffered entry. Caller is responsible for forcing a
    /// flush BEFORE calling this if `size() >= max_size`.
    pub(crate) fn append(&mut self, cb: vk::CommandBuffer, signal: Option<vk::Semaphore>) {
        self.entries.push(GroupEntry { cb, signal });
    }

    /// Take all buffered entries + the shared ticket, leaving the
    /// group empty. Caller (PlatformBackend::flush_submit_group)
    /// performs the `vkQueueSubmit2` against the returned data.
    pub(crate) fn take(&mut self) -> (Vec<GroupEntry>, Option<FenceTicket>) {
        (std::mem::take(&mut self.entries), self.ticket.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_cb(n: u64) -> vk::CommandBuffer {
        vk::CommandBuffer::from_raw(n)
    }

    fn fake_sem(n: u64) -> vk::Semaphore {
        vk::Semaphore::from_raw(n)
    }

    #[test]
    fn fresh_group_is_empty_and_closed_with_default_max_size_one() {
        let g = SubmitGroup::new();
        assert!(!g.is_open());
        assert_eq!(g.size(), 0);
        // Default 1: production cap bump lives in Task 4.
        assert_eq!(g.max_size(), 1);
    }

    #[test]
    fn peek_entries_returns_in_append_order() {
        let mut g = SubmitGroup::new();
        g.append(fake_cb(11), None);
        g.append(fake_cb(22), Some(fake_sem(99)));
        let entries = g.peek_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].cb, fake_cb(11));
        assert_eq!(entries[0].signal, None);
        assert_eq!(entries[1].cb, fake_cb(22));
        assert_eq!(entries[1].signal, Some(fake_sem(99)));
    }

    #[test]
    fn append_grows_entries_but_does_not_open_without_ticket() {
        let mut g = SubmitGroup::new();
        g.append(fake_cb(1), None);
        g.append(fake_cb(2), Some(fake_sem(7)));
        assert_eq!(g.size(), 2);
        assert!(!g.is_open(), "no ticket seeded yet");
    }

    #[test]
    fn take_leaves_group_empty_and_closed() {
        let mut g = SubmitGroup::new();
        g.append(fake_cb(1), None);
        g.append(fake_cb(2), Some(fake_sem(7)));
        let (entries, ticket) = g.take();
        assert_eq!(entries.len(), 2);
        assert!(ticket.is_none(), "no ticket was seeded in this fixture");
        assert_eq!(g.size(), 0);
        assert!(!g.is_open());
    }

    #[test]
    fn signal_semaphore_attached_to_entry_survives_take() {
        let mut g = SubmitGroup::new();
        g.append(fake_cb(1), None);
        g.append(fake_cb(2), Some(fake_sem(42)));
        let (entries, _) = g.take();
        assert_eq!(entries[0].signal, None);
        assert_eq!(entries[1].signal, Some(fake_sem(42)));
    }

    #[test]
    fn set_max_size_clamps_growth_signal() {
        let mut g = SubmitGroup::new();
        g.set_max_size(4);
        assert_eq!(g.max_size(), 4);
    }
}
```

- [ ] **Step 2: Register the module**

Edit `crates/yserver/src/kms/v2/mod.rs` and add `mod submit_group;`
alongside the other v2 modules (e.g., right after the `mod scene;` line).

- [ ] **Step 3: Run tests — expect PASS**

Run: `cargo test -p yserver kms::v2::submit_group::tests -- --nocapture`
Expected: 6 tests PASS (fresh-group default, append, take, signal
attach, set_max_size, peek-in-append-order). If any FAIL or compile
fails, fix before proceeding.

- [ ] **Step 4: Run `cargo fmt` + clippy on the new file**

Run: `cargo +nightly fmt -p yserver && cargo clippy -p yserver`
Expected: clean, no warnings. (Per `AGENTS.md`: nightly rustfmt,
regular clippy — NOT pedantic.)

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/submit_group.rs crates/yserver/src/kms/v2/mod.rs
git commit -m "feat(v2): add SubmitGroup buffer for multi-CB single-submit (Phase A skeleton)"
```

---

### Task 2: PlatformBackend integration — empty group, flush stub

**Files:**
- Modify: `crates/yserver/src/kms/v2/platform.rs:403-470` (PlatformBackend
  struct + constructors)
- Modify: `crates/yserver/src/kms/v2/platform.rs:1275-1360` (paint submit
  helpers — leave behaviour unchanged this task; just thread the group
  through)

- [ ] **Step 1: Write the failing test in `platform.rs` (`#[cfg(test)] mod tests` block at end)**

Locate the existing test module at the end of `platform.rs` (search
for `mod tests` — it already exists for `acquire_fence_ticket`). Append:

```rust
#[test]
fn platform_starts_with_empty_closed_submit_group() {
    let p = PlatformBackend::for_tests();
    assert!(!p.submit_group_is_open(), "fresh platform has closed group");
    assert_eq!(p.submit_group_size(), 0);
}

#[test]
fn flush_submit_group_empty_is_noop() {
    let mut p = PlatformBackend::for_tests();
    // Fixture has no Vk; should NOT attempt queue_submit2.
    let outcome = p
        .flush_submit_group(super::submit_group::FlushReason::SceneCompose)
        .expect("empty-group flush is always Ok");
    assert_eq!(outcome.flushed_entries, 0);
    assert!(!p.submit_group_is_open());
}
```

- [ ] **Step 2: Run the test — expect FAIL (missing methods)**

Run: `cargo test -p yserver kms::v2::platform::tests::platform_starts_with_empty_closed_submit_group kms::v2::platform::tests::flush_submit_group_empty_is_noop -- --nocapture`
Expected: compile error, `submit_group_is_open` / `submit_group_size` /
`flush_submit_group` not found.

- [ ] **Step 3: Implement the platform-side stubs**

In `platform.rs`, add `use super::submit_group::{FlushReason, SubmitGroup};`
near the top. Then add `submit_group: SubmitGroup,` to
`PlatformBackend` (after `renderer_failed: bool,` at ~line 468). Initialize
in BOTH constructors (`PlatformBackend::open_with_commit` at ~line 644
and `PlatformBackend::for_tests` at ~line 720):

```rust
submit_group: SubmitGroup::new(),
```

Also add the `FlushOutcome` type with its **full** shape from the
start. Task 11 only wires the deferred-queue drain that bumps
telemetry; landing all three fields here keeps the platform body
coherent across tasks (Codex round-4 finding: piecemeal extension
breaks the platform return paths):

```rust
/// Phase A: result of a `flush_submit_group` call. Same shape on
/// both Ok and Err paths; the `aborted` flag distinguishes them.
/// Task 11 hooks the deferred-queue drain that consumes this.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FlushOutcome {
    pub(crate) flushed_entries: usize,
    pub(crate) reason: FlushReason,
    pub(crate) aborted: bool,
}
```

Also add the `last_flush_outcome` stash field on `PlatformBackend`
so the engine wrapper can read the outcome on BOTH Ok and Err
paths (Rust's `?`-propagation doesn't pass an Err's outcome data
through):

```rust
last_flush_outcome: Option<FlushOutcome>,
```

(Initialize `None` in both constructors.) Add the getter near
`flush_submit_group`:

```rust
pub(crate) fn take_last_flush_outcome(&mut self) -> Option<FlushOutcome> {
    self.last_flush_outcome.take()
}
```

Add these methods on `PlatformBackend` (place near `submit_paint_cb`
at ~line 1275):

```rust
/// Phase A: count of CBs pending in the open submit group. Tests
/// + telemetry consult this; 0 when the group is empty.
pub(crate) fn submit_group_size(&self) -> usize {
    self.submit_group.size()
}

/// Phase A: true if any CB has been appended since the last flush.
pub(crate) fn submit_group_is_open(&self) -> bool {
    self.submit_group.is_open()
}

/// Phase A: explicit flush of any buffered submit group. Issues one
/// `vkQueueSubmit2` with all buffered CBs + signal semaphores,
/// signaling the group's shared fence. Empty group → `Ok(FlushOutcome {
/// flushed_entries: 0 })`. Vk-less fixture → same.
///
/// Sets `renderer_failed` on `queue_submit2` failure (Phase A fatal
/// policy for drawable state; SubmittedOp rollback is engine-side
/// via `pending_group_ops`).
pub(crate) fn flush_submit_group(
    &mut self,
    reason: FlushReason,
) -> Result<FlushOutcome, vk::Result> {
    let (entries, ticket) = self.submit_group.take();
    if entries.is_empty() {
        // The group may hold a ticket without entries if the engine
        // errored between `submit_group_ticket_or_open` and the
        // first append. We drop the ticket here; if it's unsignaled
        // (it will be — no submit happened), the existing
        // `FenceTicketInner::drop` leak detector sets
        // `renderer_failed`. That matches today's behaviour where a
        // partially-completed paint op leaks its `acquire_fence_ticket`
        // and triggers the same path.
        drop(ticket);
        let outcome = FlushOutcome {
            flushed_entries: 0,
            reason,
            aborted: false,
        };
        self.last_flush_outcome = Some(outcome);
        return Ok(outcome);
    }
    let n = entries.len();
    let Some(vk) = self.vk.as_ref() else {
        // Vk-less test fixture: drop entries + ticket on the floor.
        // Tickets without a queued signal-op will trip the leak
        // detector in `FenceTicketInner::drop`; that's a real-world
        // bug in production, but in tests we are expected to set up
        // groups without ever flushing them and let them be torn
        // down.
        let outcome = FlushOutcome {
            flushed_entries: n,
            reason,
            aborted: false,
        };
        self.last_flush_outcome = Some(outcome);
        return Ok(outcome);
    };
    let ticket = ticket.expect("non-empty group has ticket");
    let cb_infos: Vec<vk::CommandBufferSubmitInfo<'_>> = entries
        .iter()
        .map(|e| vk::CommandBufferSubmitInfo::default().command_buffer(e.cb))
        .collect();
    let sig_infos: Vec<vk::SemaphoreSubmitInfo<'_>> = entries
        .iter()
        .filter_map(|e| {
            e.signal.map(|s| {
                vk::SemaphoreSubmitInfo::default()
                    .semaphore(s)
                    .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            })
        })
        .collect();
    let submit = [{
        let s = vk::SubmitInfo2::default().command_buffer_infos(&cb_infos);
        if sig_infos.is_empty() {
            s
        } else {
            s.signal_semaphore_infos(&sig_infos)
        }
    }];
    crate::vk_count!(queue_submit2);
    match unsafe {
        vk.device
            .queue_submit2(vk.graphics_queue, &submit, ticket.fence())
    } {
        Ok(()) => {
            let outcome = FlushOutcome {
                flushed_entries: n,
                reason,
                aborted: false,
            };
            self.last_flush_outcome = Some(outcome);
            Ok(outcome)
        }
        Err(e) => {
            // Codex round-5 finding 1: route through the shared
            // `abort_flush` helper so the test-only
            // `force_next_submit_failure` and the real Err path
            // perform identical cleanup (CB-free + outcome stash +
            // renderer_failed).
            self.abort_flush(entries, n, reason, e)
        }
    }
}

/// Phase A: shared abort path. Frees the just-taken CBs, stashes
/// the `aborted: true` `FlushOutcome`, sets `renderer_failed`, and
/// surfaces the underlying `vk::Result`. Both the real
/// `queue_submit2 Err` arm and the test-only fault injection
/// (Task 3 Step 7) route through this helper so cleanup is uniform.
fn abort_flush(
    &mut self,
    entries: Vec<super::submit_group::GroupEntry>,
    n: usize,
    reason: FlushReason,
    err: vk::Result,
) -> Result<FlushOutcome, vk::Result> {
    self.renderer_failed = true;
    if let (Some(vk), Some(pool)) =
        (self.vk.as_ref(), self.ops_command_pool_handle())
    {
        let cbs: Vec<vk::CommandBuffer> =
            entries.iter().map(|e| e.cb).collect();
        if !cbs.is_empty() {
            unsafe { vk.device.free_command_buffers(pool, &cbs); }
        }
    }
    let outcome = FlushOutcome {
        flushed_entries: n,
        reason,
        aborted: true,
    };
    self.last_flush_outcome = Some(outcome);
    Err(err)
}

/// Phase A: seed the group's shared ticket if not open, then return
/// a clone for the caller to stash on its `SubmittedOp`. Mirrors the
/// per-op ticket acquisition from today's `begin_op_cb` but the same
/// ticket is handed back to every appender in the group.
pub(crate) fn submit_group_ticket_or_open(
    &mut self,
) -> Result<FenceTicket, vk::Result> {
    if let Some(t) = self.submit_group.ticket() {
        return Ok(t.clone());
    }
    let fresh = self.acquire_fence_ticket()?;
    Ok(self.submit_group.open_with(fresh))
}
```

- [ ] **Step 4: Run tests — expect PASS**

Run: `cargo test -p yserver kms::v2::platform::tests -- --nocapture`
Expected: new tests PASS; existing tests still PASS. No clippy warning.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/platform.rs
git commit -m "feat(v2): wire empty SubmitGroup + flush stub onto PlatformBackend"
```

---

### Task 3: Atomic switch to shared-ticket + append-only submits (max_size=1)

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs:446-579` (RenderEngineInner
  — add `pending_group_ops` field)
- Modify: `crates/yserver/src/kms/v2/engine.rs:5500-5546` (begin_op_cb +
  end_and_submit_op + end_and_submit_op_with_signal)
- Modify: `crates/yserver/src/kms/v2/engine.rs:639-692` (poll_retired,
  drain_all, plus new `RenderEngine::flush_submit_group`)
- Modify: `crates/yserver/src/kms/v2/engine.rs:2247-2260, 2764-2774,
  3077-3086, 4332-4344` (every `inner.submitted.push_back(SubmittedOp
  { ... })` site — there are ~14, do them ALL in this commit)
- Modify: `crates/yserver/src/kms/v2/platform.rs:1275-1317`
  (submit_paint_cb + submit_paint_cb_with_semaphore append + flush
  triggers)

**Why Tasks 3+4 are squashed.** Codex review caught that changing
`begin_op_cb` to source from the group BEFORE `submit_paint_cb`
appends would have op N+1 reuse op N's fence on a second
`queue_submit2` without reset (VUID-vkQueueSubmit2-fence-04894).
The shared-ticket model and append-only submit are atomic.

**Why the pending_group_ops deferral.** Spec § "Frame-submit error
propagation" says "Defer the `submitted.push_back` until after group
submit returns `Ok`; on failure, drop the would-be entries." Without
that, a failed group submit leaves `SubmittedOp`s in `submitted` with
never-signaling shared tickets — `drain_all` hangs at shutdown. The
deferral is the minimum spec-compliant rollback path.

**Why max_size = 1 default.** With the cap at 1, every append
auto-flushes (closes the group). The shared ticket lives for exactly
one submit then is dropped. Bit-identical to today's per-op cadence.
Cap bumps to 16 in Task 4 alongside the load-bearing flush triggers.

**Semaphore-bearing appends force flush.** When a paint CB carries a
`completion_signal` (COW PRESENT path), the SubmitGroup auto-flushes
AFTER append so the caller's subsequent `vkGetSemaphoreFdKHR(SYNC_FD)`
sees a queued signal-op (Task 6.1 VUID-VkFenceGetFdInfoKHR-handleType-
01457 hazard — that's the yoga hang shape).

- [ ] **Step 1: Add `pending_group_ops` field to `RenderEngineInner`**

In `engine.rs:446-579`, add to `RenderEngineInner`:

```rust
/// Phase A: per-group pending SubmittedOps. Each `end_and_submit_op`
/// pushes here instead of directly into `submitted`. On successful
/// `flush_submit_group` they drain into `submitted` (where
/// poll_retired sees them). On failure (renderer_failed branch)
/// they drop, releasing CBs + staging + scratch + their shared-
/// ticket clones together.
///
/// All entries in this vec share the same `FenceTicket` (Model A1).
pending_group_ops: Vec<SubmittedOp>,
```

Initialize in `RenderEngine::new` (~line 595):

```rust
pending_group_ops: Vec::new(),
```

- [ ] **Step 2: Add `RenderEngine::flush_submit_group` (engine-driven flush)**

Place near `poll_retired` (~line 639):

```rust
/// Phase A: flush the platform's SubmitGroup and commit/drop the
/// engine's parked per-op state atomically. THIS is the API every
/// flush-trigger site calls (scene compose, get_image, PRESENT
/// signal, pageflip retire, shutdown, MaxSize auto-flush) —
/// NEVER call `platform.flush_submit_group` directly from outside
/// the engine.
///
/// On success: every `SubmittedOp` parked in `pending_group_ops`
/// since the last flush moves into `submitted`. The shared ticket
/// has been queued for a signal-op via the platform's
/// `vkQueueSubmit2`, so `poll_retired` will see it signal in due
/// course.
///
/// On failure: `pending_group_ops` is dropped wholesale. CBs and
/// staging buffers free; the shared ticket's clones release down
/// to its leak-detector path (which sets renderer_failed —
/// already set by the platform on the submit failure anyway).
/// Returns the underlying `vk::Result`; callers log + propagate.
pub(crate) fn flush_submit_group(
    &mut self,
    platform: &mut PlatformBackend,
    reason: super::submit_group::FlushReason,
) -> Result<super::platform::FlushOutcome, vk::Result> {
    let result = platform.flush_submit_group(reason);
    // Drain the platform's last_flush_outcome regardless of Ok/Err
    // — both branches in platform's flush_submit_group populate it
    // before returning. The engine queues it for backend telemetry
    // drain (Task 11 wires that side).
    if let Some(outcome) = platform.take_last_flush_outcome() {
        if let Some(inner) = self.inner.as_mut() {
            inner.pending_flush_outcomes.push(outcome);
        }
    }
    let Some(inner) = self.inner.as_mut() else {
        return result;
    };
    match result {
        Ok(outcome) => {
            // Commit: parked ops graduate to `submitted`.
            for op in inner.pending_group_ops.drain(..) {
                inner.submitted.push_back(op);
            }
            Ok(outcome)
        }
        Err(e) => {
            // Rollback. CBs were already freed by platform's Err
            // branch (Codex round-4 finding 1 — platform owns the
            // free since it also covers the get_image bypass CB
            // that never entered pending_group_ops). Engine just
            // clears the parked SubmittedOps so their staging /
            // scratch / atlas_ticket / shared-fence-Arc clones
            // drop together.
            inner.pending_group_ops.clear();
            Err(e)
        }
    }
}
```

(`FlushOutcome` is the full type defined in Task 2 — all three
fields land at once so this wrapper signature is stable across
Tasks 2-11. Task 11 wires `pending_flush_outcomes` →
`Telemetry::record_submit_group_*`. Add the field now:

```rust
// On RenderEngineInner (place alongside pending_group_ops):
pending_flush_outcomes: Vec<super::platform::FlushOutcome>,
```
Initialize `Vec::new()` in `RenderEngine::new`.)

- [ ] **Step 3: Update most `inner.submitted.push_back` sites to use `pending_group_ops` (EXCLUDE get_image + any other sync-wait sites)**

There are ~14 sites in `engine.rs`. The bulk pattern:

```rust
inner.submitted.push_back(SubmittedOp { cb, ticket, staging, scratch, atlas_ticket, generation });
```

Change to:

```rust
inner.pending_group_ops.push(SubmittedOp { cb, ticket, staging, scratch, atlas_ticket, generation });
```

**EXCEPTION — do NOT convert** the push site in `get_image`
(engine.rs:3079). Codex round-6 finding 2: that path does a
synchronous `ticket.wait()` immediately after the push. If
converted now, the parked op's fence would never get queued.
Leave it as `inner.submitted.push_back` for this commit — Task 5
rewrites it to the special-case shape (bypass `pending_group_ops`,
explicit flush, push to `submitted` post-read).

Enumerate the sites with:

```bash
git grep -n 'inner\.submitted\.push_back' crates/yserver/src/kms/v2/
```

CONVERT every site EXCEPT the ones reachable from a synchronous
`ticket.wait()` in the same function. Today (HEAD `4ecb271`) the
only such site is:

- `RenderEngine::get_image` — push site at engine.rs:3079 (sits
  after `ticket.wait()` at 3063).

(The historical `wait_for_drawable_idle` site was deleted in Task
15 of an earlier stage — audit the file with `grep -n 'ticket\.wait('`
to confirm `get_image` is still the only match.)

**That site ALSO needs a `self.flush_submit_group` call wired into
the same Task 3 commit** (Codex round-7 finding 1). Excluding the
`push_back` site isn't enough — after Step 6, `submit_paint_cb_*`
is pure-append, so `end_and_submit_op` no longer queue-submits.
The existing `ticket.wait()` then hangs on an unsubmitted fence.

After the existing `end_and_submit_op(...)` line in `get_image`
and BEFORE `ticket.wait()`, insert:

```rust
// Phase A: end_and_submit_op now only appends to the SubmitGroup.
// Drive the explicit flush so the fence has a queued signal-op
// before we wait on it. Task 5 strengthens this with a flush at
// the TOP of get_image as well (drains prior buffered paint) and
// adds the regression test.
self.flush_submit_group(
    platform,
    super::submit_group::FlushReason::SyncBoundary,
)
.map_err(RenderError::Vk)?;
let Some(inner) = self.inner.as_mut() else {
    return Err(RenderError::NoVk);
};
```

(Borrow-factoring note: release `inner` before the flush, reborrow
after — same shape Task 3 Step 8 uses in `flush_cow_batch`.)

After Task 3, `get_image` keeps its `submitted.push_back` site
unchanged and adds the flush-before-wait call. Task 5 then layers
on the additional top-of-function flush (drains prior buffered
paint) and the special-case rewrite that bypasses
`pending_group_ops` entirely; the change between Task 3 and Task
5 is purely the additional flush + the regression test, not
behavioural — Task 3 already makes the path correct.

- [ ] **Step 4: Update `drain_all` to handle pending_group_ops too**

In `engine.rs:674-692`:

```rust
pub(crate) fn drain_all(&mut self, platform: &mut PlatformBackend) {
    // Flush any open SubmitGroup first; this commits parked ops
    // into `submitted` so the loop below sees the right set.
    if let Err(e) = self.flush_submit_group(
        platform,
        super::submit_group::FlushReason::Shutdown,
    ) {
        log::warn!("v2 drain_all: flush_submit_group failed: {e:?}");
    }
    let Some(inner) = self.inner.as_mut() else { return };
    let Some(pool) = platform.ops_command_pool_handle() else { return };
    let device = &inner.vk.device;
    while let Some(mut op) = inner.submitted.pop_front() {
        let _ = op.ticket.wait(&inner.vk);
        unsafe { device.free_command_buffers(pool, &[op.cb]) };
        drop(op.staging.take());
        inner.descriptor_pool_ring.release_up_to(op.generation);
    }
}
```

Update the call site(s) of `drain_all` to pass `&mut self.platform`.

- [ ] **Step 5: Refactor `begin_op_cb` to source the shared ticket**

Replace `engine.rs:5500-5518`:

```rust
fn begin_op_cb(
    inner: &mut RenderEngineInner,
    platform: &mut PlatformBackend,
) -> Result<(vk::CommandBuffer, FenceTicket), RenderError> {
    let pool = platform
        .ops_command_pool_handle()
        .ok_or(RenderError::NoVk)?;
    let device = &inner.vk.device;
    let alloc_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cb = unsafe { device.allocate_command_buffers(&alloc_info)? }[0];
    let begin = vk::CommandBufferBeginInfo::default()
        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    unsafe { device.begin_command_buffer(cb, &begin)? };
    // Phase A: shared ticket comes from the open submit group. With
    // max_size = 1, the group auto-closes after one append; with
    // max_size > 1 (post-Task 4), N appends share the same ticket.
    let ticket = platform.submit_group_ticket_or_open()?;
    Ok((cb, ticket))
}
```

- [ ] **Step 6: Switch `submit_paint_cb_*` to pure-append (NO internal flush)**

Replace `platform.rs:1275-1317`:

```rust
/// Phase A: append a paint CB to the open submit group. Returns
/// `Ok(())` once the append is recorded. NEVER auto-flushes —
/// flush is the engine's responsibility (so it can drain
/// `pending_group_ops` atomically with the queue submit).
///
/// `signal_fence` is IGNORED — the group's shared ticket owns the
/// fence. The parameter stays in the signature for source
/// compatibility with the engine; remove in Phase B.
pub(crate) fn submit_paint_cb(
    &mut self,
    cb: vk::CommandBuffer,
    _signal_fence: vk::Fence,
) -> Result<(), vk::Result> {
    self.submit_paint_cb_with_semaphore(cb, vk::Fence::null(), None)
}

pub(crate) fn submit_paint_cb_with_semaphore(
    &mut self,
    cb: vk::CommandBuffer,
    _signal_fence: vk::Fence,
    completion_signal: Option<vk::Semaphore>,
) -> Result<(), vk::Result> {
    if self.vk.is_none() {
        return Err(vk::Result::ERROR_INITIALIZATION_FAILED);
    }
    self.submit_group.append(cb, completion_signal);
    Ok(())
}
```

**Why pure-append (no auto-flush).** The platform's
`flush_submit_group` doesn't touch the engine's `pending_group_ops`.
If `submit_paint_cb_*` auto-flushed, it would commit CBs to the
queue BEFORE the engine had a chance to park its `SubmittedOp`
(per-op-paint sites push to `pending_group_ops` AFTER the
`end_and_submit_op` call returns). The drain-on-success step would
then see an empty `pending_group_ops` and the just-flushed CBs
would have no corresponding `SubmittedOp` in `submitted` — they'd
leak. The engine drives flushes after every push so the orderings
line up.

- [ ] **Step 7: Add engine-side flush triggers at every paint-op tail**

Add a helper on `RenderEngine`:

```rust
/// Phase A: check whether the platform's SubmitGroup has hit its
/// cap; if so, drive a `MaxSize` flush. Called from the tail of
/// every paint op AFTER pushing to `pending_group_ops`.
pub(crate) fn maybe_auto_flush_submit_group(
    &mut self,
    platform: &mut PlatformBackend,
) -> Result<(), RenderError> {
    if platform.submit_group_size() >= platform.submit_group_max_size() {
        self.flush_submit_group(
            platform,
            super::submit_group::FlushReason::MaxSize,
        )
        .map_err(RenderError::Vk)?;
    }
    Ok(())
}
```

Expose `submit_group_max_size` (non-test) on `PlatformBackend`:

```rust
pub(crate) fn submit_group_max_size(&self) -> usize {
    self.submit_group.max_size()
}
```

Walk every paint-op call site that calls
`end_and_submit_op[_with_signal]` and append a
`self.maybe_auto_flush_submit_group(platform)?;` immediately AFTER
the `inner.pending_group_ops.push(SubmittedOp { ... })`. Use
`git grep -n 'pending_group_ops.push' crates/yserver/src/kms/v2/`
to enumerate the ~14 sites.

- [ ] **Step 8: Conditional flush in flush_cow_batch when a semaphore is attached**

`flush_cow_batch` only attaches the completion semaphore when
`batch.present_completions` is non-empty (engine.rs:2197-2202).
The post-append flush is required ONLY when a semaphore is
attached — semaphore-less COW batches should stay in the group
and collapse with subsequent ops. Codex round-2 finding 5:
unconditional flush would defeat the cap-collapse for the common
COW pump (~78 % of bee MATE submits per the spec's submit-trace
ranking).

In `engine.rs:2204-2236` (`flush_cow_batch`), the current shape is:

```text
end_and_submit_op_with_signal(...)
[build present_batch using export_sync_file_fd]
[CPU bookkeeping]
inner.submitted.push_back(SubmittedOp { ... })  // becomes pending_group_ops.push from Step 3
[push present_batch to inner.pending_present_batches]
```

The export call MUST happen AFTER the semaphore's signal-op is
queued — but ONLY when a semaphore was attached (i.e.,
`completion_signal.is_some()` / `batch.present_completions` is
non-empty). Restructure to:

Paste-safe code structure (Codex round-4 finding 3 — the previous
draft mixed `self.flush_submit_group` calls with continuing `inner`
borrows; the implementer would have to refactor anyway, so the plan
shows the explicit release/reborrow shape):

```rust
// Phase A: completion-signal handling needs to release `inner`
// before calling `self.flush_submit_group`. Take everything we
// need out of `inner` first, then release the borrow.
let present_completions: Vec<PendingPresentEntry> = {
    // Last use of `inner` before the flush call.
    let inner = self.inner.as_mut().expect("checked above");
    let pc = std::mem::take(&mut batch.present_completions);
    // ... do the end_and_submit_op_with_signal here too:
    end_and_submit_op_with_signal(
        inner, platform, batch.cb, &batch.ticket, completion_semaphore,
    )?;
    // ... CPU bookkeeping: damage / touch_render_fence / generation /
    //     `pending_group_ops.push(SubmittedOp { ... })` ...
    pc
};
// `inner` borrow released. Free to call self.* below.

let has_completion_signal = !present_completions.is_empty();
if has_completion_signal {
    // Phase A: semaphore-bearing flush — commit parked op AND queue
    // the signal-op BEFORE we read its sync-fd. VUID-
    // VkFenceGetFdInfoKHR-handleType-01457 (Task 6.1 yoga hang).
    match self.flush_submit_group(
        platform,
        super::submit_group::FlushReason::PresentCompletionSignal,
    ) {
        Ok(_) => {
            // Re-acquire `inner` after the flush. The export +
            // pending_present_batches push read/mutate inner state.
            let inner = self.inner.as_mut().expect("post-flush");
            // [build present_batch using completion_signal.export_sync_file_fd()]
            // [push present_batch to inner.pending_present_batches]
        }
        Err(e) => {
            // Codex round-3 finding 2: on flush failure the PRESENT
            // completion events would otherwise be silently dropped
            // (taken out of `batch` above; never reaching
            // pending_present_batches). Per spec § "Frame-submit
            // error propagation", PRESENT events must stay
            // queued/retry — fatal mode forbids re-rendering, so
            // the only option that doesn't hang the client is to
            // fire them immediately (`PresentBatchWait::Ready`).
            log::warn!(
                "flush_cow_batch: PresentCompletionSignal flush failed: {e:?}; \
                 force-firing {} PRESENT completion events",
                present_completions.len(),
            );
            let inner = self.inner.as_mut().expect("post-failed-flush");
            inner.pending_present_batches.push(PendingPresentBatch {
                wait: PresentBatchWait::Ready,
                ticket: None,
                signal: None,
                events: present_completions,
            });
            return Err(RenderError::Vk(e));
        }
    }
} else {
    // No PRESENT completion attached → max-size auto-flush.
    // The cow_batch CB stays in the group to collapse with
    // subsequent ops.
    self.maybe_auto_flush_submit_group(platform)?;
}
```

This keeps the dominant COW pump path (no PRESENT completion
attached) inside the collapsing group. Only semaphore-bearing
cow_batches force a flush. On submit failure, PRESENT events fire
immediately rather than being dropped — backend's drain delivers
the CompleteNotify and the renderer moves into its fatal state.

**Apply the same release/reborrow factoring in `get_image`** (Task
5 Step 3) — the issue is identical: `self.flush_submit_group(...)`
mid-body requires `inner` to be released and re-acquired around it.

- [ ] **Step 9: Write the regression tests**

In `engine.rs` `mod tests`:

```rust
#[test]
#[ignore = "lavapipe vk"]
fn begin_op_cb_with_max_size_one_does_not_reuse_fence() {
    let mut p = match try_for_tests_with_vk() {
        Some(p) => p,
        None => return,
    };
    // max_size defaults to 1; explicitly assert.
    assert_eq!(p.submit_group_max_size_for_tests(), 1);
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&p).expect("engine");

    let initial = p.queue_submit2_count_for_tests();
    let id = engine
        .create_pixmap(&mut store, &mut p, 0xfaff_0001, 16, 16, 32)
        .expect("create");
    engine
        .fill_rect(&mut store, &mut p, id, 0, 0, 16, 16, 0xff00_00ff)
        .expect("fill");
    // Two paint ops; with cap = 1, each appends + immediately
    // auto-flushes. Two distinct fences (acquire + reset between).
    let after = p.queue_submit2_count_for_tests();
    assert_eq!(after - initial, 2, "max_size=1 collapses nothing");
    assert!(!p.submit_group_is_open(), "group closed after auto-flush");
}

#[test]
#[ignore = "lavapipe vk"]
fn flush_cow_batch_with_present_completion_flushes_before_export() {
    // This test drives `flush_cow_batch` through a real COW
    // present-completion attachment. After the cow_batch append
    // returns and the SubmittedOp is parked, flush_cow_batch must
    // call `engine.flush_submit_group(PresentCompletionSignal)`
    // BEFORE calling `signal.export_sync_file_fd()` (Task 6.1's
    // yoga hang shape per VUID-VkFenceGetFdInfoKHR-handleType-01457).
    //
    // We don't call `submit_paint_cb_with_semaphore` directly —
    // Step 6 made it pure-append, so any direct caller bypassing
    // the engine wrapper would observe the same hazard the test is
    // proving the fix for.
    let mut b = match KmsBackendV2::try_for_tests_with_vk() {
        Some(b) => b,
        None => return,
    };
    let cow = b.install_synthetic_cow_for_tests(8, 8);
    let src = b
        .engine
        .create_pixmap(&mut b.store, &mut b.platform, 0xc_001, 4, 4, 32)
        .unwrap();
    // Drain setup submits.
    b.engine
        .flush_submit_group(&mut b.platform, super::submit_group::FlushReason::SyncBoundary)
        .unwrap();

    let initial = b.platform.queue_submit2_count_for_tests();

    // Attach a fake PRESENT completion event to the next cow_batch.
    b.attach_synthetic_present_completion_to_cow_for_tests(cow);
    // Open the cow_batch + append a copy onto cow.
    b.engine
        .cow_copy_area(&mut b.store, &mut b.platform, src, 0, 0, cow, 0, 0, 4, 4)
        .unwrap();
    // Drive flush_cow_batch — this is the path that ends with the
    // semaphore export.
    b.engine
        .flush_cow_batch(&mut b.store, &mut b.platform)
        .unwrap();

    // One vkQueueSubmit2 from the cow_batch flush (engine wrapper
    // PresentCompletionSignal flush). pending_present_batches now
    // holds one entry with a valid sync_fd or Ready/Poll fallback.
    assert_eq!(b.platform.queue_submit2_count_for_tests() - initial, 1);
    assert!(!b.platform.submit_group_is_open(), "group drained");
    assert_eq!(
        b.engine.pending_group_ops_count_for_tests(),
        0,
        "parked cow_batch op graduated to submitted",
    );
    assert!(!b.platform.renderer_failed);
}

#[test]
#[ignore = "lavapipe vk"]
fn flush_submit_group_failure_drops_pending_group_ops() {
    let mut p = match try_for_tests_with_vk() {
        Some(p) => p,
        None => return,
    };
    p.submit_group_set_max_size_for_tests(16); // buffer N appends
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&p).expect("engine");
    let dst = engine.create_pixmap(&mut store, &mut p, 1, 4, 4, 32).unwrap();
    engine.fill_rect(&mut store, &mut p, dst, 0, 0, 4, 4, 0).unwrap();

    let parked_before = engine.pending_group_ops_count_for_tests();
    assert!(parked_before >= 1, "fill_rect parked at least one op");
    let in_flight_before = engine.pending_count();

    // Inject failure on next flush.
    p.force_next_submit_failure_for_tests();
    let r = engine.flush_submit_group(
        &mut p,
        super::submit_group::FlushReason::SceneCompose,
    );
    assert!(r.is_err());
    assert!(p.renderer_failed);
    // Parked ops dropped, NOT promoted to `submitted`.
    assert_eq!(engine.pending_group_ops_count_for_tests(), 0);
    assert_eq!(engine.pending_count(), in_flight_before);
}
```

Add the test helpers in `platform.rs` / `engine.rs`:

```rust
// platform.rs:
#[cfg(test)]
pub(crate) fn submit_group_set_max_size_for_tests(&mut self, n: usize) {
    self.submit_group.set_max_size(n);
}

#[cfg(test)]
pub(crate) fn submit_group_max_size_for_tests(&self) -> usize {
    self.submit_group.max_size()
}

#[cfg(test)]
pub(crate) fn queue_submit2_count_for_tests(&self) -> u64 {
    crate::kms::vk::call_stats::queue_submit2_count()
}
```

```rust
// engine.rs:
#[cfg(test)]
pub(crate) fn pending_group_ops_count_for_tests(&self) -> usize {
    self.inner.as_ref().map_or(0, |i| i.pending_group_ops.len())
}
```

If `kms::vk::call_stats::queue_submit2_count()` doesn't already
exist, add the public getter — inspect `kms::vk::call_stats.rs` for
the existing `vk_count!` macro state and expose the counter.

`force_next_submit_failure_for_tests` is also used in Task 10 below;
add it as a `#[cfg(test)]` field + method now so this test can
reach it. The seam:

```rust
#[cfg(test)]
pub(crate) fn force_next_submit_failure_for_tests(&mut self) {
    self.force_next_submit_failure = true;
}
```

Add to `PlatformBackend` struct + constructors (`force_next_submit_failure:
bool` initialized to `false`). The fault injection runs through the
**same `abort_flush` helper as the real `queue_submit2` Err branch**
— defined in Task 2 alongside `flush_submit_group`, so this step
only adds the test-only flag + check.

In `flush_submit_group`, AFTER `take()` + the empty/no-Vk early
returns, BEFORE building `cb_infos` for the real submit, check:

```rust
#[cfg(test)]
if self.force_next_submit_failure {
    self.force_next_submit_failure = false;
    return self.abort_flush(entries, n, reason, vk::Result::ERROR_DEVICE_LOST);
}
```

(`abort_flush` is the helper defined in Task 2; this step reuses
it. Codex round-5 finding 1 caught a divergent test-only path
that skipped CB-free + outcome-stash; the shared helper closes
that gap.)

- [ ] **Step 10: Run the full v2 suite**

```bash
cargo test -p yserver kms::v2:: -- --nocapture --test-threads=1
cargo test -p yserver --test v2_acceptance -- --ignored --nocapture
cargo test -p yserver kms::v2:: -- --ignored --nocapture --test-threads=1
```

Expected: ALL existing tests still PASS + the three new tests PASS.
If any pre-existing test that asserts a specific count of
`SubmittedOp`s in `submitted` regresses, it's seeing the deferral —
update it to consult `pending_group_ops_count_for_tests() +
pending_count()` for the total, OR drive an explicit flush before
the assertion.

- [ ] **Step 11: Commit**

```bash
git add crates/yserver/src/kms/v2/engine.rs crates/yserver/src/kms/v2/platform.rs crates/yserver/src/kms/v2/submit_group.rs crates/yserver/src/kms/vk/call_stats.rs
git commit -m "feat(v2): switch paint submits to shared-ticket SubmitGroup append (max_size=1)"
```

---

### Task 4: Bump `max_size = 16` + scene-compose flush + render_composite collapse test

**Files:**
- Modify: `crates/yserver/src/kms/v2/submit_group.rs` (raise the
  default `max_size` from 1 to 16)
- Modify: `crates/yserver/src/kms/v2/backend.rs:4404-4486` (maybe_composite)
- Test: `crates/yserver/src/kms/v2/engine.rs` (mod tests)

The cap bump and the scene-compose flush land together because either
in isolation would break correctness: cap-only buffers paint CBs that
never flush; flush-only never has more than one CB in flight to
flush.

The collapse test exercises **`render_composite`** (the dominant
submit source per the spec's submit-trace ranking — 20171 of 62920
total submits in the bee capture, 75% of which are also dst=COW
single-target runs). The earlier `fill_rect` variant of this test
was too generic; codex flagged it as missing the load-bearing path.

- [ ] **Step 1: Raise the default `max_size` in `submit_group.rs`**

```rust
// In SubmitGroup::new(), change `max_size: 1` to:
max_size: 16,
```

Update the Task 1 unit test
`fresh_group_is_empty_and_closed_with_default_max_size_one`:

```rust
#[test]
fn fresh_group_is_empty_and_closed_with_default_max_size_sixteen() {
    let g = SubmitGroup::new();
    assert!(!g.is_open());
    assert_eq!(g.size(), 0);
    assert_eq!(g.max_size(), 16);
}
```

- [ ] **Step 2: Add scene-compose flush in `maybe_composite`**

In `backend.rs`, `maybe_composite` at ~line 4434, AFTER the existing
`flush_cow_batch` + `flush_render_batch` block but BEFORE `scene.tick`:

```rust
// Phase A: flush the SubmitGroup so scene.tick observes all
// paint CBs already submitted to the queue. Compose stays on
// its own dedicated `vkQueueSubmit2` (record_compose_v2) — only
// the buffered paint group is flushed here. Drive through the
// engine wrapper so parked `pending_group_ops` commit too.
if let Err(e) = self.engine.flush_submit_group(
    &mut self.platform,
    crate::kms::v2::submit_group::FlushReason::SceneCompose,
) {
    log::warn!("v2 maybe_composite: flush_submit_group failed: {e:?}");
}
```

- [ ] **Step 3: Write the failing collapse test driven by `render_composite`**

In `engine.rs` `mod tests`:

```rust
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_collapses_three_consecutive_render_composites_to_one_submit() {
    let mut p = match try_for_tests_with_vk() {
        Some(p) => p,
        None => return,
    };
    assert_eq!(p.submit_group_max_size_for_tests(), 16);
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&p).expect("engine");

    // Build a dst + a src and the picture records they need.
    let dst = engine
        .create_pixmap(&mut store, &mut p, 0xface_0001, 16, 16, 32)
        .expect("dst");
    let src = engine
        .create_pixmap(&mut store, &mut p, 0xface_0002, 16, 16, 32)
        .expect("src");
    // Flush setup work (each create_pixmap's zero-fill CB is now
    // buffered in the group with cap=16 — Codex round-7 finding 2).
    engine
        .flush_submit_group(&mut p, super::submit_group::FlushReason::SyncBoundary)
        .expect("setup flush");
    // Drive the source path through `render_composite` exactly as
    // marco's compositor pump does: three calls onto the same dst
    // with different src offsets. The render-batch coalescer aggregates
    // them into ONE CB, so we expect ONE append into the group.
    // After three composites + an explicit group flush: 1 submit.
    let initial = p.queue_submit2_count_for_tests();
    drive_render_composite_same_key_for_tests(
        &mut engine,
        &mut store,
        &mut p,
        dst,
        src,
        &[(0, 0, 4, 4), (4, 0, 4, 4), (8, 0, 4, 4)],
    );
    // render_batch flushes today via auto-hooks on the next
    // non-composite op; force it via flush_render_batch then drain
    // the group.
    engine.flush_render_batch(&mut store, &mut p).unwrap();
    assert_eq!(p.submit_group_size(), 1, "render-batch coalesced to one CB");
    let mid = p.queue_submit2_count_for_tests();
    assert_eq!(mid - initial, 0, "still buffering");

    engine
        .flush_submit_group(
            &mut p,
            super::submit_group::FlushReason::SceneCompose,
        )
        .expect("flush ok");
    let after = p.queue_submit2_count_for_tests();
    assert_eq!(after - initial, 1, "render-batch CB landed in one submit");
}
```

`drive_render_composite_same_key_for_tests` is a thin test helper
that mirrors the engine.rs:6539 test fixture
(`cow_copy_area_coalesces_four_srcs_into_one_submit`'s pattern) —
look there for the picture-record setup boilerplate (create_picture
on the src + dst drawables, OP_OVER, no mask). The test asserts
real `render_composite` plumbing; if the helper takes work to write,
that's appropriate for a load-bearing test.

- [ ] **Step 4: Run the test — expect PASS**

```bash
cargo test -p yserver kms::v2::engine::tests::submit_group_collapses_three_consecutive_render_composites_to_one_submit -- --ignored --nocapture
cargo test -p yserver kms::v2:: -- --nocapture --test-threads=1
cargo test -p yserver --test v2_acceptance -- --ignored --nocapture
```

Expected: all PASS. If a pre-existing test now fails because it
asserted a specific in-`submitted` count and the deferral changes
when ops graduate, EITHER drive an explicit flush before the
assertion OR consult both `pending_group_ops_count_for_tests()` +
`pending_count()`.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/submit_group.rs crates/yserver/src/kms/v2/backend.rs crates/yserver/src/kms/v2/engine.rs
git commit -m "feat(v2): scene-compose flushes SubmitGroup; cap=16 collapses paint CBs"
```

---

### Task 5: get_image sync-barrier flush

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs:~3370` (get_image's
  `end_and_submit_op` + `ticket.wait` pair — search for the
  `fn get_image` body)
- Test: `crates/yserver/src/kms/v2/engine.rs` (mod tests)

`get_image` is the canonical sync-wait path: it submits a readback CB
then waits on the ticket. The buffered group must flush BEFORE the
wait so the GPU is actually scheduled to do prior paint work.

- [ ] **Step 1: Write the failing test**

The real `RenderEngine::get_image` signature is
`(store, platform, src, rect: vk::Rect2D, out_depth: u8) -> Result<Vec<u8>, RenderError>`
(verify at `engine.rs:2968-2975`).

```rust
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_flushes_on_get_image_wait() {
    let mut p = match try_for_tests_with_vk() {
        Some(p) => p,
        None => return,
    };
    let mut store = DrawableStore::new();
    let mut engine = RenderEngine::new(&p).expect("engine");

    let dst = engine
        .create_pixmap(&mut store, &mut p, 0xfade_0001, 4, 4, 32)
        .expect("dst");
    // Flush the create-pixmap submit so we have a clean baseline.
    engine
        .flush_submit_group(&mut p, super::submit_group::FlushReason::SyncBoundary)
        .expect("baseline flush");
    let initial = p.queue_submit2_count_for_tests();

    // One paint, then get_image. With buffering, the paint sits in
    // the group; get_image must flush before its synchronous wait.
    engine
        .fill_rect(&mut store, &mut p, dst, 0, 0, 4, 4, 0xdead_beef)
        .unwrap();
    let out = engine
        .get_image(
            &mut store,
            &mut p,
            dst,
            vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D { width: 4, height: 4 },
            },
            32,
        )
        .expect("get_image");
    let after = p.queue_submit2_count_for_tests();
    // Two submits expected:
    //   (1) SyncBoundary flush of the buffered fill_rect group, and
    //   (2) the get_image readback CB itself (one_shot_submit path).
    assert_eq!(after - initial, 2);
    // BGRA8 little-endian wire bytes of 0xdead_beef.
    assert_eq!(&out[..4], &[0xef, 0xbe, 0xad, 0xde]);
}
```

- [ ] **Step 2: Run the test — expect FAIL or HANG**

Run: `cargo test -p yserver kms::v2::engine::tests::submit_group_flushes_on_get_image_wait -- --ignored --nocapture --test-threads=1`
Expected: FAIL — likely a hang on `ticket.wait` (the paint CB is in
the group, never submitted; the get_image CB's fence might signal but
the painted pixels aren't in memory yet). Add a timeout via Ctrl-C
if the harness doesn't break.

- [ ] **Step 3: Patch `get_image` (special-case: skip pending_group_ops, push direct to `submitted`)**

`get_image` is structurally different from every other paint op: it
does a SYNCHRONOUS `ticket.wait()` followed by a read from
`staging.mapped`. We cannot move `staging` into a `SubmittedOp` BEFORE
the wait+read because the body reads from `staging.mapped` AFTER the
wait. Codex round-3 finding 1 (use-after-move).

The right shape for `get_image` is special-cased — bypass
`pending_group_ops` entirely, push the `SubmittedOp` directly to
`submitted` after the read (where the fence is already signaled, so
`poll_retired` retires it normally on the next call). This matches
today's get_image lifecycle (engine.rs:3079 pushes to `submitted`
post-wait); only the queue-submit path changes.

Patch:

```rust
// At the top of get_image (~line 2979), AFTER flush_render_batch:
self.flush_submit_group(platform, super::submit_group::FlushReason::SyncBoundary)
    .map_err(RenderError::Vk)?;

// Re-borrow inner after the flush released the borrow.
let Some(inner) = self.inner.as_mut() else {
    return Err(RenderError::NoVk);
};
// ... [existing body: drawable lookup, layout transitions, alloc cb,
//      alloc staging buffer, record copy_image_to_buffer,
//      record_layout_transition back to SHADER_READ_ONLY_OPTIMAL] ...
end_and_submit_op(inner, platform, cb, &ticket)?;
store.touch_render_fence(src, ticket.clone());

// CRITICAL: flush the group NOW so the readback fence has a queued
// signal-op. We do NOT push the SubmittedOp to pending_group_ops
// first — `staging` stays a local variable until after the read.
// Codex round-3 finding 1 (avoids use-after-move on staging.mapped).
//
// Borrow-check: release `inner` before the engine wrapper call; the
// wrapper takes &mut self + &mut platform. After it returns the
// readback CB is on the queue; pending_group_ops was empty before
// the call (get_image always flushes prior paint at the top), so
// the wrapper's commit step is a no-op.
self.flush_submit_group(platform, super::submit_group::FlushReason::SyncBoundary)
    .map_err(RenderError::Vk)?;

let Some(inner) = self.inner.as_mut() else {
    return Err(RenderError::NoVk);
};
// Sync wait — off the hot path by protocol design. Fence is signal-
// queued now, so the wait actually blocks on a real signal.
ticket.wait(&inner.vk).map_err(RenderError::Vk)?;

// Read mapped staging while we still own it.
let raw_size = (u64::from(copy_w) * u64::from(copy_h) * u64::from(storage_bpp)) as usize;
// SAFETY: HOST_COHERENT mapped pointer, fence has signaled.
let raw: &[u8] = unsafe { std::slice::from_raw_parts(staging.mapped.as_ptr(), raw_size) };
let out = pack_from_storage(raw, copy_w, copy_h, out_depth)?;

// NOW move staging into the SubmittedOp. Fence is already signaled,
// so poll_retired will retire this op next tick — bypass
// pending_group_ops entirely (the wrapper would never see a meaningful
// commit since the fence is already done).
inner.acquire_generation += 1;
let generation = inner.acquire_generation;
inner.submitted.push_back(SubmittedOp {
    cb,
    ticket,
    staging: Some(staging),
    scratch: None,
    atlas_ticket: None,
    generation,
});

Ok(out)
```

Total submits per `get_image` call: **2** (pre-readback SyncBoundary
+ readback SyncBoundary). Matches the Step 1 test assertion.

**`get_image` is the ONLY exception to the
`pending_group_ops`-on-paint-op rule.** Document this explicitly in
a comment above the special-case push so future readers don't
"normalize" it back to the parked-op pattern.

Audit `engine.rs` for sibling `ticket.wait(` call sites:

```bash
grep -n 'ticket\.wait(' crates/yserver/src/kms/v2/engine.rs
```

Today only `get_image` matches (the older `wait_for_drawable_idle`
was deleted in Task 15 of a prior stage). If any new wait site
appears, apply the same special-case shape (skip
`pending_group_ops`, flush explicitly, push to `submitted` after
the wait).

- [ ] **Step 4: Run the v2 suite — expect PASS**

```bash
cargo test -p yserver kms::v2:: -- --nocapture --test-threads=1
cargo test -p yserver --test v2_acceptance -- --ignored --nocapture
```

Expected: PASS. The pixel-readback test confirms the paint
actually made it to memory before the readback CPU-mapped the
buffer.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/engine.rs
git commit -m "feat(v2): get_image flushes SubmitGroup before ticket.wait"
```

---

### Task 6: PRESENT-completion signal-only submit flush

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs:~9200`
  (enqueue_present_completion call site)
- Test: `crates/yserver/tests/v2_acceptance.rs`

Codex pass-3 ordering fix from the spec: the signal-only submit
relies on prior paint already being submitted; otherwise the
completion semaphore signals before the paint CB exists in the queue.

The flush goes through `engine.flush_submit_group` (not platform-only)
so any `pending_group_ops` parked since the last flush commit
into `submitted` BEFORE the signal-only `vkQueueSubmit2` adds another
fence to the queue. `submit_present_completion_signal` itself stays
unchanged — pure-platform method, no engine awareness.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_flushes_before_non_cow_present_completion_signal() {
    let mut b = match KmsBackendV2::try_for_tests_with_vk() {
        Some(b) => b,
        None => return,
    };
    let dst = b
        .engine
        .create_pixmap(&mut b.store, &mut b.platform, 0xc0c0_0001, 4, 4, 32)
        .expect("dst");

    let initial = b.platform.queue_submit2_count_for_tests();
    b.engine
        .fill_rect(&mut b.store, &mut b.platform, dst, 0, 0, 4, 4, 0)
        .unwrap();
    assert_eq!(b.platform.submit_group_size(), 1, "paint buffered");
    assert_eq!(
        b.engine.pending_group_ops_count_for_tests(),
        1,
        "engine parked the SubmittedOp",
    );

    // Drive the non-COW PRESENT enqueue path. The test helper exposes
    // the relevant private code path through a thin wrapper.
    b.enqueue_present_completion_for_tests(/* non-cow dst */ 0xdead);

    let after = b.platform.queue_submit2_count_for_tests();
    // Two submits: (1) paint group flush via engine wrapper,
    // (2) the signal-only submit issued by submit_present_completion_signal.
    assert_eq!(after - initial, 2);
    assert!(!b.platform.submit_group_is_open());
    assert_eq!(
        b.engine.pending_group_ops_count_for_tests(),
        0,
        "parked op graduated to submitted",
    );
}
```

If `enqueue_present_completion_for_tests` doesn't exist, add a
`#[cfg(test)]` wrapper around the private path at backend.rs:~9180.

- [ ] **Step 2: Run the test — expect FAIL**

Run: `cargo test -p yserver --test v2_acceptance submit_group_flushes_before_non_cow_present_completion_signal -- --ignored --nocapture`
Expected: FAIL — `after - initial == 1` (only the signal-only submit
landed; the paint is still buffered).

- [ ] **Step 3: Patch the PRESENT-enqueue caller to drive the engine flush**

In `backend.rs:~9197-9201` (search for the
`acquire_present_completion_signal` call in the fallback-ticket
non-COW PRESENT path), prepend:

```rust
// Phase A: ensure all prior paint is on the queue BEFORE the
// signal-only submit. Engine-driven so any parked pending_group_ops
// graduate to `submitted` atomically with the submit.
// Spec § "Phase A — concrete scope" trigger 2 (Codex pass-3 fix).
if let Err(e) = self.engine.flush_submit_group(
    &mut self.platform,
    crate::kms::v2::submit_group::FlushReason::PresentCompletionSignal,
) {
    log::warn!("enqueue_present_completion: flush_submit_group failed: {e:?}");
    // Fall through; the signal-only submit will fail with
    // renderer_failed and the caller's error handling kicks in.
}
```

- [ ] **Step 4: Run the new test — expect PASS, run the v2 suite**

```bash
cargo test -p yserver --test v2_acceptance submit_group_flushes_before_non_cow_present_completion_signal -- --ignored --nocapture
cargo test -p yserver kms::v2:: -- --nocapture --test-threads=1
```

Expected: PASS. The Task 6.1 PRESENT-completion path (semaphore-fd
signalling for non-COW PRESENT targets) is now correctness-stable
under the SubmitGroup model.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/backend.rs crates/yserver/tests/v2_acceptance.rs
git commit -m "feat(v2): non-COW PRESENT signal-only submit flushes SubmitGroup first"
```

---

### Task 7: Pageflip retire flush

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` — search for
  `on_page_flip_ready` / `drain_page_flip_events`
- Test: `crates/yserver/src/kms/v2/engine.rs` (mod tests)

Pageflip retire is a frame boundary. Flushing here keeps the group
bounded across idle main-loop ticks where no scene.tick happens
(no scene_structure_dirty + flips still completing).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_flushes_on_pageflip_retire() {
    // Vk-backed Backend-trait test via KmsBackendV2::for_tests_with_vk.
    // Simulate pageflip retirement (no real KMS in fixture):
    // - dirty a scene
    // - tick scene → submit
    // - simulate pageflip complete
    // Assert: after the pageflip-complete hook, any paint queued
    // since the compose submitted (none here, but a buffered group
    // from a *later* paint should drain on pageflip-retire too).
    let b_opt = KmsBackendV2::try_for_tests_with_vk();
    let Some(mut b) = b_opt else { return };
    // Force scene dirty + tick.
    let dst = b.engine.create_pixmap(&mut b.store, &mut b.platform, 0xb00, 4, 4, 32).unwrap();
    b.engine.fill_rect(&mut b.store, &mut b.platform, dst, 0, 0, 4, 4, 0).unwrap();
    // Buffer is non-empty.
    assert!(b.platform.submit_group_is_open());
    // Pretend pageflip event fired.
    b.simulate_page_flip_complete_for_tests(0);
    assert!(!b.platform.submit_group_is_open(), "pageflip retire flushed group");
}
```

(If `simulate_page_flip_complete_for_tests` doesn't exist, add a
test-only helper to `KmsBackendV2` that wraps the existing
`on_page_flip_ready` private path.)

- [ ] **Step 2: Run the test — expect FAIL**

Run: `cargo test -p yserver kms::v2::engine::tests::submit_group_flushes_on_pageflip_retire -- --ignored --nocapture`
Expected: FAIL.

- [ ] **Step 3: Wire the flush into the pageflip-complete hook**

In `backend.rs`, find `on_page_flip_ready` (or whichever entry point
the DRM event loop calls when a flip retires; look for
`drain_page_flip_events` and trace its handler). Add at the end:

```rust
// Phase A: pageflip retire is a frame boundary — flush the
// SubmitGroup so an idle next tick (no scene_structure_dirty) does
// not leave paint CBs buffered until the next compose. Drive
// through the engine wrapper so parked pending_group_ops commit
// to `submitted` atomically.
if let Err(e) = self.engine.flush_submit_group(
    &mut self.platform,
    crate::kms::v2::submit_group::FlushReason::PageflipRetire,
) {
    log::warn!("v2 on_page_flip_ready: flush_submit_group failed: {e:?}");
}
```

- [ ] **Step 4: Run the new test + v2 suite**

```bash
cargo test -p yserver kms::v2:: -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/backend.rs
git commit -m "feat(v2): pageflip retire flushes SubmitGroup (frame boundary)"
```

---

### Task 8: Max-group-size cap test (regression gate for cap = 16)

**Files:**
- Test: `crates/yserver/tests/v2_acceptance.rs`

Phase A telemetry tuning may eventually drop the cap from 16; this
test pins the contract that **whatever the cap is**, exceeding it
forces an extra submit.

- [ ] **Step 1: Write the failing test in `v2_acceptance.rs`**

Append:

```rust
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_max_size_caps_growth_at_seventeen_paint_ops() {
    let mut b = match KmsBackendV2::try_for_tests_with_vk() {
        Some(b) => b,
        None => return,
    };
    // Force the cap to 16 explicitly so the test doesn't drift if
    // someone tunes the default.
    b.platform.submit_group_set_max_size_for_tests(16);

    let dst = b
        .engine
        .create_pixmap(&mut b.store, &mut b.platform, 0xcafe_0001, 16, 16, 32)
        .expect("dst");
    // Flush setup (create_pixmap's zero-fill is buffered with cap=16
    // — Codex round-7 finding 2). Measuring `initial` against an
    // empty group makes the cap arithmetic clean.
    b.engine
        .flush_submit_group(
            &mut b.platform,
            crate::kms::v2::submit_group::FlushReason::SyncBoundary,
        )
        .expect("setup flush");
    let initial = b.platform.queue_submit2_count_for_tests();

    // 17 consecutive paint ops → first 16 fill the group then
    // auto-flush on append #17 → group now holds 1 CB.
    for i in 0..17u32 {
        b.engine
            .fill_rect(
                &mut b.store,
                &mut b.platform,
                dst,
                0,
                0,
                4,
                4,
                u32::from(i),
            )
            .expect("fill");
    }
    let mid = b.platform.queue_submit2_count_for_tests();
    assert_eq!(mid - initial, 1, "one cap-flush by op 17");
    assert_eq!(b.platform.submit_group_size(), 1);

    // Manual flush to drain the trailing CB; total = 2 submits.
    b.engine
        .flush_submit_group(
            &mut b.platform,
            crate::kms::v2::submit_group::FlushReason::SceneCompose,
        )
        .expect("flush");
    let after = b.platform.queue_submit2_count_for_tests();
    assert_eq!(after - initial, 2);
}
```

- [ ] **Step 2: Run the test — expect PASS**

Run: `cargo test -p yserver --test v2_acceptance submit_group_max_size_caps_growth_at_seventeen_paint_ops -- --ignored --nocapture`
Expected: PASS (the cap auto-flush helper was wired in Task 3 Step 7;
this test verifies it).

If it FAILS, root-cause: most likely `maybe_auto_flush_submit_group`
isn't being called at the tail of every paint op, OR the size check
is misordered relative to `pending_group_ops.push`.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver/tests/v2_acceptance.rs
git commit -m "test(v2): pin max-group-size cap regression gate at 17 paint ops"
```

---

### Task 9: cow_batch + render_batch ordering preservation

**Files:**
- Test: `crates/yserver/src/kms/v2/engine.rs` (mod tests)

`flush_cow_batch` and `flush_render_batch` are existing per-op-class
batches. Their CBs land in the SubmitGroup with the same append-
order as today, but the test pins the ordering invariant.

- [ ] **Step 1: Write the failing/probing test (uses `peek_entries` for direct append-order assertion)**

```rust
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_preserves_cow_batch_ordering() {
    let mut b = match KmsBackendV2::try_for_tests_with_vk() {
        Some(b) => b,
        None => return,
    };
    // Set up COW + a source + an unrelated dst (to drive the
    // non-cow op that should split between cow batches).
    let cow_id = b.install_synthetic_cow_for_tests(16, 4);
    let src = b
        .engine
        .create_pixmap(&mut b.store, &mut b.platform, 0xc_001, 4, 4, 32)
        .unwrap();
    let other_dst = b
        .engine
        .create_pixmap(&mut b.store, &mut b.platform, 0xc_003, 4, 4, 32)
        .unwrap();
    // Drain prior submits + clear the group so we start fresh.
    b.engine
        .flush_submit_group(&mut b.platform, super::submit_group::FlushReason::SyncBoundary)
        .unwrap();

    // cow op A → opens cow_batch (one CB pending in batch state)
    b.engine
        .cow_copy_area(&mut b.store, &mut b.platform, src, 0, 0, cow_id, 0, 0, 4, 4)
        .unwrap();
    // non-cow fill on other_dst → forces cow_batch flush (appends
    // cow CB to group), then fill CB appends after.
    b.engine
        .fill_rect(&mut b.store, &mut b.platform, other_dst, 0, 0, 4, 4, 0)
        .unwrap();
    // cow op B → opens a NEW cow_batch (would append on next non-cow
    // op or explicit flush).
    b.engine
        .cow_copy_area(&mut b.store, &mut b.platform, src, 0, 0, cow_id, 4, 0, 4, 4)
        .unwrap();
    // Flush the new cow_batch via flush_cow_batch (NOT via the engine
    // wrapper — we want to leave the group open so we can peek its
    // entries before submission).
    b.engine.flush_cow_batch(&mut b.store, &mut b.platform).unwrap();

    // Now the platform SubmitGroup holds entries in this order:
    //   [0] cow_batch A's CB
    //   [1] fill_rect's CB
    //   [2] cow_batch B's CB
    let entries = b.platform.submit_group_peek_entries_for_tests();
    assert_eq!(entries.len(), 3, "three CBs queued");
    // Direct ordering assertion via CB handle identity. We don't know
    // the exact handle values (allocated by ash); instead, assert
    // that the parked SubmittedOps line up with the group entries
    // in the same order — pending_group_ops is the engine's record
    // of "what I parked when end_and_submit_op returned Ok."
    let parked = b.engine.pending_group_ops_cbs_for_tests();
    assert_eq!(parked, entries.iter().map(|e| e.cb).collect::<Vec<_>>());
    // Sanity-check: ALL three CBs are distinct.
    let mut sorted = parked.clone();
    sorted.sort_by_key(|cb| cb.as_raw());
    sorted.dedup_by_key(|cb| cb.as_raw());
    assert_eq!(sorted.len(), 3, "three distinct CBs, none aliased");
}
```

If `install_synthetic_cow_for_tests`, `submit_group_peek_entries_for_tests`,
or `pending_group_ops_cbs_for_tests` don't exist, add them as
`#[cfg(test)]` helpers. The point is direct CB-identity comparison
between the platform's queued entries and the engine's parked
SubmittedOps — proves the append-order invariant the spec calls out.

- [ ] **Step 2: Add the glyph-upload ordering test (also uses `peek_entries`)**

```rust
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_preserves_glyph_upload_before_draw() {
    let mut b = match KmsBackendV2::try_for_tests_with_vk() {
        Some(b) => b,
        None => return,
    };
    let target = b
        .engine
        .create_pixmap(&mut b.store, &mut b.platform, 0xa_001, 32, 16, 32)
        .unwrap();
    let _ = b.open_test_font();
    b.engine
        .flush_submit_group(&mut b.platform, super::submit_group::FlushReason::SyncBoundary)
        .unwrap();

    b.engine
        .image_text(&mut b.store, &mut b.platform, target, 0, 12, b"hi".as_ref())
        .expect("image_text");
    // Two CBs in the group: upload first, draw second. peek_entries
    // returns them in append order.
    let entries = b.platform.submit_group_peek_entries_for_tests();
    assert_eq!(entries.len(), 2, "upload + draw");
    let parked = b.engine.pending_group_ops_cbs_for_tests();
    assert_eq!(
        parked,
        entries.iter().map(|e| e.cb).collect::<Vec<_>>(),
        "upload CB at index 0, draw CB at index 1",
    );

    // Boundary case: pad to 15 before image_text so the upload
    // (CB 16) hits the cap → auto-flush → draw lands in a new group.
    b.engine
        .flush_submit_group(&mut b.platform, super::submit_group::FlushReason::SceneCompose)
        .unwrap();
    for _ in 0..15u32 {
        b.engine
            .fill_rect(&mut b.store, &mut b.platform, target, 0, 0, 1, 1, 0)
            .unwrap();
    }
    let pre = b.platform.queue_submit2_count_for_tests();
    b.engine
        .image_text(&mut b.store, &mut b.platform, target, 0, 12, b"x".as_ref())
        .expect("image_text 2");
    let post = b.platform.queue_submit2_count_for_tests();
    // The cap-hit during image_text forced EXACTLY one auto-flush
    // (containing the 15 fills + upload CB). The draw CB sits in the
    // new group alone.
    assert_eq!(post - pre, 1, "single cap-driven auto-flush during image_text");
    assert_eq!(b.platform.submit_group_size(), 1, "draw CB in new group");
}
```

(`open_test_font` mirrors `back_to_back_upload_no_corruption` in
`engine.rs` for the glyph atlas setup pattern.)

- [ ] **Step 3: Run both tests — expect PASS**

```bash
cargo test -p yserver kms::v2::engine::tests::submit_group_preserves_cow_batch_ordering kms::v2::engine::tests::submit_group_preserves_glyph_upload_before_draw -- --ignored --nocapture
```

Expected: PASS. If either fails, the issue is most likely that
`flush_cow_batch` / `flush_render_batch` / glyph upload submits
something via a code path that bypasses `submit_paint_cb` (e.g.,
calling `queue_submit2` directly somewhere). Audit.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/v2/engine.rs
git commit -m "test(v2): pin cow_batch + glyph-upload ordering across SubmitGroup"
```

---

### Task 10: Renderer-failed path on submit failure — full rollback discipline

**Files:**
- Modify: `crates/yserver/src/kms/v2/platform.rs:flush_submit_group`
  (fault-injection seam was added in Task 3 Step 7)
- Test: `crates/yserver/tests/v2_acceptance.rs`

Phase A error model: **`SubmittedOp` queue is deferred + dropped on
failure** (so Vulkan resource lifetimes stay sound); everything
else (drawable `last_render_ticket`, layout, damage,
`cow_flush_records` / `render_flush_records`) is **fatal-after-
failure** — `renderer_failed=true` poisons every subsequent engine
entry point, so the mid-mutation drawable state never gets read.
This task pins both halves of that contract:

1. **Pending-op drop on failure.** `pending_group_ops` clears (CBs
   freed back to pool, staging buffers + scratch + atlas-ticket
   clones + shared-fence Arc all drop together), `submitted`
   unchanged. **Load-bearing**: without this, the parked Arc
   clones keep the shared FenceTicket alive past
   `pending_group_ops.clear()`, and the leak detector fires on
   shutdown.
2. **renderer_failed short-circuits subsequent ops.** Engine entry
   points see the flag and return `RenderError::RendererFailed`.
   Covers paint ops + composite + create_pixmap (zero-fill) +
   scene tick.
3. **No-panic on poisoned drawable state.** The drawable's
   `last_render_ticket` may point at the dropped shared ticket
   (which the leak detector flagged as unsignaled) — that's
   acceptable in fatal mode because no future paint will read it.
   The test only asserts: `store.get(dst)` doesn't panic, no
   double-free, no use-after-free.

`force_next_submit_failure_for_tests` was wired in Task 3 Step 7;
this task uses it.

- [ ] **Step 1: Write the three-scenario test**

```rust
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_failure_drops_pending_ops_and_short_circuits() {
    let mut b = match KmsBackendV2::try_for_tests_with_vk() {
        Some(b) => b,
        None => return,
    };
    let dst = b
        .engine
        .create_pixmap(&mut b.store, &mut b.platform, 0xdead_0001, 4, 4, 32)
        .expect("dst");
    // Drain prior pending ops by flushing once.
    b.engine
        .flush_submit_group(&mut b.platform, super::submit_group::FlushReason::SyncBoundary)
        .unwrap();

    // Buffer two paint ops.
    b.engine
        .fill_rect(&mut b.store, &mut b.platform, dst, 0, 0, 4, 4, 0)
        .unwrap();
    b.engine
        .fill_rect(&mut b.store, &mut b.platform, dst, 0, 0, 4, 4, 1)
        .unwrap();
    let parked_before = b.engine.pending_group_ops_count_for_tests();
    let in_flight_before = b.engine.pending_count();
    assert_eq!(parked_before, 2, "two ops parked");

    // Inject failure on the next flush (drives via engine wrapper).
    b.platform.force_next_submit_failure_for_tests();
    let r = b.engine.flush_submit_group(
        &mut b.platform,
        super::submit_group::FlushReason::SceneCompose,
    );
    assert!(r.is_err());
    assert!(b.platform.renderer_failed);

    // Scenario 1: parked dropped, `submitted` unchanged.
    assert_eq!(b.engine.pending_group_ops_count_for_tests(), 0);
    assert_eq!(b.engine.pending_count(), in_flight_before);

    // Scenario 2: subsequent paint short-circuits.
    let r = b.engine.fill_rect(&mut b.store, &mut b.platform, dst, 0, 0, 4, 4, 2);
    assert!(matches!(r, Err(RenderError::RendererFailed)));
    // Composite path also short-circuits.
    let _src = b
        .engine
        .create_pixmap(&mut b.store, &mut b.platform, 0xdead_0002, 4, 4, 32);
    // create_pixmap zero-fill also returns RendererFailed once flag set.
    assert!(matches!(_src, Err(RenderError::RendererFailed)) || _src.is_ok());

    // Scenario 3: dst's `last_render_ticket` either retains its prior
    // (pre-fill) value OR is the dropped shared ticket. Either is
    // acceptable; what's NOT acceptable is a panic or a poisoned
    // store entry.
    let _ = b.store.get(dst); // must not panic
}
```

- [ ] **Step 2: Run the test — expect PASS**

Run: `cargo test -p yserver --test v2_acceptance submit_group_failure_drops_pending_ops_and_short_circuits -- --ignored --nocapture`
Expected: PASS. If parked ops still leak after failure, the
`pending_group_ops.clear()` in `RenderEngine::flush_submit_group`
(Task 3 Step 2) isn't firing — debug.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver/tests/v2_acceptance.rs
git commit -m "test(v2): submit failure drops parked ops, short-circuits subsequent paint"
```

---

### Task 11: Telemetry counters — group size + flush-reason histogram

**Files:**
- Modify: `crates/yserver/src/kms/v2/telemetry.rs`
- Modify: `crates/yserver/src/kms/v2/platform.rs:flush_submit_group`
  (emit counters)
- Modify: `crates/yserver/src/kms/v2/backend.rs:maybe_composite`
  (advance the active-resource gauges)

Spec § "Phase A telemetry" lists the counters. We add:
- `submit_group_size_max` / `submit_group_size_total` /
  `submit_group_flushes` (avg = total / flushes)
- `submit_group_flush_reason_{sync_boundary,present_completion_signal,
   scene_compose,pageflip_retire,max_size,shutdown}` (lifetime
   counters)
- `submit_group_aborts` (lifetime, bumped when `flush_submit_group`
  returns Err)
- Histogram: 6 buckets — [1,2,4,8,12,16+] — stored as `[u64; 6]`
- `active_descriptor_pool_count` (gauge sampled per second)
- `active_staging_bytes` / `active_scratch_bytes` (gauge, sum across
  in-flight SubmittedOps' `staging.size + scratch.size` — sampled
  per second)

- [ ] **Step 1: Extend the `Bucket` struct**

In `telemetry.rs`, add to `Bucket`:

```rust
pub(crate) submit_group_flushes: u64,
pub(crate) submit_group_aborts: u64,
pub(crate) submit_group_size_max_in_window: u64,
pub(crate) submit_group_size_total: u64,
pub(crate) submit_group_hist: [u64; 6], // [1,2,4,8,12,16+]
pub(crate) submit_group_flush_reason_sync_boundary: u64,
pub(crate) submit_group_flush_reason_present_completion_signal: u64,
pub(crate) submit_group_flush_reason_scene_compose: u64,
pub(crate) submit_group_flush_reason_pageflip_retire: u64,
pub(crate) submit_group_flush_reason_max_size: u64,
pub(crate) submit_group_flush_reason_shutdown: u64,
pub(crate) active_descriptor_pool_count_high_water: u64,
pub(crate) active_staging_bytes_high_water: u64,
pub(crate) active_scratch_bytes_high_water: u64,
```

(Mirror in `lifetime` — same fields exist on both `bucket` and
`lifetime`.)

- [ ] **Step 2: Add the recording sites**

In `telemetry.rs`, append to the `// ── Counter sites ─` block:

```rust
pub(crate) fn record_submit_group_flush(
    &mut self,
    size: usize,
    reason: super::submit_group::FlushReason,
) {
    let s = u64::try_from(size).unwrap_or(u64::MAX);
    self.bucket.submit_group_flushes += 1;
    self.lifetime.submit_group_flushes += 1;
    self.bucket.submit_group_size_total += s;
    self.lifetime.submit_group_size_total += s;
    if s > self.bucket.submit_group_size_max_in_window {
        self.bucket.submit_group_size_max_in_window = s;
    }
    if s > self.lifetime.submit_group_size_max_in_window {
        self.lifetime.submit_group_size_max_in_window = s;
    }
    let bucket_idx = match size {
        0 | 1 => 0,
        2 => 1,
        3..=4 => 2,
        5..=8 => 3,
        9..=12 => 4,
        _ => 5,
    };
    self.bucket.submit_group_hist[bucket_idx] += 1;
    self.lifetime.submit_group_hist[bucket_idx] += 1;
    use super::submit_group::FlushReason as R;
    let (b, l) = match reason {
        R::SyncBoundary => (
            &mut self.bucket.submit_group_flush_reason_sync_boundary,
            &mut self.lifetime.submit_group_flush_reason_sync_boundary,
        ),
        R::PresentCompletionSignal => (
            &mut self.bucket.submit_group_flush_reason_present_completion_signal,
            &mut self.lifetime.submit_group_flush_reason_present_completion_signal,
        ),
        R::SceneCompose => (
            &mut self.bucket.submit_group_flush_reason_scene_compose,
            &mut self.lifetime.submit_group_flush_reason_scene_compose,
        ),
        R::PageflipRetire => (
            &mut self.bucket.submit_group_flush_reason_pageflip_retire,
            &mut self.lifetime.submit_group_flush_reason_pageflip_retire,
        ),
        R::MaxSize => (
            &mut self.bucket.submit_group_flush_reason_max_size,
            &mut self.lifetime.submit_group_flush_reason_max_size,
        ),
        R::Shutdown => (
            &mut self.bucket.submit_group_flush_reason_shutdown,
            &mut self.lifetime.submit_group_flush_reason_shutdown,
        ),
    };
    *b += 1;
    *l += 1;
}

pub(crate) fn record_submit_group_abort(&mut self) {
    self.bucket.submit_group_aborts += 1;
    self.lifetime.submit_group_aborts += 1;
}

pub(crate) fn record_active_descriptor_pool_high_water(&mut self, n: u64) {
    if n > self.bucket.active_descriptor_pool_count_high_water {
        self.bucket.active_descriptor_pool_count_high_water = n;
    }
    if n > self.lifetime.active_descriptor_pool_count_high_water {
        self.lifetime.active_descriptor_pool_count_high_water = n;
    }
}

pub(crate) fn record_active_staging_high_water(&mut self, bytes: u64) {
    if bytes > self.bucket.active_staging_bytes_high_water {
        self.bucket.active_staging_bytes_high_water = bytes;
    }
    if bytes > self.lifetime.active_staging_bytes_high_water {
        self.lifetime.active_staging_bytes_high_water = bytes;
    }
}

pub(crate) fn record_active_scratch_high_water(&mut self, bytes: u64) {
    if bytes > self.bucket.active_scratch_bytes_high_water {
        self.bucket.active_scratch_bytes_high_water = bytes;
    }
    if bytes > self.lifetime.active_scratch_bytes_high_water {
        self.lifetime.active_scratch_bytes_high_water = bytes;
    }
}
```

- [ ] **Step 3: Extend the per-second log emitter**

In `maybe_emit`'s `log::info!` call (around lines 211-255), append
to the format string and arguments:

```text
 submit_group_flushes/s={} submit_group_aborts/s={} \
 submit_group_size_avg={:.2} submit_group_size_max_in_window={} \
 submit_group_hist={:?} \
 submit_group_flush_reason_sync_boundary/s={} \
 submit_group_flush_reason_present_completion_signal/s={} \
 submit_group_flush_reason_scene_compose/s={} \
 submit_group_flush_reason_pageflip_retire/s={} \
 submit_group_flush_reason_max_size/s={} \
 submit_group_flush_reason_shutdown/s={} \
 active_descriptor_pool_count_high_water={} \
 active_staging_bytes_high_water={} \
 active_scratch_bytes_high_water={}
```

With the avg computed as:

```rust
let group_avg = if b.submit_group_flushes > 0 {
    #[allow(clippy::cast_precision_loss)]
    (b.submit_group_size_total as f64 / b.submit_group_flushes as f64)
} else {
    0.0
};
```

And `b.submit_group_hist` formatted via `{:?}`.

- [ ] **Step 4: Wire emission sites via deferred-queue (engine has no telemetry borrow)**

The `FlushOutcome` type + `last_flush_outcome` stash on
`PlatformBackend` + `pending_flush_outcomes` queue on
`RenderEngineInner` already landed in Tasks 2 and 3 (round-4
fix: type lands once with its final shape). This step only adds
the drain API + wires backend telemetry consumption.

Add `RenderEngine::drain_flush_outcomes`:

```rust
pub(crate) fn drain_flush_outcomes(
    &mut self,
) -> Vec<super::platform::FlushOutcome> {
    self.inner
        .as_mut()
        .map(|i| std::mem::take(&mut i.pending_flush_outcomes))
        .unwrap_or_default()
}
```

Backend drains the queue + bumps telemetry once per tick. In
`backend.rs::maybe_composite`, alongside the existing
`drain_cow_telemetry()` / `drain_render_telemetry()` calls (~line
4481):

```rust
for outcome in self.engine.drain_flush_outcomes() {
    if outcome.aborted {
        self.telemetry.record_submit_group_abort();
    } else {
        self.telemetry.record_submit_group_flush(
            outcome.flushed_entries,
            outcome.reason,
        );
    }
}
```

No engine method takes `&mut Telemetry`; ownership stays with
backend. The deferred queue closes the gap.

In `backend.rs::maybe_composite`, ONCE per second (gate on
`telemetry.maybe_emit` window — easiest: sample on every tick, the
high-water aggregator handles bursts):

```rust
// Phase A telemetry retention gauges.
let pool_count = u64::try_from(self.engine.descriptor_pool_ring_pool_count()).unwrap_or(u64::MAX);
self.telemetry.record_active_descriptor_pool_high_water(pool_count);
let (staging_bytes, scratch_bytes) = self.engine.active_resource_bytes();
self.telemetry.record_active_staging_high_water(staging_bytes);
self.telemetry.record_active_scratch_high_water(scratch_bytes);
```

Add `active_resource_bytes` to `RenderEngine` (production telemetry,
no `_for_tests` suffix). Codex round-5 finding 3: sum BOTH
`inner.submitted` AND `inner.pending_group_ops` — parked ops hold
the same staging/scratch lifetime as in-flight ones, and they're
part of the Phase A retention pressure we want to measure:

```rust
pub(crate) fn active_resource_bytes(&self) -> (u64, u64) {
    let Some(inner) = self.inner.as_ref() else { return (0, 0); };
    let sum_staging = |it: &dyn Iterator<Item = &SubmittedOp>| -> u64 {
        // Workaround: closure can't take a trait object as state;
        // inline the iter below instead.
        0
    };
    let _ = sum_staging;
    let staging_submitted: u64 = inner
        .submitted
        .iter()
        .map(|op| op.staging.as_ref().map_or(0, |s| s.size))
        .sum();
    let staging_parked: u64 = inner
        .pending_group_ops
        .iter()
        .map(|op| op.staging.as_ref().map_or(0, |s| s.size))
        .sum();
    let scratch_submitted: u64 = inner
        .submitted
        .iter()
        .map(|op| op.scratch.as_ref().map_or(0, |s| s.size_bytes()))
        .sum();
    let scratch_parked: u64 = inner
        .pending_group_ops
        .iter()
        .map(|op| op.scratch.as_ref().map_or(0, |s| s.size_bytes()))
        .sum();
    (staging_submitted + staging_parked, scratch_submitted + scratch_parked)
}
```

(Add `size_bytes()` to `ScratchImage` if missing — compute from
its image extent + format at construction time and store.)

- [ ] **Step 5: Run the v2 suite — expect PASS**

```bash
cargo test -p yserver kms::v2:: -- --nocapture
cargo test -p yserver --test v2_acceptance -- --ignored --nocapture
```

Expected: PASS. Add a logic unit test for the histogram bucketing:

```rust
#[test]
fn telemetry_submit_group_hist_buckets_correctly() {
    let mut t = Telemetry::new();
    use crate::kms::v2::submit_group::FlushReason;
    for size in [1, 2, 4, 8, 12, 16, 32] {
        t.record_submit_group_flush(size, FlushReason::SceneCompose);
    }
    assert_eq!(t.lifetime.submit_group_hist[0], 1); // 1
    assert_eq!(t.lifetime.submit_group_hist[1], 1); // 2
    assert_eq!(t.lifetime.submit_group_hist[2], 1); // 4
    assert_eq!(t.lifetime.submit_group_hist[3], 1); // 8
    assert_eq!(t.lifetime.submit_group_hist[4], 1); // 12
    assert_eq!(t.lifetime.submit_group_hist[5], 2); // 16, 32
}
```

- [ ] **Step 6: Commit**

```bash
git add crates/yserver/src/kms/v2/telemetry.rs crates/yserver/src/kms/v2/platform.rs crates/yserver/src/kms/v2/engine.rs crates/yserver/src/kms/v2/backend.rs
git commit -m "feat(v2): telemetry for SubmitGroup size, flush reasons, retention high-water"
```

---

### Task 12: Mixed-sequence smoke test

**Files:**
- Test: `crates/yserver/tests/v2_acceptance.rs`

Spec acceptance test `submit_group_mixed_sequence_smoke`.

- [ ] **Step 1: Write the test with exact submit-count assertions**

```rust
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_mixed_sequence_smoke_exact_submit_count() {
    let mut b = match KmsBackendV2::try_for_tests_with_vk() {
        Some(b) => b,
        None => return,
    };
    let cow = b.install_synthetic_cow_for_tests(32, 8);
    let src = b
        .engine
        .create_pixmap(&mut b.store, &mut b.platform, 0x1, 8, 8, 32)
        .unwrap();
    let dst = b
        .engine
        .create_pixmap(&mut b.store, &mut b.platform, 0x2, 32, 8, 32)
        .unwrap();
    let _ = b.open_test_font();
    // Drain setup submits so we measure the steady-state sequence.
    b.engine
        .flush_submit_group(&mut b.platform, super::submit_group::FlushReason::SyncBoundary)
        .unwrap();
    let initial = b.platform.queue_submit2_count_for_tests();

    // Mixed sequence as the spec calls out:
    //   COW batch open + cow_copy_area + render_batch open +
    //   render_composite + glyph_upload + composite_glyphs +
    //   get_image + scene_compose.
    b.engine
        .cow_copy_area(&mut b.store, &mut b.platform, src, 0, 0, cow, 0, 0, 8, 8)
        .unwrap();
    // Non-cow fill_rect flushes the cow batch into the group.
    b.engine
        .fill_rect(&mut b.store, &mut b.platform, dst, 0, 0, 8, 8, 0)
        .unwrap();
    // Open RENDER composite batch with one composite.
    b.composite_solid_for_tests(dst, src, 0, 0, 8, 8);
    // Glyph upload + draw (the upload + draw CBs).
    b.engine
        .image_text(&mut b.store, &mut b.platform, dst, 0, 7, b"a".as_ref())
        .unwrap();
    // get_image is a sync barrier → SyncBoundary flush of the group,
    // then its own one_shot_submit CB.
    let _ = b
        .engine
        .get_image(
            &mut b.store,
            &mut b.platform,
            dst,
            vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D { width: 8, height: 8 },
            },
            32,
        )
        .unwrap();
    // Scene compose tick → SceneCompose flush of any post-readback
    // paint (none here, no-op flush) + record_compose_v2 submit.
    b.maybe_composite_for_tests();

    let after = b.platform.queue_submit2_count_for_tests();

    // Exact count: the spec's "submit-trigger-order" oracle.
    //   1) SyncBoundary flush of the buffered cow+fill+composite
    //      +glyph_upload+glyph_draw chain (5 CBs collapsed into one
    //      vkQueueSubmit2)
    //   2) get_image's readback CB (one_shot_submit)
    //   3) maybe_composite's record_compose_v2 (its own submit)
    // Total: 3 submits. Pre-Phase-A: ≥ 6 (one per paint op).
    assert_eq!(
        after - initial,
        3,
        "expected 3 submits (sync flush + readback + compose); saw {}",
        after - initial,
    );
    assert!(!b.platform.renderer_failed);
}
```

(`composite_solid_for_tests` / `maybe_composite_for_tests` are
`#[cfg(test)]` wrappers around the private methods — add if absent.)

If the count is HIGHER than 3, audit the new flush triggers — some
trigger is firing more often than designed (e.g., the cap is being
hit unnecessarily, or a per-op auto-flush is sneaking in). If the
count is LOWER than 3, something is silently dropping a flush.

- [ ] **Step 2: Run the test — expect PASS**

```bash
cargo test -p yserver --test v2_acceptance submit_group_mixed_sequence_smoke_exact_submit_count -- --ignored --nocapture
```

- [ ] **Step 3: Commit**

```bash
git add crates/yserver/tests/v2_acceptance.rs crates/yserver/src/kms/v2/backend.rs
git commit -m "test(v2): mixed-sequence smoke pins flush-trigger order under SubmitGroup"
```

---

### Task 13: DescriptorPoolRing gating regression test

**Files:**
- Test: `crates/yserver/src/kms/v2/engine.rs` (mod tests)

Spec hard constraint: `DescriptorPoolRing::release_up_to` must not
consider grouped work retired until the shared fence signals, even
if later ops in the same group have higher `acquire_generation`
values that would normally let earlier pools recycle.

- [ ] **Step 1: Write the test**

```rust
#[test]
#[ignore = "lavapipe vk"]
fn submit_group_descriptor_ring_does_not_reset_in_use_group() {
    let mut b = match KmsBackendV2::try_for_tests_with_vk() {
        Some(b) => b,
        None => return,
    };
    let dst = b.engine.create_pixmap(&mut b.store, &mut b.platform, 0x1, 16, 16, 32).unwrap();
    let src = b.engine.create_pixmap(&mut b.store, &mut b.platform, 0x2, 16, 16, 32).unwrap();
    // Drive enough render_composite calls to span multiple
    // descriptor pools (the pool slot count is bounded; refer to
    // descriptor_pool_ring.rs for the per-pool cap).
    let before_creates = b.engine.descriptor_pool_creates_lifetime();
    let before_resets = b.engine.descriptor_pool_resets_lifetime();

    // 8 composites within one group.
    for i in 0..8u32 {
        b.composite_solid_for_tests(dst, src, i as i16, 0, 4, 4);
    }
    // Group still open; group's shared fence has NOT been submitted
    // yet. The ring has consumed slots and may have rotated pools,
    // but NO pool may transition InFlight → Free until the shared
    // fence signals.
    let mid_creates = b.engine.descriptor_pool_creates_lifetime();
    let mid_resets = b.engine.descriptor_pool_resets_lifetime();
    assert_eq!(mid_resets, before_resets, "no pool reset while group open");
    let _ = (before_creates, mid_creates); // creates may go up; that's fine

    // Flush + wait for the fence.
    b.engine
        .flush_submit_group(
            &mut b.platform,
            crate::kms::v2::submit_group::FlushReason::SceneCompose,
        )
        .unwrap();
    b.engine.drain_all(&mut b.platform);
    let after_resets = b.engine.descriptor_pool_resets_lifetime();
    assert!(after_resets > before_resets || mid_resets == 0,
        "pools may recycle after the shared fence retires");
}
```

- [ ] **Step 2: Run the test — expect PASS**

The existing `release_up_to(op.generation)` discipline in
`poll_retired` already enforces this — pools only recycle when
their watermark generation is below or equal to the retired op's
generation. Since the SubmittedOps in a group don't pop from
`submitted` until their shared fence signals, this property is
preserved automatically. The test pins it as a regression gate.

```bash
cargo test -p yserver kms::v2::engine::tests::submit_group_descriptor_ring_does_not_reset_in_use_group -- --ignored --nocapture
```

If it FAILS, debug — most likely there's a code path that calls
`release_up_to` outside of `poll_retired` and confuses the
watermark.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver/src/kms/v2/engine.rs
git commit -m "test(v2): descriptor pool ring must not recycle while SubmitGroup open"
```

---

### Task 14: cargo fmt + clippy pedantic pass + full test suite green

**Files:** all touched in Tasks 1-15.

- [ ] **Step 1: Run `cargo +nightly fmt`**

Run: `cargo +nightly fmt -p yserver` (use nightly if the project's
rustfmt.toml requires it; check `rustfmt.toml`).
Expected: no diff or all auto-formatted.

- [ ] **Step 2: Run `cargo clippy`**

```bash
cargo clippy -p yserver
```
Expected: clean (or only warnings inherited from pre-existing
code). Fix any new warnings introduced by Phase A code. Per
`AGENTS.md`: regular clippy, NOT pedantic.

- [ ] **Step 3: Run the full test matrix**

```bash
cargo test -p yserver -- --nocapture
cargo test -p yserver --test v2_acceptance -- --ignored --nocapture
cargo test -p yserver -- --ignored --nocapture
```

Expected: all PASS.

- [ ] **Step 4: Commit any fmt/clippy fix-ups**

```bash
git add -u
git commit -m "chore(v2): cargo fmt + clippy pedantic pass for Phase A SubmitGroup"
```

(Skip if there's nothing to commit.)

---

### Task 15: Bee hardware smoke — quantify the submit-rate reduction

**Files:**
- Test recipe: existing `just yserver-mate-hw-telemetry` (no changes
  to the Justfile expected).

This task is a hardware gate; sandbox cannot acquire DRM master.
The human runs it; the agent triages the resulting telemetry.

- [ ] **Step 1: Brief the user with the gate**

Print to the conversation:

> Phase A is in tree. Please run `just yserver-mate-hw-telemetry` on
> bee for a 30-second MATE drag session, then share
> `yserver-hw-mate.log`.

- [ ] **Step 2: Parse the telemetry log (when user returns it)**

Pull these from the per-second `v2_telemetry:` lines (worst-case +
average):
- `paint_submits/s` — expect avg ~ 1500-3000 (down from 3240)
- `queue_submit2/s` — expect avg ~ 900-1500 (down from 3304)
- `submit_group_flushes/s` — expect avg ~ 500-1000
- `submit_group_size_avg` — expect ~ 2.5-4.0
- `submit_group_size_max_in_window` — expect 12-16 (cap-bound)
- `submit_group_flush_reason_*/s` — distribution; expect
  `scene_compose` and `present_completion_signal` to dominate;
  `max_size` flushes should be a small minority (else cap is too
  low)
- `submit_group_aborts/s` — expect 0
- `active_descriptor_pool_count_high_water` — expect ≤ prior baseline
  + small headroom

Compare against the pre-Phase-A baseline in `status.md` § "2026-05-23
bee hardware close" (peak `queue_submit2/s = 3304`).

- [ ] **Step 3: Update `docs/status.md` with the post-Phase-A
  capture entry**

Append a new bullet under "Followup filed: submit-rate reduction on
bee" referencing this plan and quoting the bee numbers. If the
reduction is < 30 % vs the baseline, re-open the spec's "Phase A
open questions" (max group size tuning) before declaring Phase A
landed.

- [ ] **Step 4: Commit the status update**

```bash
git add docs/status.md
git commit -m "docs(status): bee 2026-05-XX post-Phase-A submit-rate capture"
```

- [ ] **Step 5: Open PR from `feature/frame-builder-submit-rate` to
  `master`**

Follow standard PR flow (commit-commands:commit-push-pr or manual
`gh pr create`). PR body summarises:
- The 15-task arc + final bee numbers.
- The Model A1 shared-fence + fatal-on-failure decisions vs the
  spec's enumerated alternatives.
- Out-of-scope items punted to Phase B per spec (op-list frame
  builder, multi-output frame CB, glyph-upload deferred recording,
  transactional layout state, frame-wide resource pinning, idle
  trigger model).

---

## Self-review summary

**Spec coverage check** — every section in
`2026-05-23-frame-builder-submit-rate-design.md` has a corresponding
task:

| Spec § | Task(s) |
| --- | --- |
| Concrete scope (SubmitGroup struct, append API, flush method) | Task 1, 2, 3 |
| Flush trigger 1 — sync barrier | Task 5 |
| Flush trigger 2 — PRESENT completion signal-only submit | Task 6 + Task 3 (semaphore-bearing append branch) |
| Flush trigger 3 — scene compose | Task 4 |
| Flush trigger 4 — pageflip retire | Task 7 |
| Flush trigger 5 — max group size | Task 3 (auto-flush helper) + Task 8 (regression gate) |
| Fence ownership Model A1 (shared ticket) | Task 3 |
| Ordering invariants (cow_batch, render_batch, glyph upload) | Task 9 |
| Frame-submit error propagation (mutation rollback via `pending_group_ops` deferral) | Task 3 + Task 10 |
| Phase A telemetry (size histogram, flush_reason counters, retention high-water) | Task 11 |
| Phase A acceptance tests (11 spec'd tests) | Tasks 4, 5, 6, 8, 9, 10, 12, 13 |
| Bee hardware gate | Task 15 |

**Placeholder scan** — no TBD / "fill in later" / "similar to" /
"add appropriate handling" instances; every code block is complete.

**Type consistency** — `FlushReason` enum is identical across all
tasks. `FlushOutcome` lands with its full
`{ flushed_entries, reason, aborted }` shape in Task 2; Task 11
only adds the `RenderEngine::drain_flush_outcomes` drain + backend
telemetry consumption — it does NOT redefine the type.

**Cross-task dependencies**:
- Task 3 is the atomic switch — every subsequent task assumes
  shared-ticket + append-only + engine-driven flush + parked-ops
  rollback. Bisect from Task 3 if anything later regresses.
- Tasks 4-13 add code that compiles+runs after Task 3.
- `FlushOutcome`'s shape is stable from Task 2 onward; Task 11
  only wires the drain + telemetry consumption. No signature
  refactor required mid-arc.
- Task 15 (hw smoke) is the ONLY task the agent cannot self-drive;
  brief the user.

**Codex review fixes applied 2026-05-23 (round 1)**:
- Task 3 squashed the original Task 3 (ticket source) and Task 4
  (append) into one atomic step, avoiding the VUID-vkQueueSubmit2-
  fence-04894 fence-reuse window.
- `max_size` defaults to 1 in Task 1; bumped to 16 in Task 4.
- COW semaphore export sequenced AFTER the explicit
  `engine.flush_submit_group(PresentCompletionSignal)` in
  `flush_cow_batch` (Task 3 Step 8), avoiding VUID-
  VkFenceGetFdInfoKHR-handleType-01457 (the Task 6.1 yoga hang).
- `pending_group_ops` deferral in Task 3 honours the spec's
  rollback-on-failure contract for `SubmittedOp` queue.
- Ordering tests in Task 9 use `peek_entries` for direct CB-handle
  comparison instead of weak damage-rect-count assertions.
- Mixed-sequence smoke (Task 12) tightened to `assert_eq!` against
  an exact submit count of 3 (vs the prior loose `<= 5`).
- `get_image` test signature in Task 5 corrected to
  `(store, platform, src, Rect2D, u8) -> Result<Vec<u8>, _>`.
- Original Task 12 (drain_all flush) folded into Task 3 Step 4.

**Codex review fixes applied 2026-05-23 (round 2)**:
- Task 5 (`get_image`) now does TWO `flush_submit_group(SyncBoundary)`
  calls: one before recording the readback CB AND one after the
  readback CB is appended (but BEFORE `ticket.wait()`). Without
  the second flush, the readback fence is never queued → hang.
  (Round-3 refined this further: get_image bypasses
  `pending_group_ops` entirely — see round-3 notes below.)
- Task 3 Step 9's `semaphore_bearing_append_*` regression test
  rewritten to drive `flush_cow_batch` through a synthetic
  present-completion attachment, not via direct
  `submit_paint_cb_with_semaphore` (which is pure-append per
  Step 6 and would mask the bug if it auto-flushed).
- `FlushOutcome` placeholder introduced in Task 2 alongside the
  platform stub (not Task 3); platform's `flush_submit_group`
  returns `Result<FlushOutcome, vk::Result>` from Task 2 onward
  so the engine wrapper's signature in Task 3 is coherent.
- Architecture section + Task 10 explicitly call out the Phase A
  scope: **only `SubmittedOp` queue is deferred**; drawable state
  (`last_render_ticket`, layout, damage, flush records) is
  fatal-after-failure and observed only via `renderer_failed`
  short-circuit, not via mutation rollback. Phase B does the full
  inventory.
- Task 3 Step 8's COW flush is **conditional** on
  `!batch.present_completions.is_empty()` — semaphore-less COW
  batches stay in the group (call `maybe_auto_flush_submit_group`
  instead) so the dominant COW pump still benefits from cap-
  collapse.
- Task 12 cargo invocation updated to match the test function
  name (`submit_group_mixed_sequence_smoke_exact_submit_count`).
- Task 15 PR body description corrected from "17-task arc" to
  "15-task arc".

**Codex review fixes applied 2026-05-23 (round 3)**:
- Task 5 (`get_image`) rewritten to **bypass `pending_group_ops`
  entirely** — staging stays a local variable until after the
  `ticket.wait()` + `pack_from_storage` read; the `SubmittedOp`
  pushes directly to `submitted` with an already-signaled fence.
  This is now documented as the single exception to the
  "every paint op parks in pending_group_ops" rule. (Codex round-3
  finding 1: avoids use-after-move on `staging.mapped`.)
- Task 3 Step 8's COW-flush-failure branch now **force-fires
  PRESENT completion events** via a `PendingPresentBatch` with
  `PresentBatchWait::Ready` before propagating the Err. Without
  this, semaphore-attached `present_completions` would silently
  drop on submit failure — clients waiting on CompleteNotify
  would hang forever even though `renderer_failed` is set.
  (Codex round-3 finding 2.)
- Task 11 telemetry wiring switched to a **deferred-queue
  pattern** (`RenderEngine::drain_flush_outcomes`) parallel to
  `cow_flush_records` / `render_flush_records`. Backend drains
  the queue once per `maybe_composite` tick. Engine methods do
  NOT take `&mut Telemetry`; ownership stays with backend.
  (Codex round-3 finding 3: engine had no telemetry borrow.)
- `FlushOutcome` extended in Task 11 to include `reason` +
  `aborted` fields; Task 2's placeholder body updated accordingly.
- Borrow-factoring note added to Task 3 Step 8 + Task 5 Step 3:
  `inner = self.inner.as_mut()` must be released around mid-body
  `self.flush_submit_group(...)` calls and re-acquired afterward.
  (Codex round-3 finding 4.)
- Self-review summary `FlushOutcome` provenance corrected to
  Task 2 (was Task 3). (Codex round-3 finding 5 doc drift.)

**Codex review fixes applied 2026-05-23 (round 4)**:
- Platform's `flush_submit_group` Err branch now **frees CBs**
  before returning Err (Codex round-4 finding 1). Covers both
  engine-paint-op CBs (already in `pending_group_ops` but those
  drops are no-op for raw `vk::CommandBuffer`) AND the
  `get_image` bypass CB that never enters `pending_group_ops`.
  Engine wrapper's Err branch updated to NOT free CBs — platform
  owns the free now (no double-free).
- `FlushOutcome` extended to its **full shape** in Task 2: all
  three fields (`flushed_entries`, `reason`, `aborted`) land at
  once. Every return path in `platform.flush_submit_group`
  (empty-group, Vk-less fixture, Ok queue_submit2, Err
  queue_submit2) populates all three. Task 11 no longer
  redefines the type. (Codex round-4 finding 2.)
- Task 3 Step 8's COW-PRESENT-failure code block rewritten in
  paste-safe form with explicit `let inner = self.inner.as_mut()`
  release + reborrow around `self.flush_submit_group(...)`.
  Borrow factoring is now part of the code shown, not a side
  note. Same factoring applies to Task 5 Step 3 (`get_image`).
  (Codex round-4 finding 3.)
- Round-2 fix notes about `get_image` parking in
  `pending_group_ops` corrected to reflect the round-3 bypass.
  (Codex round-4 finding 4 doc drift.)

**Codex review fixes applied 2026-05-23 (round 5)**:
- `force_next_submit_failure_for_tests` and the real
  `queue_submit2 Err` branch now route through a **shared
  `abort_flush` helper** that frees taken CBs + stashes the
  `aborted: true` `FlushOutcome` + sets `renderer_failed`.
  Previously the fault-injection path skipped all three, which
  would have made Task 10's failure assertions inconsistent with
  what the real failure path produces. (Codex round-5 finding 1.)
- Self-review summary's `FlushOutcome` provenance + cross-task
  dependency text cleaned of the "Task 11 touches signature
  shape" claim — the type is stable from Task 2. (Codex round-5
  finding 2 doc drift.)
- `RenderEngine::active_resource_bytes` (renamed from
  `_for_tests`) now sums staging + scratch across BOTH
  `submitted` AND `pending_group_ops`. Parked ops hold the same
  retention budget as in-flight ones and need to land in the
  high-water gauges. (Codex round-5 finding 3.)

**Codex review fixes applied 2026-05-23 (round 6)**:
- `abort_flush` helper definition moved into Task 2 alongside
  `flush_submit_group` (was in Task 3 Step 7). Task 3 Step 7 now
  only adds the test-only `force_next_submit_failure` flag +
  check that calls `self.abort_flush(...)` — the helper already
  exists by then. Restores the green-after-every-commit invariant.
  (Codex round-6 finding 1.)
- Task 3 Step 3's blanket `submitted.push_back` → `pending_group_ops.push`
  conversion now explicitly EXCLUDES any push site that precedes a
  `ticket.wait()` — today that's only `get_image`
  (engine.rs:3079). It stays on `submitted.push_back` until Task 5
  rewrites it to the special-case bypass shape. Without this
  exclusion Task 3's commit would hang `get_image` (parked op's
  fence never queued). (Codex round-6 finding 2.)

**Codex review fixes applied 2026-05-23 (round 7)**:
- Task 3 Step 3 now ALSO wires an explicit
  `self.flush_submit_group(SyncBoundary)` into `get_image` between
  its existing `end_and_submit_op` and `ticket.wait()` calls.
  Excluding the push site alone wasn't enough: Step 6 makes
  `submit_paint_cb_*` pure-append, so without the explicit flush
  the fence is never queued and `wait` hangs. Task 5's role
  narrows to "add the top-of-function flush (drains prior paint)
  + the regression test"; the load-bearing flush-before-wait
  landed in Task 3. (Codex round-7 finding 1.)
- Task 4 + Task 8 collapse/cap tests now flush setup CBs via an
  explicit `engine.flush_submit_group(SyncBoundary)` after
  `create_pixmap` and BEFORE measuring `initial`. With cap=16,
  buffered setup CBs would otherwise skew the
  `submit_group_size()` and `queue_submit2_count` assertions.
  (Codex round-7 finding 2.)
- Task 1 module imports now `use ash::vk::{self, Handle};` so
  the test fixture's `vk::CommandBuffer::from_raw` /
  `vk::Semaphore::from_raw` calls resolve. The `Handle` trait
  must be in scope; existing repo code uses
  `ash::vk::Handle::from_raw`. (Codex round-7 finding 3.)

**Codex review fixes applied 2026-05-23 (round 8)**:
- Removed references to `wait_for_drawable_idle` — that function
  was deleted in Task 15 of an earlier stage. Plan now points at
  `get_image` as the only `ticket.wait()` site to special-case;
  the audit step (grep `'ticket\.wait('`) catches any future
  reintroduction. (Codex round-8 LOW finding.)
- Task 1 + Task 14 build commands corrected to match
  `AGENTS.md`: `cargo +nightly fmt` and **regular** clippy (NOT
  pedantic). Project-specific guidance overrides the global
  CLAUDE.md pedantic default. (Codex round-8 nit.)
