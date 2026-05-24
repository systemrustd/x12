# Frame-builder Phase B.1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a `FrameBuilder` on `RenderEngine`, port `composite_glyphs` so that one `composite_glyphs` call's glyph uploads + draw collapse into a single primary command buffer with exactly one `vkQueueSubmit2`, while every other paint op temporarily reverts to a one-CB-per-submit cadence (SubmitGroup `max_size = 1`). This sub-phase IS the bee MATE-load fix: the RDNA2/RADV `composite_glyphs` GPUVM fault site stops emitting multi-CB-per-submit work, and all other paint ops on bee fall back to the pre-Phase-A single-submit shape that already boots MATE cleanly on the `cap=1` row of the 2026-05-23 capture.

**Architecture:** A new `FrameBuilder` field on `RenderEngineInner` owns the `Closed` ↔ `OpenForPaint` lifecycle. Paint entry points opt into it (via `YSERVER_FRAME_BUILDER`, defaults on) by appending `RecordedOp` enum entries plus pinned `Arc<StagingBuffer>` clones, drawable-layout-overlay snapshots, atlas-layout-overlay snapshots, and pending glyph-cache inserts into the frame. At close (driven by Invariant M3 in `maybe_composite`, Invariant M2 in every non-ported paint op, the existing get_image sync, the existing PRESENT-completion semaphore site, shutdown, a 16 ms timeout, or a per-frame pin-set ceiling) the frame replays its op list as one primary CB recorded via `begin_op_cb` + `end_and_submit_op` (which lands as the sole entry in the SubmitGroup; cap=1 auto-flushes immediately) and parks its pin set on a `pending_frames` queue gated by the same `FenceTicket` the submission used. Close-success commits the layout overlay back into `Drawable::storage.current_layout` + atlas layout, sets each touched drawable's `last_render_ticket` to the frame ticket, commits pending glyph inserts, and sets the atlas's new `last_render_ticket`. Close-failure rolls everything back and sets `renderer_failed` (the same fatal discipline Phase A already enforces). All other paint entry points (`fill_rect`, `fill_rect_batch`, `logic_fill`, `copy_area`, `cow_copy_area`, `put_image`, `image_text`, `render_composite`, `render_fill_rectangles`, `render_traps_or_tris`) still record their own CB via the Phase A `pending_group_ops` path, but with `max_size = 1` set unconditionally at platform open (Invariant M1) so the SubmitGroup never carries > 1 CB during the B.1 ↔ B.4 sub-phase window.

**Tech Stack:** Rust, `ash` Vulkan bindings, existing v2 `FenceTicket` / `OpsCommandPool` / `SubmitGroup` / `DescriptorPoolRing` / `DrawableStore` / `V2GlyphAtlas` / `RenderEngine` infrastructure, existing v2 telemetry + submit-trace plumbing.

**Reference docs:**
- Phase B spec: `docs/superpowers/specs/2026-05-24-frame-builder-phase-b-design.md`
- Phase A spec: `docs/superpowers/specs/2026-05-23-frame-builder-submit-rate-design.md`
- Phase A close-out plan: `docs/superpowers/plans/2026-05-23-frame-builder-submit-rate-phase-a.md`
- Phase A status closeout: `docs/status.md` § "Phase A — CLOSED 2026-05-24"
- bee 2026-05-23 fault capture: `docs/status.md` § "2026-05-23 bee MATE-load freeze"
- Task 6.1 Arc-pinning precedent: `docs/superpowers/specs/2026-05-23-deferred-present-completion-design.md`

**File structure (locked in before tasks):**
- **Create**: `crates/yserver/src/kms/v2/frame_builder.rs` — `FrameBuilder` struct + `CloseReason` enum + `RecordedOp` enum + `RecordedCompositeGlyphs` / `RecordedGlyphUpload` / `RecordedLayoutTransition` payloads + `FramePinSet` + `FrameLayoutTable` + `FrameSubmittedRecord` + no-Vk unit tests. Plus public introspection helpers (`#[cfg(test)] peek_ops`, `is_open`, `op_count`, `pin_count`).
- **Modify**: `crates/yserver/src/kms/v2/mod.rs` — declare `pub(crate) mod frame_builder;` alongside `submit_group`.
- **Modify**: `crates/yserver/src/kms/v2/glyph_atlas.rs:134-146` — add `last_render_ticket: Option<FenceTicket>` field; gate atlas destruction at backend shutdown on that ticket. Keep `record_upload` API surface untouched (frame builder calls the same recording method against the frame CB).
- **Modify**: `crates/yserver/src/kms/v2/engine.rs` —
  - Embed `frame_builder: FrameBuilder` on `RenderEngineInner` (next to `pending_group_ops`).
  - Add `pending_frames: VecDeque<FrameSubmittedRecord>` parallel to `submitted`; walk in `poll_retired` + `drain_all`.
  - Refactor `composite_glyphs` (engine.rs:3736-4039) to branch on `frame_builder_enabled()` into a new private `composite_glyphs_via_frame_builder` path; legacy path stays as the off branch for safe rollback.
  - Add `close_open_frame_for_non_ported_op` helper called at the top of `fill_rect` / `fill_rect_batch` / `logic_fill` / `copy_area` / `cow_copy_area` / `put_image` / `image_text` / `render_composite` / `render_fill_rectangles` / `render_traps_or_tris` (the 10 paint entry points other than `composite_glyphs` and `get_image`).
  - Add `close_open_frame_for_sync_wait` called from the existing `get_image` flushpoint.
  - Add `close_open_frame_for_present_completion` called from the existing PRESENT-completion-semaphore submit site in `attach_cow_present_completion`.
  - Add `close_open_frame_for_timeout` called from `maybe_composite`'s top once per tick (16 ms default; env knob `YSERVER_FRAME_BUILDER_TIMEOUT_MS`).
  - Add `frame_builder_close_count_for_tests`, `frame_builder_open_for_tests`, `frame_builder_pin_count_for_tests` introspection used by integration tests.
- **Modify**: `crates/yserver/src/kms/v2/platform.rs:1442-1448` — replace the test-only `submit_group_set_max_size_for_tests` invocation pattern with an unconditional `submit_group.set_max_size(1)` call at platform open (`PlatformBackend::open_with_commit` or `PlatformBackend::new`, whichever owns submit-group construction today). Phase A's `set_max_size_for_tests` stays as a separate cfg-test override; production sets 1 unconditionally during B.1–B.4.
- **Modify**: `crates/yserver/src/kms/v2/backend.rs:4574-4689` (`maybe_composite`) — call `engine.close_open_frame_for_scene_compose(...)` before the existing `flush_submit_group(FlushReason::SceneCompose)` call (Invariant M3). Call `engine.close_open_frame_for_timeout(...)` once at the top of `maybe_composite`. Wire shutdown frame close into `KmsBackendV2::shutdown` (find the existing engine `drain_all` call site).
- **Modify**: `crates/yserver/src/kms/v2/telemetry.rs` — new counters: `frame_builder_opens`, `frame_builder_closes`, per-close-reason buckets (`scene_compose`, `non_ported_paint_op`, `legacy_sc_compose`, `present_completion_signal`, `sync_wait`, `timeout`, `shutdown`, `pin_ceiling`), `frame_builder_ops_per_frame_total` + `_max_in_window` + `_hist[8]` (buckets 1, 2-3, 4-7, 8-15, 16-31, 32-63, 64-127, 128+), `frame_builder_active_pins_high_water`, `frame_builder_aborts`, `frame_builder_glyph_uploads_per_frame_max` + `_total`. Emit at the existing 1 Hz cadence inside `maybe_emit`.
- **Modify**: `crates/yserver/tests/v2_acceptance.rs` — three new integration tests (`v2_frame_builder_composite_glyphs_one_submit`, `v2_frame_builder_renderer_failed_on_submit_failure`, `v2_frame_builder_mixed_sequence_smoke`) plus the small wrapper additions on `KmsBackendV2` they need.
- **Modify**: `docs/status.md` — Phase B.1 status entry + bee hardware-smoke gate placeholder.

**Phased rollout choice.** Tasks are ordered so HEAD stays green and `KmsBackendV2` is runnable after every commit, AND so the bee fix lands as the SINGLE last behavioural flip. The structure:

- **Tasks 1–9** build new abstractions + retirement infrastructure in isolation. No engine paint path uses them yet. `YSERVER_FRAME_BUILDER` defaults OFF; production paint uses the legacy path; bee remains broken.
- **Task 10** sets `SubmitGroup::max_size = 1` by changing the *type-level default* in `SubmitGroup::new()` from 16 → 1. This is the first observable behaviour change (other platforms see a perf regression here; bee survives at cap=1 per the 2026-05-23 capture). M1 is centralised in one place; the legacy `YSERVER_PAINT_SUBMIT_GROUP_CAP` env override that allowed `> 1` is disabled for B.1–B.4.
- **Tasks 11–12** wire the engine close path (`RenderEngine::close_open_frame`) but don't call it from anywhere yet. Engine compiles + runs unchanged for clients.
- **Tasks 13–14** wire Invariants M3 and M2 — but with `YSERVER_FRAME_BUILDER` still OFF, the frame is never open, so the new close-frame calls are no-ops. Bisect-safe.
- **Tasks 15–20** port `composite_glyphs` and wire each remaining close trigger (PRESENT-completion at the REAL site `enqueue_present_completion` in `backend.rs:9413`, `get_image` sync, timeout, shutdown, pin-ceiling). Gate stays OFF throughout. The implementation lives on the OFF branch; tests flip it ON locally via `set_frame_builder_enabled_for_tests`.
- **Task 21** wires telemetry.
- **Tasks 22–23** are integration tests (renderer_failed rollback, mixed-sequence smoke).
- **Task 24** is the single behavioural flip: `YSERVER_FRAME_BUILDER` default ON. This is the commit that fixes bee.
- **Task 25** is lint + format.
- **Task 26** is the bee hardware gate (user-driven) + status doc update.

Any bisect that lands on a regression in B.1's commit range narrows to: "the bee fix activates here" (Task 24) vs "this close trigger was wired wrong" (whichever task added it). Task 10 (cap=1) is the only off-the-shelf perf-regression commit before Task 24.

---

## Invariant inventory (load-bearing — every task that adds code must respect)

- **M1 (sub-phase B.1–B.4).** `SubmitGroup::max_size == 1` unconditionally; no paint path may rely on multi-CB packing inside one `vkQueueSubmit2`. The cap retires at B.5 with the SubmitGroup itself.
- **M2 (sub-phase B.1–B.3).** Every paint entry point that has NOT been ported to the FrameBuilder closes the open frame BEFORE recording its own CB. The 10 entry points in scope for B.1's M2 wiring: `fill_rect`, `fill_rect_batch`, `logic_fill`, `copy_area`, `cow_copy_area`, `put_image`, `image_text`, `render_composite`, `render_fill_rectangles`, `render_traps_or_tris`. (`composite_glyphs` is the *only* paint op ported in B.1, so M2 fires for everything else.)
- **M3 (sub-phase B.1–B.3).** Legacy scene compose closes the open frame BEFORE recording its own CB. Implemented inside `maybe_composite`.
- **Drawable ticket-touch.** Every `RecordedOp` append that reads or writes a `DrawableId` calls `store.touch_render_fence(id, frame_ticket.clone())` AND snapshots that drawable's pre-frame `last_render_ticket` into `touched_drawables` if this is the first touch in the frame. Close-success leaves the frame ticket in place; close-failure restores the pre-frame ticket. Mirrors the Phase A `engine.rs:2862` discipline (the 2026-05-22 Rembrandt UAF fix) lifted from "one batch" → "one frame".
- **Atlas ticket-touch.** Every frame that records a `RecordedOp::GlyphUpload` or `RecordedOp::CompositeGlyphs` snapshots `V2GlyphAtlas::last_render_ticket` once (first-touch-in-frame) into the frame's `touched_atlas_prev_ticket: Option<Option<FenceTicket>>` slot. Close-success sets `V2GlyphAtlas::last_render_ticket = Some(frame_ticket.clone())`; close-failure restores the snapshot.
- **renderer_failed fatal-after-failure.** Inherited from Phase A. Frame-close-failure sets `platform.renderer_failed = true` (or relies on `abort_flush` setting it on the platform side) after the rollback walk; subsequent paint entry-point invocations short-circuit on the existing gate.

---

## Close-path correctness pattern (load-bearing — every Task 12 / Task 15 sketch must follow)

Codex review surfaced three structural pitfalls. The plan codifies their avoidance here so Task 12 and Task 15 sketches can stay terse without losing correctness.

### Pitfall 1 — Commit happens AFTER `vkQueueSubmit2` accepts, not after CB append

`end_and_submit_op` only ends a CB and appends it to the SubmitGroup; the real `vkQueueSubmit2` happens later inside `PlatformBackend::flush_submit_group` (`platform.rs:1476`). The frame-builder's pin-set push + overlay commits + atlas-cache commits MUST happen AFTER that flush returns Ok. If the flush returns Err, the platform's `abort_flush` (`platform.rs:1563`) frees the CBs, sets `renderer_failed = true`, and the engine's existing `flush_submit_group` (`engine.rs:759-794`) clears `pending_group_ops` — but `pending_frames` and committed overlays would remain leaked unless the close path holds them locally until after submit success.

**Mandated ordering inside `close_open_frame`:**

1. Take the open frame from `FrameBuilder` (local owner — `Box<OpenFrame>`); on failure later, drop this local.
2. Allocate one CB via `begin_op_cb`. Record all ops into it via the 3-pass walk, threading layouts via the overlay (see § "Pitfall 3"). On a record-pass error: free the CB manually (`device.free_command_buffers(pool, &[cb])`); set `platform.renderer_failed = true`; drop the local frame (pin set drops, Arcs decrement, overlays evaporate); return Err. CB was never appended to the SubmitGroup → no double-free.
3. Call `end_and_submit_op(inner, platform, cb, &frame_ticket)?`. On failure: same as step 2 (CB never reached the SubmitGroup because `end_and_submit_op` calls `vk.device.end_command_buffer` then `platform.submit_paint_cb_with_semaphore` which appends; if the *end* fails, the CB hasn't reached the group; if the *append* fails it's a platform-level error).
4. Push `SubmittedOp { cb, ticket: frame_ticket.clone(), staging: None, scratch: None, atlas_ticket: None, generation }` onto `pending_group_ops`. (Phase A discipline: the SubmittedOp's resource fields stay None — pins live on the FrameSubmittedRecord.)
5. **Release the `inner` mutable borrow** by closing its lexical scope. (Avoid `drop(inner)` — clippy can warn on dropping a reference; use a tight `{ ... }` block instead, or rebind with `let _ = inner;` to signal "borrow ends here".)
6. Call `self.flush_submit_group(platform, FlushReason::FrameBuilder)?` (this is `RenderEngine::flush_submit_group`, the engine-side wrapper that drives platform + drains `pending_group_ops` into `submitted` on Ok / clears on Err).
7. Match the flush result:
   - **Ok:** re-borrow inner (`let inner = self.inner.as_mut().expect("re-acquired")`). Push the FrameSubmittedRecord onto `pending_frames`. Commit overlays via helper. Commit atlas pending inserts. Call `inner.frame_builder.complete_close_success()`. Return `CloseOutcome::Submitted { … }`.
   - **Err:** re-borrow inner. Platform already freed the CB inside `abort_flush` (do NOT free it ourselves). Platform already set `renderer_failed = true`. Drop the local OpenFrame (pins, overlays, pending atlas inserts — all evaporate, restoring nothing in storage because storage was snapshotted at append-time, not mutated). Call `inner.frame_builder.complete_close_failure()`. Return Err.

This pattern avoids both the double-free and the commit-before-failure.

### Pitfall 2 — Layout-state snapshot vs overlay-as-source-of-truth

The spec § "Transactional layout state" describes an overlay where the recorder reads/writes the overlay and storage is committed only on close-success. That is the structurally cleanest model. **B.1 takes a simpler equivalent** that the spec § "Transactional layout state" explicitly allows (the "best-effort restoration" trade-off bullet):

- **Append-time:** at the FIRST op-in-frame that touches a drawable or the atlas, the frame builder calls `layouts.first_touch_drawable(id, drawable.storage.current_layout)` AND `layouts.first_touch_atlas(atlas.current_layout)` — capturing the pre-frame layout into the overlay. The overlay's `current_in_frame` field is set to the same value initially.
- **Close-time record pass:** the recorder (today's `record_text_run_scissored` via `StorageTextTarget`; today's `V2GlyphAtlas::record_upload`) mutates `drawable.storage.current_layout` and `V2GlyphAtlas::current_layout` IN PLACE during recording. The overlay's `current_in_frame` is updated at the end of each op to mirror storage's new value (purely for diagnostic / future-B.2 use; B.1 doesn't read it).
- **Close-success:** no-op (storage already holds the recorder's final values).
- **Close-failure:** before propagating Err, the rollback walk writes `overlay.pre_frame_layout` back into `drawable.storage.current_layout` and `V2GlyphAtlas::current_layout`. Because `renderer_failed` is set, every subsequent paint entry short-circuits; only diagnostic / shutdown readers (e.g. `get_image`) see the restored values. This is the "best-effort restoration" path the spec allows.

This means Task 4 (FrameLayoutTable) lands the overlay machinery as designed, but in B.1 the overlay's role is exclusively *snapshot for rollback*. B.2 may flip to overlay-as-source-of-truth when porting `render_composite` if that path needs cross-op-in-frame layout state. The plan's commit/rollback helpers (Task 12) are written to handle either model — they read from the overlay for rollback values; commit-on-success is a no-op write today and will become the actual storage-write when B.2 needs it.

### Pitfall 3 — Borrowck pattern for engine close paths

`self.<method>(...)` (`&mut self`-taking) and `let inner = self.inner.as_mut()` (mutable borrow of a field) cannot coexist. The close path needs both. The fix throughout `close_open_frame` (Task 12) and `composite_glyphs_via_frame_builder` (Task 15):

```rust
// Wrong (won't compile):
let Some(inner) = self.inner.as_mut() else { return … };
… use inner …
self.maybe_auto_flush_submit_group(platform)?;   // ← second &mut self borrow while `inner` lives
inner = self.inner.as_mut().expect("re-acquired"); // ← can't reassign let binding

// Right (lexical scopes):
{
    let inner = self.inner.as_mut().expect("inner");
    … use inner only inside this block …
}  // ← inner borrow released here
self.flush_submit_group(platform, FlushReason::FrameBuilder)?;  // ← second &mut self call now ok
{
    let inner = self.inner.as_mut().expect("inner");
    … use inner again …
}
```

Apply this pattern uniformly. Task 12's body breaks into 3-4 scoped blocks; the OpenFrame (local owner) and the cb (local handle) survive across blocks because they don't borrow self.

For Task 15's pin-ceiling branch: hoist the ceiling check OUT of the per-glyph loop. Either:
(a) Pre-walk: compute the maximum needed pins, close+reopen ONCE if the pre-walk would exceed the ceiling.
(b) Per-glyph: collect ops + pin-staging-buffer Arc clones into local `Vec`s during the loop (no `inner` borrow held across iterations), then commit them all to the frame at the end. Pin-ceiling check happens before commit.

Pick (b) for B.1: simpler, no double-walk over the glyph slice. The cost (one extra `Vec<Arc<...>>` allocation) is bounded by the glyph count of the call.

---

### Task 1: `frame_builder.rs` module skeleton + `CloseReason` enum + state machine (no Vk)

**Files:**
- Create: `crates/yserver/src/kms/v2/frame_builder.rs`
- Modify: `crates/yserver/src/kms/v2/mod.rs:10-22` (add `pub(crate) mod frame_builder;` after the `submit_group` line)

- [ ] **Step 1: Add the module declaration**

```rust
// crates/yserver/src/kms/v2/mod.rs
mod backend;
pub(crate) mod cursor;
pub(crate) mod descriptor_pool_ring;
pub(crate) mod engine;
pub(crate) mod frame_builder;            // ← new line
pub(crate) mod glyph_atlas;
pub(crate) mod owned_semaphore;
pub(crate) mod platform;
pub(crate) mod present_completion;
pub(crate) mod scene;
pub(crate) mod store;
pub(crate) mod submit_group;
pub(crate) mod submit_trace;
pub(crate) mod telemetry;
```

- [ ] **Step 2: Write the failing unit tests in `frame_builder.rs`**

```rust
//! Stage 5 frame-builder Phase B sub-phase B.1: deferred per-frame
//! op-list recording.
//!
//! `FrameBuilder` owns a `Closed ↔ OpenForPaint` lifecycle. Paint
//! entry points that have been ported (`composite_glyphs` in B.1)
//! append `RecordedOp`s; a close trigger (Invariant M2 / M3, the
//! existing get_image / PRESENT-completion sync points, a timeout,
//! shutdown, or a pin-set ceiling) replays the op list as ONE primary
//! command buffer, submits it via the SubmitGroup (cap=1, so the
//! submit auto-flushes immediately), and parks the frame's resource
//! pins on a `pending_frames` queue gated by the submit's
//! `FenceTicket`.
//!
//! Phase B spec — `docs/superpowers/specs/2026-05-24-frame-builder-phase-b-design.md`.
//! This file holds the no-Vk-required pieces (state machine, op enum,
//! pin sets, layout overlay); the recording side lives in
//! `engine.rs::FrameBuilder::close_into_cb_*` because it needs the
//! engine's CB pool + atlas + drawable-store access.

use ash::vk;

use super::glyph_atlas::{AtlasEntry, GlyphKey};
use super::platform::FenceTicket;
use super::store::DrawableId;

/// Why a frame closed. Bumped into telemetry on every close so the
/// rollout can see which trigger is dominating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseReason {
    /// `maybe_composite` saw a ready output + dirty scene; the frame
    /// closes paint-only (compose stays separate in B.1 — folded into
    /// the frame at B.4).
    SceneCompose,
    /// Invariant M2: a non-ported paint op is about to record its own
    /// CB; close the frame first so the non-ported op sees committed
    /// `Drawable::storage.current_layout` + `last_render_ticket`.
    NonPortedPaintOp,
    /// Invariant M3: legacy scene compose is about to record; close
    /// the frame first for the same reason as M2.
    LegacyScCompose,
    /// COW PRESENT-completion semaphore got attached; the frame must
    /// close immediately so `vkGetSemaphoreFdKHR(SYNC_FD)` sees a
    /// queued signal-op (Task 6.1 yoga hang precedent).
    PresentCompletionSignal,
    /// `get_image` is about to wait on a fence; close the frame first
    /// so the readback's `ticket.wait()` observes a submitted CB.
    SyncWait,
    /// Idle / no-pageflip case. A frame open > T ms forces close to
    /// release pinned resources.
    Timeout,
    /// `KmsBackendV2::shutdown` is tearing down platform state.
    Shutdown,
    /// `max_pinned_resources_per_frame` ceiling hit (1024 default).
    PinCeiling,
}

/// FrameBuilder lifecycle. `Closed` is the hot path for X11 traffic
/// that doesn't touch the paint surface (event-only requests, idle).
/// `OpenForPaint` is where every recorded op accumulates between
/// the first paint and a close trigger.
///
/// Phase B's spec sketches a third state, `ClosingWithCompose`, for
/// when scene compose joins the frame. That state lands in sub-phase
/// B.4; B.1 only carries the two-state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameState {
    Closed,
    OpenForPaint,
}

#[derive(Debug)]
pub(crate) struct FrameBuilder {
    state: FrameState,
    /// `None` while `Closed`; `Some(open)` while `OpenForPaint`. The
    /// inner `OpenFrame` is the per-frame bookkeeping (op list, pins,
    /// layout overlay, touched-drawables, atlas-prev-ticket, etc.).
    /// Boxed so the FrameBuilder's stable size doesn't bloat
    /// `RenderEngineInner` when no frame is open.
    open: Option<Box<OpenFrame>>,
    /// Lifetime open count; bumped on every `Closed → OpenForPaint`.
    /// Tests use this; telemetry mirrors it.
    lifetime_opens: u64,
    /// Lifetime close count; bumped on every `OpenForPaint → Closed`.
    lifetime_closes: u64,
    /// Per-frame ceiling: maximum number of pinned resources before a
    /// pin-ceiling close fires. 1024 default.
    max_pinned_resources_per_frame: usize,
}

impl FrameBuilder {
    pub(crate) fn new() -> Self {
        Self {
            state: FrameState::Closed,
            open: None,
            lifetime_opens: 0,
            lifetime_closes: 0,
            max_pinned_resources_per_frame: 1024,
        }
    }

    pub(crate) fn state(&self) -> FrameState {
        self.state
    }

    pub(crate) fn is_open(&self) -> bool {
        matches!(self.state, FrameState::OpenForPaint)
    }

    pub(crate) fn lifetime_opens(&self) -> u64 {
        self.lifetime_opens
    }

    pub(crate) fn lifetime_closes(&self) -> u64 {
        self.lifetime_closes
    }

    pub(crate) fn set_max_pinned_resources_per_frame(&mut self, n: usize) {
        // Min 1: ceiling=0 would make the first append always close.
        // Tests override; production stays at 1024.
        self.max_pinned_resources_per_frame = n.max(1);
    }

    pub(crate) fn max_pinned_resources_per_frame(&self) -> usize {
        self.max_pinned_resources_per_frame
    }
}

/// Per-frame bookkeeping. Allocated when `Closed → OpenForPaint` fires;
/// dropped on close (success: contents move into `FrameSubmittedRecord`;
/// failure: dropped immediately).
#[derive(Debug)]
pub(crate) struct OpenFrame {
    /// The shared `FenceTicket` for every op in this frame. Acquired
    /// from `PlatformBackend::submit_group_ticket_or_open()` at frame
    /// open. Cloned into every `touched_drawables` entry on commit.
    pub(crate) ticket: FenceTicket,
    pub(crate) ops: Vec<RecordedOp>,
    pub(crate) close_reason_on_open: Option<CloseReason>, // unused in B.1; reserved for B.4
}

#[cfg(test)]
mod state_tests {
    use super::*;

    #[test]
    fn fresh_frame_builder_is_closed_with_no_lifetime_counts() {
        let fb = FrameBuilder::new();
        assert_eq!(fb.state(), FrameState::Closed);
        assert!(!fb.is_open());
        assert_eq!(fb.lifetime_opens(), 0);
        assert_eq!(fb.lifetime_closes(), 0);
    }

    #[test]
    fn default_pin_ceiling_is_1024() {
        let fb = FrameBuilder::new();
        assert_eq!(fb.max_pinned_resources_per_frame(), 1024);
    }

    #[test]
    fn set_max_pinned_resources_clamps_to_at_least_one() {
        let mut fb = FrameBuilder::new();
        fb.set_max_pinned_resources_per_frame(0);
        assert_eq!(fb.max_pinned_resources_per_frame(), 1);
        fb.set_max_pinned_resources_per_frame(42);
        assert_eq!(fb.max_pinned_resources_per_frame(), 42);
    }

    #[test]
    fn close_reason_has_eight_variants_for_b1() {
        // Compile-time guard: if a new variant slips in, the
        // exhaustive match below stops compiling. The eight in B.1
        // map 1:1 to the close triggers in § "Frame close triggers"
        // of the spec.
        fn _exhaustive(r: CloseReason) -> &'static str {
            match r {
                CloseReason::SceneCompose => "scene_compose",
                CloseReason::NonPortedPaintOp => "non_ported_paint_op",
                CloseReason::LegacyScCompose => "legacy_sc_compose",
                CloseReason::PresentCompletionSignal => "present_completion_signal",
                CloseReason::SyncWait => "sync_wait",
                CloseReason::Timeout => "timeout",
                CloseReason::Shutdown => "shutdown",
                CloseReason::PinCeiling => "pin_ceiling",
            }
        }
        assert_eq!(_exhaustive(CloseReason::SceneCompose), "scene_compose");
    }
}

// The rest of this module — RecordedOp, FramePinSet, FrameLayoutTable,
// FrameSubmittedRecord — lands in subsequent tasks.
```

- [ ] **Step 3: Verify the failing tests fail (module doesn't compile yet because `glyph_atlas::{AtlasEntry, GlyphKey}` import is unused — strip it for now, restore in Task 3)**

```bash
cargo test -p yserver --lib kms::v2::frame_builder
```
Expected: PASS (the three small state-machine tests succeed; the imports for `AtlasEntry`/`GlyphKey` are dead — remove them from the use line for this task, re-add in Task 3 when `RecordedOp` lands).

- [ ] **Step 4: Strip the unused imports**

Remove `use super::glyph_atlas::{AtlasEntry, GlyphKey};` and `use super::store::DrawableId;` for now. They come back in Task 3.

- [ ] **Step 5: Confirm green**

```bash
cargo build -p yserver
cargo test -p yserver --lib kms::v2::frame_builder
```
Both: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver/src/kms/v2/mod.rs crates/yserver/src/kms/v2/frame_builder.rs
git commit -m "feat(v2/frame_builder): module skeleton + CloseReason + FrameBuilder lifecycle (Phase B.1 Task 1)"
```

---

### Task 2: `RecordedOp` enum + `RecordedCompositeGlyphs` / `RecordedGlyphUpload` / `RecordedLayoutTransition` payloads

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs`

The B.1 scope only needs three op variants (composite_glyphs is the sole ported entry point). B.2 adds `RenderComposite` + `RenderFill`; B.3 adds the remaining variants; the spec calls these out in § "Op representation".

- [ ] **Step 1: Write the failing unit tests**

```rust
// Append at the bottom of frame_builder.rs

use super::glyph_atlas::{AtlasEntry, GlyphKey};
use super::store::DrawableId;

/// Index into `OpenFrame::pins.staging_buffers`. Saved on `RecordedOp`
/// payloads so close-time replay can fetch the right pinned buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PinnedStagingIdx(pub(crate) u32);

/// A glyph to draw at frame-close time. Mirrors the in-tree
/// `TextGlyph` struct (`crate::kms::vk::ops::text::TextGlyph`); we hold
/// our own copy here so the recorded op is independent of the live
/// `TextGlyph` type (which the recorder consumes by reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecordedTextGlyph {
    pub(crate) atlas_x: u32,
    pub(crate) atlas_y: u32,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) dst_x: i32,
    pub(crate) dst_y: i32,
}

#[derive(Debug)]
pub(crate) struct RecordedCompositeGlyphs {
    pub(crate) dst_id: DrawableId,
    pub(crate) foreground_rgba: [f32; 4],
    pub(crate) glyphs: Vec<RecordedTextGlyph>,
    pub(crate) clip_scissors: Vec<vk::Rect2D>,
    /// Damage rect to commit on close-success. Pre-computed at append
    /// time (today's `composite_glyphs` already computes the same
    /// bbox at engine.rs:3913-3922) so close-time doesn't have to
    /// re-walk the glyph list.
    pub(crate) damage_rect: Option<vk::Rect2D>,
}

#[derive(Debug)]
pub(crate) struct RecordedGlyphUpload {
    /// Pin index into the frame's staging-buffer pin vector. Replay
    /// reads the buffer handle from the pinned Arc.
    pub(crate) staging_pin_idx: PinnedStagingIdx,
    pub(crate) atlas_x: u32,
    pub(crate) atlas_y: u32,
    pub(crate) w: u32,
    pub(crate) h: u32,
    /// Cache-insert pair to commit on close-success (atlas's lookup
    /// becomes hit-able by this key after the frame ticket signals,
    /// but the cache entry is committed in the engine on close-success
    /// — the spec's "transactional cache insert" discipline).
    pub(crate) insert_key: GlyphKey,
    pub(crate) insert_entry: AtlasEntry,
}

/// Reserved for future ops that need an explicit cross-frame layout
/// transition. `composite_glyphs` doesn't emit any in B.1 (the text
/// pipeline's recorder embeds its own barriers via the per-call
/// `StorageTextTarget` adapter), but the variant exists so the
/// recorder skeleton in Task 11 can match exhaustively and B.2 can
/// fold ported `render_composite` / `render_fill` paths in without
/// touching this enum's variant set.
#[derive(Debug)]
pub(crate) struct RecordedLayoutTransition {
    pub(crate) drawable_id: DrawableId,
    pub(crate) src_stage: vk::PipelineStageFlags2,
    pub(crate) src_access: vk::AccessFlags2,
    pub(crate) dst_stage: vk::PipelineStageFlags2,
    pub(crate) dst_access: vk::AccessFlags2,
    pub(crate) target_layout: vk::ImageLayout,
}

#[derive(Debug)]
pub(crate) enum RecordedOp {
    CompositeGlyphs(RecordedCompositeGlyphs),
    GlyphUpload(RecordedGlyphUpload),
    LayoutTransition(RecordedLayoutTransition),
}

#[cfg(test)]
mod op_tests {
    use super::*;

    #[test]
    fn recorded_op_size_is_under_256_bytes() {
        // Spec § "Open questions for the implementation plan" item 1
        // asks the plan to profile op size after the structure lands.
        // 256 B is a generous initial ceiling: the largest B.1 variant
        // is `CompositeGlyphs` which contains two `Vec<...>` (24 B
        // each on 64-bit) + a [f32;4] + a DrawableId + an
        // Option<Rect2D>. If this fails at any point during B.1 it
        // means a variant grew unexpectedly — investigate before
        // committing. Plan picks Box<...> wrapping if a future B.2/B.3
        // variant blows past 256 B.
        assert!(
            std::mem::size_of::<RecordedOp>() <= 256,
            "RecordedOp grew to {} bytes — investigate before committing",
            std::mem::size_of::<RecordedOp>(),
        );
    }

    #[test]
    fn recorded_composite_glyphs_carries_dst_glyph_list_and_clip() {
        let op = RecordedCompositeGlyphs {
            dst_id: DrawableId(1),
            foreground_rgba: [1.0, 0.5, 0.25, 1.0],
            glyphs: vec![RecordedTextGlyph {
                atlas_x: 0,
                atlas_y: 0,
                w: 8,
                h: 12,
                dst_x: 100,
                dst_y: 200,
            }],
            clip_scissors: vec![vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: vk::Extent2D { width: 1280, height: 720 },
            }],
            damage_rect: None,
        };
        assert_eq!(op.glyphs.len(), 1);
        assert_eq!(op.glyphs[0].dst_x, 100);
        assert_eq!(op.clip_scissors[0].extent.width, 1280);
    }

    #[test]
    fn recorded_glyph_upload_carries_staging_index_and_pending_insert() {
        let up = RecordedGlyphUpload {
            staging_pin_idx: PinnedStagingIdx(0),
            atlas_x: 16,
            atlas_y: 32,
            w: 12,
            h: 18,
            insert_key: GlyphKey { font_xid: 1234, codepoint: 65 },
            insert_entry: AtlasEntry {
                atlas_x: 16,
                atlas_y: 32,
                w: 12,
                h: 18,
                pen_left: 0,
                pen_top: 0,
            },
        };
        assert_eq!(up.staging_pin_idx.0, 0);
        assert_eq!(up.insert_entry.w, 12);
    }
}
```

(`DrawableId` is `pub(crate) struct DrawableId(pub(crate) u64)` per `crates/yserver/src/kms/v2/store.rs`; the literal `DrawableId(1)` matches the existing tuple-struct shape. Confirm before the test runs by reading `store.rs` around line 580.)

- [ ] **Step 2: Run the new tests; expect failure on import resolution**

```bash
cargo test -p yserver --lib kms::v2::frame_builder
```
Expected: FAIL — `AtlasEntry` and `GlyphKey` are imported but the test refers to them and the `RecordedOp` enum without their public re-export being available. Verify the error is the expected resolution error.

- [ ] **Step 3: Confirm `GlyphKey` + `AtlasEntry` are `pub(crate)`**

Check `crates/yserver/src/kms/v2/glyph_atlas.rs` near line 40-70 — both types should already be `pub(crate)`. If `GlyphKey`'s constructor fields aren't accessible, expose them (`#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]` plus `pub(crate)` on the struct's fields).

- [ ] **Step 4: Re-run tests until green**

```bash
cargo test -p yserver --lib kms::v2::frame_builder
```
Expected: PASS, all three tests green; `recorded_op_size_is_under_256_bytes` printing nothing unusual.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/frame_builder.rs crates/yserver/src/kms/v2/glyph_atlas.rs
git commit -m "feat(v2/frame_builder): RecordedOp enum (CompositeGlyphs/GlyphUpload/LayoutTransition) — Phase B.1 Task 2"
```

---

### Task 3: `FramePinSet` (Mechanism 1 — Arc<StagingBuffer>)

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs`
- Modify: `crates/yserver/src/kms/v2/engine.rs` — wrap `StagingBuffer` in `Arc<StagingBuffer>` where the frame builder will share pins (mechanism 1 from the spec)

The spec § "Frame-wide resource pinning" lists three pinning mechanisms. B.1 only needs Mechanism 1 (Arc clone) — Mechanism 2 (descriptor watermark on `DescriptorPoolRing`) is not needed because `composite_glyphs`'s text pipeline holds a static descriptor set (`crate::kms::vk::text_pipeline::TextPipeline::descriptor_set`); it does NOT consult `inner.descriptor_pool_ring`. Mechanism 3 (Arc-wrapping singleton scratch like `DstReadback` / `mask_scratch`) is not needed in B.1 because `composite_glyphs` doesn't touch any of those. Both mechanisms 2 + 3 land in B.2 alongside `render_composite`.

- [ ] **Step 1: Make `StagingBuffer` Arc-shareable inside the engine**

In `engine.rs:275-373`, the existing `StagingBuffer` is owned `Drop`-on-drop. Wrap into `Arc<StagingBuffer>` at every callsite where the frame builder will pin it. For B.1 this is just `composite_glyphs` line 3852:

```rust
// crates/yserver/src/kms/v2/engine.rs:3852 (before)
let staging = StagingBuffer::new(Arc::clone(&inner.vk), upload_bytes.max(1))?;

// (after — wrap in Arc so the frame builder can clone-pin it)
let staging = Arc::new(StagingBuffer::new(Arc::clone(&inner.vk), upload_bytes.max(1))?);
```

Then propagate: `SubmittedOp::staging: Option<StagingBuffer>` → `Option<Arc<StagingBuffer>>`. Search every site that constructs or moves `SubmittedOp` and update accordingly (the engine has ~10 such sites; grep `staging:` in engine.rs and update inline).

- [ ] **Step 2: Write the failing pin-set tests**

```rust
// In frame_builder.rs

use std::sync::Arc;

/// Resource pins held alive across a frame. Mechanism 1 of spec
/// § "Frame-wide resource pinning". B.1 only pins `StagingBuffer`
/// clones (one per glyph upload). B.2 will extend with sync objects,
/// semaphores, and Mechanism 3 Arc'd scratch handles.
#[derive(Debug, Default)]
pub(crate) struct FramePinSet {
    pub(crate) staging_buffers: Vec<Arc<super::engine::StagingBuffer>>,
}

impl FramePinSet {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn pin_staging(&mut self, staging: Arc<super::engine::StagingBuffer>) -> PinnedStagingIdx {
        let idx = u32::try_from(self.staging_buffers.len()).expect("< u32::MAX pins");
        self.staging_buffers.push(staging);
        PinnedStagingIdx(idx)
    }

    pub(crate) fn len(&self) -> usize {
        self.staging_buffers.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.staging_buffers.is_empty()
    }
}

#[cfg(test)]
mod pin_tests {
    use super::*;

    // No-Vk pin tests can't construct a real StagingBuffer (it owns Vk
    // handles). Pin tests here verify the bookkeeping; integration
    // tests in v2_acceptance.rs verify the real-Vk path.

    #[test]
    fn fresh_pin_set_is_empty() {
        let p = FramePinSet::new();
        assert_eq!(p.len(), 0);
        assert!(p.is_empty());
    }
}
```

The `super::engine::StagingBuffer` reference forces `StagingBuffer` to be at least `pub(crate)` visible to the frame_builder module. If it is private today, lift the visibility:

```rust
// engine.rs:275
- struct StagingBuffer {
+ pub(crate) struct StagingBuffer {
```

(And same for its `Drop` impl + the `new`/`new_with_usage` accessors are already only called within engine.rs, so they stay private; only the type name needs to escape.)

- [ ] **Step 3: Run + commit**

```bash
cargo build -p yserver
cargo test -p yserver --lib kms::v2::frame_builder
```
Both: PASS. Then:

```bash
git add crates/yserver/src/kms/v2/frame_builder.rs crates/yserver/src/kms/v2/engine.rs
git commit -m "feat(v2/frame_builder): FramePinSet + Arc<StagingBuffer> (Mechanism 1) — Phase B.1 Task 3"
```

---

### Task 4: `FrameLayoutTable` overlay — drawable + atlas, (pre_frame, in_frame), commit + rollback

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs`

The spec § "Transactional layout state" mandates an overlay shape: `HashMap<DrawableId, LayoutOverlayEntry>` plus an `atlas_pre_frame_layout: Option<vk::ImageLayout>` slot. `current_layout_for(...)` queries the overlay first, falling back to `Drawable::storage.current_layout` when not yet touched in-frame. Commit-on-success writes `current_in_frame_layout` back into storage; rollback-on-failure writes `pre_frame_layout` back.

- [ ] **Step 1: Write the failing unit tests**

```rust
// frame_builder.rs

use std::collections::HashMap;

#[derive(Debug, Clone, Copy)]
pub(crate) struct LayoutOverlayEntry {
    pub(crate) pre_frame_layout: vk::ImageLayout,
    pub(crate) current_in_frame_layout: vk::ImageLayout,
}

/// Per-frame layout overlay. Mutated on each `record_layout_transition`
/// from a ported paint op (none in B.1 — the text pipeline's recorder
/// embeds its own barriers — but the structure is the load-bearing
/// answer to open question 3 and lands in B.1 so B.2 can fold ported
/// `render_composite` / `render_fill` paths in without re-architecting).
///
/// Atlas image layout is tracked separately: the overlay carries a
/// single `Option<LayoutOverlayEntry>` for the atlas because there's
/// exactly one atlas per engine, and `V2GlyphAtlas::current_layout`
/// is the single source of truth that the commit step writes back.
#[derive(Debug, Default)]
pub(crate) struct FrameLayoutTable {
    pub(crate) drawables: HashMap<DrawableId, LayoutOverlayEntry>,
    pub(crate) atlas: Option<LayoutOverlayEntry>,
}

impl FrameLayoutTable {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// First-touch snapshot for a drawable. `pre_frame_layout` is the
    /// value the caller read out of `Drawable::storage.current_layout`
    /// at the moment of first append-in-frame.
    pub(crate) fn first_touch_drawable(
        &mut self,
        id: DrawableId,
        pre_frame_layout: vk::ImageLayout,
    ) {
        self.drawables.entry(id).or_insert(LayoutOverlayEntry {
            pre_frame_layout,
            current_in_frame_layout: pre_frame_layout,
        });
    }

    pub(crate) fn set_drawable_in_frame(&mut self, id: DrawableId, layout: vk::ImageLayout) {
        if let Some(entry) = self.drawables.get_mut(&id) {
            entry.current_in_frame_layout = layout;
        } else {
            // Mis-use; shouldn't happen if `first_touch_drawable` is
            // always called before this method. Panic in debug to
            // surface the bug.
            debug_assert!(
                false,
                "set_drawable_in_frame without first_touch_drawable for {:?}",
                id
            );
        }
    }

    pub(crate) fn first_touch_atlas(&mut self, pre_frame_layout: vk::ImageLayout) {
        if self.atlas.is_none() {
            self.atlas = Some(LayoutOverlayEntry {
                pre_frame_layout,
                current_in_frame_layout: pre_frame_layout,
            });
        }
    }

    pub(crate) fn set_atlas_in_frame(&mut self, layout: vk::ImageLayout) {
        match self.atlas.as_mut() {
            Some(entry) => entry.current_in_frame_layout = layout,
            None => debug_assert!(false, "set_atlas_in_frame without first_touch_atlas"),
        }
    }

    /// Query the effective layout for `id` from the perspective of
    /// the next in-frame op that will touch it. Falls back to
    /// `storage_fallback` (the caller passes
    /// `drawable.storage.current_layout` if the drawable isn't in
    /// the overlay yet).
    pub(crate) fn current_layout_for_drawable(
        &self,
        id: DrawableId,
        storage_fallback: vk::ImageLayout,
    ) -> vk::ImageLayout {
        match self.drawables.get(&id) {
            Some(entry) => entry.current_in_frame_layout,
            None => storage_fallback,
        }
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn first_touch_drawable_snapshots_pre_frame_and_in_frame_equal() {
        let mut t = FrameLayoutTable::new();
        t.first_touch_drawable(DrawableId(7), vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let entry = t.drawables.get(&DrawableId(7)).unwrap();
        assert_eq!(entry.pre_frame_layout, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        assert_eq!(
            entry.current_in_frame_layout,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        );
    }

    #[test]
    fn second_touch_does_not_overwrite_pre_frame() {
        let mut t = FrameLayoutTable::new();
        t.first_touch_drawable(DrawableId(7), vk::ImageLayout::UNDEFINED);
        t.set_drawable_in_frame(DrawableId(7), vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
        t.first_touch_drawable(DrawableId(7), vk::ImageLayout::TRANSFER_DST_OPTIMAL); // re-touch
        let entry = t.drawables.get(&DrawableId(7)).unwrap();
        assert_eq!(entry.pre_frame_layout, vk::ImageLayout::UNDEFINED);
        assert_eq!(
            entry.current_in_frame_layout,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL
        );
    }

    #[test]
    fn current_layout_for_drawable_falls_back_to_storage_when_untouched() {
        let t = FrameLayoutTable::new();
        let got = t.current_layout_for_drawable(DrawableId(8), vk::ImageLayout::PRESENT_SRC_KHR);
        assert_eq!(got, vk::ImageLayout::PRESENT_SRC_KHR);
    }

    #[test]
    fn atlas_first_touch_then_set_in_frame() {
        let mut t = FrameLayoutTable::new();
        t.first_touch_atlas(vk::ImageLayout::UNDEFINED);
        t.set_atlas_in_frame(vk::ImageLayout::TRANSFER_DST_OPTIMAL);
        let a = t.atlas.unwrap();
        assert_eq!(a.pre_frame_layout, vk::ImageLayout::UNDEFINED);
        assert_eq!(a.current_in_frame_layout, vk::ImageLayout::TRANSFER_DST_OPTIMAL);
    }
}
```

- [ ] **Step 2: Run, commit**

```bash
cargo test -p yserver --lib kms::v2::frame_builder
```
Expected: PASS. Commit:

```bash
git add crates/yserver/src/kms/v2/frame_builder.rs
git commit -m "feat(v2/frame_builder): FrameLayoutTable overlay — Phase B.1 Task 4"
```

---

### Task 5: `touched_drawables` overlay — first-touch snapshot, commit + rollback

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs`

The spec § "Drawable lifetime — append-time frame-ticket touch" mandates: each `RecordedOp` append calls `store.touch_render_fence(id, frame_ticket.clone())` AND records the pre-frame ticket into a `HashMap<DrawableId, Option<FenceTicket>>` overlay. Close-success leaves the frame ticket in place (it IS the new `last_render_ticket`); close-failure restores the snapshotted prior ticket.

- [ ] **Step 1: Failing unit tests**

```rust
// frame_builder.rs

/// Per-frame snapshot of `Drawable::last_render_ticket` taken at first
/// append-in-frame. Close-failure restores each entry; close-success
/// is a no-op (the frame ticket already overwrote the slot via
/// `store.touch_render_fence` at append-time).
#[derive(Debug, Default)]
pub(crate) struct TouchedDrawables {
    /// `None` value = drawable had no prior ticket before this frame.
    pub(crate) snapshots: HashMap<DrawableId, Option<FenceTicket>>,
}

impl TouchedDrawables {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record the first-touch pre-frame ticket. Subsequent calls on
    /// the same id are no-ops (the first snapshot is the load-bearing
    /// one — it captures the value the engine needs to restore on
    /// close-failure).
    pub(crate) fn first_touch(
        &mut self,
        id: DrawableId,
        prior_ticket: Option<FenceTicket>,
    ) {
        self.snapshots.entry(id).or_insert(prior_ticket);
    }

    pub(crate) fn len(&self) -> usize {
        self.snapshots.len()
    }
}

#[cfg(test)]
mod touched_tests {
    use super::*;

    #[test]
    fn first_touch_records_prior_ticket_only_once() {
        let mut t = TouchedDrawables::new();
        t.first_touch(DrawableId(1), None);
        assert_eq!(t.len(), 1);
        // Subsequent calls do not overwrite (a later op on the same
        // drawable should not lose the originally-captured prior).
        t.first_touch(DrawableId(1), None);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn separate_drawables_track_independently() {
        let mut t = TouchedDrawables::new();
        t.first_touch(DrawableId(1), None);
        t.first_touch(DrawableId(2), None);
        assert_eq!(t.len(), 2);
    }
}
```

Re-add `use super::platform::FenceTicket;` and `use super::store::DrawableId;` to the module top if Task 1's strip removed them.

- [ ] **Step 2: Run + commit**

```bash
cargo test -p yserver --lib kms::v2::frame_builder
```
Expected: PASS. Commit:

```bash
git add crates/yserver/src/kms/v2/frame_builder.rs
git commit -m "feat(v2/frame_builder): TouchedDrawables overlay — Phase B.1 Task 5"
```

---

### Task 6: Pending glyph inserts (atlas cache transactional commit)

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs`

The spec § "Glyph upload — speculative atlas overlay" mandates: `pack` stays monotonic (leaked slots on failure are acceptable in the rare-failure regime), `insert_entry` is held in a per-frame pending list and committed only on close-success. The pending list lives on `OpenFrame`.

- [ ] **Step 1: Failing unit tests**

```rust
// frame_builder.rs

/// Pending glyph cache inserts. `composite_glyphs`'s upload path
/// already calls `V2GlyphAtlas::pack` (shelf advance, monotonic — the
/// slot stays consumed even if the frame fails), but `insert_entry`
/// (cache commit) is deferred here. Close-success drains this and
/// calls `V2GlyphAtlas::insert_entry` on the atlas; close-failure
/// drops the list — the slot leaks but the cache stays consistent
/// (next paint re-packs).
#[derive(Debug, Default)]
pub(crate) struct PendingGlyphInserts {
    pub(crate) entries: Vec<(GlyphKey, AtlasEntry)>,
}

impl PendingGlyphInserts {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push(&mut self, key: GlyphKey, entry: AtlasEntry) {
        self.entries.push((key, entry));
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod glyph_insert_tests {
    use super::*;

    #[test]
    fn fresh_is_empty() {
        assert_eq!(PendingGlyphInserts::new().len(), 0);
    }

    #[test]
    fn push_appends_in_order() {
        let mut p = PendingGlyphInserts::new();
        p.push(
            GlyphKey { font_xid: 1, codepoint: 65 },
            AtlasEntry { atlas_x: 0, atlas_y: 0, w: 8, h: 12, pen_left: 0, pen_top: 0 },
        );
        p.push(
            GlyphKey { font_xid: 1, codepoint: 66 },
            AtlasEntry { atlas_x: 8, atlas_y: 0, w: 8, h: 12, pen_left: 0, pen_top: 0 },
        );
        assert_eq!(p.len(), 2);
        assert_eq!(p.entries[0].0.codepoint, 65);
        assert_eq!(p.entries[1].0.codepoint, 66);
    }
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p yserver --lib kms::v2::frame_builder
```
Expected: PASS. Commit:

```bash
git add crates/yserver/src/kms/v2/frame_builder.rs
git commit -m "feat(v2/frame_builder): PendingGlyphInserts (transactional atlas cache commit) — Phase B.1 Task 6"
```

---

### Task 7: `OpenFrame` carries pins + layout overlay + touched drawables + pending glyph inserts

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs`

Now that the four overlays exist, fold them into `OpenFrame` (which was just `ticket + ops` after Task 1). This task is the structural glue; no new logic.

- [ ] **Step 1: Update `OpenFrame`**

```rust
// frame_builder.rs — replace the Task 1 OpenFrame skeleton

#[derive(Debug)]
pub(crate) struct OpenFrame {
    pub(crate) ticket: FenceTicket,
    pub(crate) ops: Vec<RecordedOp>,
    pub(crate) pins: FramePinSet,
    pub(crate) layouts: FrameLayoutTable,
    pub(crate) touched: TouchedDrawables,
    pub(crate) pending_glyph_inserts: PendingGlyphInserts,
    /// Snapshot of `V2GlyphAtlas::last_render_ticket` taken at the first
    /// glyph append-in-frame. `Some(None)` means "the atlas had no
    /// prior ticket" — distinct from `None` which means "not yet
    /// touched in this frame".
    pub(crate) atlas_prev_ticket_snapshot: Option<Option<FenceTicket>>,
    /// Glyph uploads recorded in this frame; bumped each push. Used
    /// for the `frame_builder_glyph_uploads_per_frame` telemetry +
    /// the spec's "load-bearing for the bee-fault narrative" gauge.
    pub(crate) glyph_uploads_in_frame: u32,
    pub(crate) close_reason_on_open: Option<CloseReason>, // reserved for B.4
}
```

- [ ] **Step 2: Write a state-machine integration test exercising the structure**

```rust
#[cfg(test)]
mod open_frame_tests {
    use super::*;

    #[test]
    fn open_frame_aggregates_all_overlays() {
        let frame = OpenFrame {
            ticket: FenceTicket::for_tests_stub(),
            ops: Vec::new(),
            pins: FramePinSet::new(),
            layouts: FrameLayoutTable::new(),
            touched: TouchedDrawables::new(),
            pending_glyph_inserts: PendingGlyphInserts::new(),
            atlas_prev_ticket_snapshot: None,
            glyph_uploads_in_frame: 0,
            close_reason_on_open: None,
        };
        assert!(frame.ops.is_empty());
        assert_eq!(frame.pins.len(), 0);
        assert_eq!(frame.touched.len(), 0);
        assert_eq!(frame.pending_glyph_inserts.len(), 0);
        assert_eq!(frame.glyph_uploads_in_frame, 0);
    }
}
```

`FenceTicket::for_tests_stub()` doesn't exist today. Add a minimal `#[cfg(test)] pub(crate) fn for_tests_stub() -> Self` constructor on `FenceTicket` in `crates/yserver/src/kms/v2/platform.rs` next to the existing `FenceTicket` impl block. The stub returns a ticket whose `poll_signaled` returns `true` and `wait` returns `Ok(())` — same shape as the existing pattern in `submit_group.rs` tests where Vk handles are faked via `vk::Handle::from_raw`. If a stub constructor already exists, reuse it.

- [ ] **Step 3: Run + commit**

```bash
cargo test -p yserver --lib kms::v2::frame_builder
```
Expected: PASS. Commit:

```bash
git add crates/yserver/src/kms/v2/frame_builder.rs crates/yserver/src/kms/v2/platform.rs
git commit -m "feat(v2/frame_builder): OpenFrame aggregates pins+layouts+touched+pending — Phase B.1 Task 7"
```

---

### Task 8: `V2GlyphAtlas::last_render_ticket` field + shutdown gate

**Files:**
- Modify: `crates/yserver/src/kms/v2/glyph_atlas.rs`

The spec § "Drawable lifetime — append-time frame-ticket touch" — sub-bullet "Atlas image": the atlas needs a `last_render_ticket: Option<FenceTicket>` field so destruction at `KmsBackendV2::shutdown` waits for the last frame that touched it. Field is `None` until the first frame-close-success sets it.

- [ ] **Step 1: Failing test — atlas-ticket field + shutdown gate**

```rust
// glyph_atlas.rs — append to the existing struct + add a test

pub(crate) struct V2GlyphAtlas {
    vk: Arc<VkContext>,
    image: vk::Image,
    view: vk::ImageView,
    memory: vk::DeviceMemory,
    packer: ShelfPacker,
    current_layout: vk::ImageLayout,
    /// Stage 5 / Phase B.1: the `FenceTicket` of the most recent frame
    /// that touched the atlas image (uploaded a glyph or sampled it
    /// in a draw). `None` until the first frame-close-success.
    /// Destruction at backend shutdown waits on this ticket the same
    /// way `DrawableStore::poll_pending_retire` gates drawable
    /// destruction (engine drains `pending_frames` first; this field
    /// is the fallback for any path that bypasses the queue).
    last_render_ticket: Option<super::platform::FenceTicket>,
}

impl V2GlyphAtlas {
    // … existing constructor; add the new field init:
    // last_render_ticket: None,

    pub(crate) fn set_last_render_ticket(&mut self, ticket: super::platform::FenceTicket) {
        self.last_render_ticket = Some(ticket);
    }

    pub(crate) fn last_render_ticket(&self) -> Option<&super::platform::FenceTicket> {
        self.last_render_ticket.as_ref()
    }
}
```

- [ ] **Step 2: Add a unit test that doesn't need a real Vk atlas**

Since `V2GlyphAtlas::new()` requires Vk, the unit test path is awkward. Instead, expose a `#[cfg(test)] pub(crate) fn for_tests_set_last_render_ticket_field_only` if needed, or rely on the integration test in Task 21 to exercise this. For now, add a TYPE-level test that the field exists and is accessible — verified by compilation alone is fine; no new test in this task.

Verify build:
```bash
cargo build -p yserver
```

- [ ] **Step 3: Commit**

```bash
git add crates/yserver/src/kms/v2/glyph_atlas.rs
git commit -m "feat(v2/glyph_atlas): last_render_ticket field for Phase B.1 frame-builder gating — Task 8"
```

---

### Task 9: `FrameSubmittedRecord` + `pending_frames` retirement queue

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs` — define the record type
- Modify: `crates/yserver/src/kms/v2/engine.rs` — embed the queue, wire into `poll_retired` + `drain_all`

The spec § "Frame-wide resource pinning" — "lifetime contract": frame-close-success moves the pin set onto a record parked alongside the frame ticket; signal-on-CPU drops the Arcs. B.1 uses a `VecDeque<FrameSubmittedRecord>` parallel to `submitted`.

- [ ] **Step 1: Define `FrameSubmittedRecord`**

```rust
// frame_builder.rs

/// One in-flight frame's resource pin set, parked until the frame's
/// `FenceTicket` signals. Walked by `RenderEngine::poll_retired` next
/// to the existing `submitted` queue; both gate retirement on the same
/// ticket. Drop order: when the ticket signals, the record drops, its
/// `pins.staging_buffers` Arcs decrement, and any `StagingBuffer`
/// whose Arc refcount hits zero releases its Vk handles.
#[derive(Debug)]
pub(crate) struct FrameSubmittedRecord {
    pub(crate) ticket: FenceTicket,
    pub(crate) pins: FramePinSet,
    /// Lifetime count snapshot — telemetry uses this to attribute the
    /// retirement to the closing frame.
    pub(crate) frame_seq: u64,
}
```

- [ ] **Step 2: Embed the queue on `RenderEngineInner`**

```rust
// engine.rs — in RenderEngineInner

pub(crate) struct RenderEngineInner {
    // … existing fields …
    /// Phase B.1: in-flight frames awaiting retirement. Parallel to
    /// `submitted`; both gate on the same `FenceTicket`s when the
    /// frame builder is in play. Walked by `poll_retired` and
    /// `drain_all`.
    pending_frames: std::collections::VecDeque<super::frame_builder::FrameSubmittedRecord>,
    /// Phase B.1: monotonic frame sequence for telemetry attribution.
    /// Bumped on every `FrameBuilder::close_into_cb` success.
    frame_seq: u64,
}

// Initializer in RenderEngine::new (around engine.rs:617):
pending_frames: std::collections::VecDeque::new(),
frame_seq: 0,
```

- [ ] **Step 3: Drain on `poll_retired`**

```rust
// engine.rs — at the end of poll_retired (after the existing submitted-drain loop):

// Phase B.1: walk pending_frames. Same ticket-signaled monotonicity
// argument as the `submitted` loop.
while let Some(front) = inner.pending_frames.front() {
    if !front.ticket.poll_signaled(&inner.vk) {
        break;
    }
    // The Arcs inside the record drop here, releasing pinned resources.
    let _ = inner.pending_frames.pop_front().expect("non-empty");
}
```

- [ ] **Step 4: Drain on `drain_all`** (shutdown)

```rust
// engine.rs — drain_all, after the existing `while let Some(mut op) = inner.submitted.pop_front()` loop:

while let Some(mut record) = inner.pending_frames.pop_front() {
    let _ = record.ticket.wait(&inner.vk);
    // Record drops; pins drop; Arcs decrement.
    drop(record);
}
```

- [ ] **Step 5: Test the drain path**

```rust
// engine.rs unit-test module, or a focused fixture under frame_builder.rs

#[cfg(test)]
mod pending_frames_tests {
    use super::*;
    use crate::kms::v2::frame_builder::{FramePinSet, FrameSubmittedRecord};
    use crate::kms::v2::platform::FenceTicket;

    #[test]
    fn poll_retired_drops_pending_frames_whose_ticket_signaled() {
        // Use a no-Vk fixture: stub engine + FenceTicket::for_tests_stub
        // returns a ticket that always polls signaled. Push a record;
        // call poll_retired; expect the record gone.
        // (Skipping if poll_retired needs a live VkContext — in that
        // case, integration test in v2_acceptance covers this. The
        // intent is to wire a unit test if the stub path supports it.)
    }
}
```

If a no-Vk stub path exists, finish the test. If not, defer the assertion to Task 21's integration tests (which exercise the live-Vk retirement path through `KmsBackendV2::for_tests_with_vk`).

- [ ] **Step 6: Verify + commit**

```bash
cargo build -p yserver
cargo test -p yserver --lib kms::v2
```
Both: PASS. Commit:

```bash
git add crates/yserver/src/kms/v2/frame_builder.rs crates/yserver/src/kms/v2/engine.rs
git commit -m "feat(v2/engine): FrameSubmittedRecord + pending_frames retirement queue — Phase B.1 Task 9"
```

---

### Task 10: Invariant M1 — `SubmitGroup::new()` defaults to 1 + add `FlushReason::FrameBuilder`

**Files:**
- Modify: `crates/yserver/src/kms/v2/submit_group.rs:51-63` (change default from 16 → 1) + `:20-27` (add `FlushReason::FrameBuilder` variant) + `:133-140` (update default test)
- Modify: `crates/yserver/src/kms/v2/platform.rs:679` — find the `YSERVER_PAINT_SUBMIT_GROUP_CAP` env-override code path; clamp it to `1` during B.1–B.4 (or remove the override entirely; pick clamp + log so a future operator who sets it sees the override is being ignored)
- Modify: `crates/yserver/src/kms/v2/telemetry.rs:113-118` — add `submit_group_flush_reason_frame_builder: u64` + map it in `record_submit_group_flush`

This is the first observable behaviour change of B.1. Bee survives this commit alone (the `cap=1` row of the 2026-05-23 capture proves it); other platforms see their submit rate regress to pre-Phase-A levels and stay there until Task 24 flips the frame-builder gate on. This is the documented trade-off in the Phase B spec § "Migration boundaries" — Invariant M1.

Codex review (round 1) raised "M1 should not rely on scattered platform overrides" — fix is to make 1 the type-level default AND disable any production override that allows `> 1`. Tests can still override via the existing `set_max_size_for_tests` helper.

- [ ] **Step 1: Change the default in `SubmitGroup::new()`**

```rust
// crates/yserver/src/kms/v2/submit_group.rs:51-63
impl SubmitGroup {
    pub(crate) fn new() -> Self {
        // Phase B Invariant M1: every queue submission carries at
        // most ONE command buffer for the duration of the B.1 → B.4
        // sub-phase rollout. The frame builder collapses paint into
        // one CB per frame itself; non-ported paint ops fall back
        // to the pre-Phase-A per-op submit cadence. Bee MATE survives
        // this trivially (see status.md § "2026-05-23 bee MATE-load
        // freeze" — the cap=1 row); other platforms see a temporary
        // submit-rate regression that recovers in B.5 when the
        // SubmitGroup retires entirely.
        Self {
            entries: Vec::new(),
            ticket: None,
            max_size: 1,
        }
    }
}
```

- [ ] **Step 2: Add `FlushReason::FrameBuilder`**

```rust
// crates/yserver/src/kms/v2/submit_group.rs:20-27
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FlushReason {
    SyncBoundary,
    PresentCompletionSignal,
    SceneCompose,
    PageflipRetire,
    MaxSize,
    Shutdown,
    /// Phase B.1: a frame-builder close drove this flush. Used by
    /// `RenderEngine::close_open_frame` regardless of the underlying
    /// `CloseReason`; the close-reason histogram is reported
    /// separately under `frame_builder_close_reason_*`.
    FrameBuilder,
}
```

- [ ] **Step 3: Update the Phase A default test**

The existing test `fresh_group_is_empty_and_closed_with_default_max_size_sixteen` in `submit_group.rs:133-140` asserts default 16. Rename + update:

```rust
#[test]
fn fresh_group_is_empty_and_closed_with_default_max_size_one() {
    let g = SubmitGroup::new();
    assert!(!g.is_open());
    assert_eq!(g.size(), 0);
    // Phase B Invariant M1: default is 1 for the duration of B.1–B.4.
    assert_eq!(g.max_size(), 1);
}
```

- [ ] **Step 4: Disable the production env-cap override**

Find the existing `YSERVER_PAINT_SUBMIT_GROUP_CAP` read in `platform.rs` around line 679 (or wherever `set_max_size` is called from production today). Codex pointed at this site as an "attractive nuisance". Two options:
- **Remove the read entirely** — simpler, more honest, but breaks any operator who depended on it. Preferred.
- **Clamp + log a warning** if the env var is set to anything other than `1`. Lower-disruption but still principled.

Pick remove. Add a deprecation note in the commit message.

- [ ] **Step 5: Update telemetry to track FlushReason::FrameBuilder**

```rust
// telemetry.rs Bucket — add field
pub(crate) submit_group_flush_reason_frame_builder: u64,

// telemetry.rs record_submit_group_flush — match arm
R::FrameBuilder => (
    &mut self.bucket.submit_group_flush_reason_frame_builder,
    &mut self.lifetime.submit_group_flush_reason_frame_builder,
),
```

Plus include this counter in the existing `maybe_emit` log line (find the per-reason print in `maybe_emit`; add `frame_builder=N`).

- [ ] **Step 6: Pin-the-default regression test**

```rust
// crates/yserver/tests/v2_acceptance.rs

#[test]
fn v2_platform_open_pins_submit_group_max_size_to_one() {
    let backend = KmsBackendV2::for_tests_with_vk().expect("vk");
    assert_eq!(
        backend.platform_submit_group_max_size_for_tests(),
        1,
        "Phase B Invariant M1: SubmitGroup max_size must be 1 in B.1–B.4"
    );
}
```

`platform_submit_group_max_size_for_tests` needs a wrapper on `KmsBackendV2`. Add it next to the existing `Phase A T8`-era wrappers in `backend.rs`.

- [ ] **Step 7: Run + commit**

```bash
cargo build -p yserver
cargo test -p yserver
```
All Phase A tests adapt to cap=1 (most of them already exercise cap=1 paths). Commit:

```bash
git add crates/yserver/src/kms/v2/submit_group.rs crates/yserver/src/kms/v2/platform.rs crates/yserver/src/kms/v2/telemetry.rs crates/yserver/src/kms/v2/backend.rs crates/yserver/tests/v2_acceptance.rs
git commit -m "feat(v2): Invariant M1 — SubmitGroup default cap=1 + remove env override + FlushReason::FrameBuilder (Phase B.1 Task 10)"
```

---

### Task 11: `FrameBuilder::open_for_paint` + `close_into_cb` skeleton (state machine, no CB recording yet)

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs` — add `open_for_paint` + `close_into_cb` stubs returning errors / asserting state

The skeleton handles the state machine and lifetime accounting. The actual CB recording lands in Task 12 with the 3-pass walk; this task just sets up the entry points so subsequent tasks can fill them in.

- [ ] **Step 1: Failing state-machine tests + stub method bodies**

```rust
// frame_builder.rs

/// Frame-close outcome surfaced to the engine. `Submitted` carries
/// the same `FlushOutcome` Phase A's `flush_submit_group` returned
/// (number of entries flushed, reason); the frame builder produces
/// one such outcome per close. `NoOp` means "frame was already
/// closed; nothing to do".
#[derive(Debug)]
pub(crate) enum CloseOutcome {
    /// Frame closed and submitted (one CB through SubmitGroup
    /// auto-flush). Carries the frame ticket the caller will record
    /// for retirement.
    Submitted {
        frame_seq: u64,
        op_count: usize,
        pin_count: usize,
        ticket: FenceTicket,
        reason: CloseReason,
    },
    /// Close requested but frame was already closed. No-op.
    AlreadyClosed,
}

impl FrameBuilder {
    /// Open a new frame, acquiring the shared `FenceTicket` from
    /// `SubmitGroup::open_with`. Panics if the frame is already open
    /// (caller is responsible for checking `is_open()` first).
    ///
    /// The `ticket` argument is the one obtained from
    /// `PlatformBackend::submit_group_ticket_or_open()` — the engine
    /// is the only caller and already has access to the platform.
    pub(crate) fn open_for_paint(&mut self, ticket: FenceTicket) {
        assert!(
            !self.is_open(),
            "FrameBuilder::open_for_paint while already open — caller must check is_open()"
        );
        self.state = FrameState::OpenForPaint;
        self.lifetime_opens = self.lifetime_opens.wrapping_add(1);
        self.open = Some(Box::new(OpenFrame {
            ticket,
            ops: Vec::new(),
            pins: FramePinSet::new(),
            layouts: FrameLayoutTable::new(),
            touched: TouchedDrawables::new(),
            pending_glyph_inserts: PendingGlyphInserts::new(),
            atlas_prev_ticket_snapshot: None,
            glyph_uploads_in_frame: 0,
            close_reason_on_open: None,
        }));
    }

    /// Take the open frame for replay. Returns `None` if not open.
    /// Caller is responsible for calling either `complete_close_success`
    /// or `complete_close_failure` afterwards to update the lifetime
    /// counter and bring the FrameBuilder back to `Closed`.
    pub(crate) fn take_open_for_close(&mut self, reason: CloseReason) -> Option<Box<OpenFrame>> {
        if !self.is_open() {
            return None;
        }
        let mut frame = self.open.take().expect("is_open implies Some");
        frame.close_reason_on_open = Some(reason);
        Some(frame)
    }

    /// Finalise close-success path. The caller has already submitted
    /// the CB and committed overlays/pins/inserts into engine + atlas
    /// state; this updates the FrameBuilder's bookkeeping.
    pub(crate) fn complete_close_success(&mut self) {
        debug_assert!(matches!(self.state, FrameState::OpenForPaint));
        self.state = FrameState::Closed;
        self.lifetime_closes = self.lifetime_closes.wrapping_add(1);
        // `self.open` is already None (take_open_for_close moved it).
    }

    /// Finalise close-failure path. The caller has already rolled back
    /// engine/atlas state and set `platform.renderer_failed`; this
    /// updates the FrameBuilder's bookkeeping.
    pub(crate) fn complete_close_failure(&mut self) {
        debug_assert!(matches!(self.state, FrameState::OpenForPaint));
        self.state = FrameState::Closed;
        self.lifetime_closes = self.lifetime_closes.wrapping_add(1);
    }

    /// True if the next append would push the pin set past the
    /// per-frame ceiling. Caller checks this and forces a close
    /// (`reason = PinCeiling`) BEFORE the new op's append.
    pub(crate) fn would_exceed_pin_ceiling(&self, new_pins: usize) -> bool {
        match self.open.as_ref() {
            None => false, // no frame open → nothing to exceed
            Some(open) => open.pins.len() + new_pins > self.max_pinned_resources_per_frame,
        }
    }

    /// `#[cfg(test)]` peek at the op list in append order.
    #[cfg(test)]
    pub(crate) fn peek_ops(&self) -> Option<&[RecordedOp]> {
        self.open.as_ref().map(|o| o.ops.as_slice())
    }

    /// `#[cfg(test)]` op count.
    #[cfg(test)]
    pub(crate) fn op_count(&self) -> usize {
        self.open.as_ref().map_or(0, |o| o.ops.len())
    }

    /// `#[cfg(test)]` pin count.
    #[cfg(test)]
    pub(crate) fn pin_count(&self) -> usize {
        self.open.as_ref().map_or(0, |o| o.pins.len())
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn open_for_paint_transitions_to_open_state_and_bumps_lifetime_opens() {
        let mut fb = FrameBuilder::new();
        fb.open_for_paint(FenceTicket::for_tests_stub());
        assert!(fb.is_open());
        assert_eq!(fb.lifetime_opens(), 1);
        assert_eq!(fb.lifetime_closes(), 0);
        assert_eq!(fb.op_count(), 0);
    }

    #[test]
    fn take_open_for_close_returns_frame_and_records_reason() {
        let mut fb = FrameBuilder::new();
        fb.open_for_paint(FenceTicket::for_tests_stub());
        let frame = fb
            .take_open_for_close(CloseReason::NonPortedPaintOp)
            .expect("frame open");
        assert_eq!(frame.close_reason_on_open, Some(CloseReason::NonPortedPaintOp));
    }

    #[test]
    fn complete_close_success_bumps_lifetime_closes_and_returns_to_closed() {
        let mut fb = FrameBuilder::new();
        fb.open_for_paint(FenceTicket::for_tests_stub());
        let _ = fb.take_open_for_close(CloseReason::SceneCompose);
        fb.complete_close_success();
        assert!(!fb.is_open());
        assert_eq!(fb.lifetime_closes(), 1);
    }

    #[test]
    fn would_exceed_pin_ceiling_false_when_closed() {
        let fb = FrameBuilder::new();
        assert!(!fb.would_exceed_pin_ceiling(10_000));
    }

    #[test]
    fn would_exceed_pin_ceiling_true_when_open_and_over_default_ceiling() {
        let mut fb = FrameBuilder::new();
        fb.open_for_paint(FenceTicket::for_tests_stub());
        assert!(fb.would_exceed_pin_ceiling(1025));
        assert!(!fb.would_exceed_pin_ceiling(1024));
    }

    #[test]
    #[should_panic(expected = "while already open")]
    fn open_for_paint_panics_when_already_open() {
        let mut fb = FrameBuilder::new();
        fb.open_for_paint(FenceTicket::for_tests_stub());
        fb.open_for_paint(FenceTicket::for_tests_stub());
    }
}
```

- [ ] **Step 2: Embed the FrameBuilder on the engine inner**

```rust
// engine.rs — in RenderEngineInner

pub(crate) struct RenderEngineInner {
    // …
    frame_builder: super::frame_builder::FrameBuilder,
    // …
}

// In RenderEngine::new initializer:
frame_builder: super::frame_builder::FrameBuilder::new(),
```

- [ ] **Step 3: Run + commit**

```bash
cargo build -p yserver
cargo test -p yserver --lib kms::v2::frame_builder
cargo test -p yserver --lib kms::v2
```
All green. Commit:

```bash
git add crates/yserver/src/kms/v2/frame_builder.rs crates/yserver/src/kms/v2/engine.rs
git commit -m "feat(v2/frame_builder): open_for_paint + close lifecycle skeleton (state machine only) — Phase B.1 Task 11"
```

---

### Task 12: 3-pass close walk — resource / record / finalise (engine-side)

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs` — implement `RenderEngine::close_open_frame(...)`

This is the centerpiece. The method takes the open frame from the `FrameBuilder`, walks its ops three times (resource pass / record pass / finalise pass), submits ONE primary CB through the SubmitGroup (cap=1 → one `vkQueueSubmit2`), and on success parks the pin set onto `pending_frames`. **The ordering follows the "Close-path correctness pattern" above** — commit happens after `flush_submit_group` returns Ok, never before.

For B.1 the only recorded ops are `RecordedOp::CompositeGlyphs` + `RecordedOp::GlyphUpload` + (unused) `RecordedOp::LayoutTransition`. The record pass therefore needs only two op-emitter functions:
- `emit_glyph_upload_into_cb` — calls the existing `V2GlyphAtlas::record_upload` (which mutates `V2GlyphAtlas::current_layout` in place; the pre-frame snapshot in `OpenFrame::layouts.atlas` is what rollback restores).
- `emit_composite_glyphs_into_cb` — calls the existing `record_text_run_scissored` via the existing `StorageTextTarget` adapter (which mutates `drawable.storage.current_layout` in place; the pre-frame snapshot in `OpenFrame::layouts.drawables[dst_id]` is what rollback restores).

The resource pass is trivial in B.1 because the only inter-op dependency is atlas-upload-then-draw within a single composite_glyphs call, and the atlas's `record_upload` already emits the surrounding barriers. The pass exists for structure (the spec mandates 3-pass) but does no work in B.1; tasks B.2+ fill it in as they add cross-op barrier logic.

- [ ] **Step 1: Sketch the signature (commit-after-flush-Ok structure)**

```rust
// engine.rs — new method on RenderEngine

impl RenderEngine {
    /// Phase B.1: close the open frame (if any) for `reason`, replay
    /// its op list into ONE primary CB, submit through the
    /// SubmitGroup (cap=1 → one vkQueueSubmit2), and ONLY THEN park
    /// the pin set onto `pending_frames` + commit overlays. On any
    /// failure before submit-success, the local OpenFrame drops
    /// (pins evaporate, overlays evaporate); rollback writes
    /// pre_frame_layout values back to storage where the recorder
    /// already mutated them.
    pub(crate) fn close_open_frame(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
        reason: super::frame_builder::CloseReason,
    ) -> Result<super::frame_builder::CloseOutcome, RenderError> {
        // ── Take the open frame from the FrameBuilder. After this
        //    point, the frame is OURS — neither FrameBuilder nor
        //    the engine has access to it until we explicitly call
        //    complete_close_{success,failure}.
        let (mut open_frame, frame_seq) = {
            let inner = match self.inner.as_mut() {
                Some(i) => i,
                None => return Ok(super::frame_builder::CloseOutcome::AlreadyClosed),
            };
            let Some(open_frame_box) = inner.frame_builder.take_open_for_close(reason) else {
                return Ok(super::frame_builder::CloseOutcome::AlreadyClosed);
            };
            inner.frame_seq = inner.frame_seq.wrapping_add(1);
            (*open_frame_box, inner.frame_seq)
        };
        let frame_ticket = open_frame.ticket.clone();

        // ── Allocate the primary CB. On failure: no SubmitGroup
        //    touch, no commit; just drop frame and complete-fail.
        let (cb, _ticket_for_op) = {
            let inner = self.inner.as_mut().expect("inner");
            match begin_op_cb(inner, platform) {
                Ok(t) => t,
                Err(e) => {
                    rollback_pre_submit(store, &mut open_frame);
                    {
                        let inner = self.inner.as_mut().expect("inner");
                        rollback_atlas(
                            inner,
                            open_frame.layouts.atlas,
                            open_frame.atlas_prev_ticket_snapshot.clone(),
                        );
                        inner.frame_builder.complete_close_failure();
                    }
                    return Err(e);
                }
            }
        };

        // ── Pass 1 (resource) — no-op in B.1.
        // ── Pass 2 (record) — record each op into cb. The recorder
        //    mutates storage layouts directly (see § Pitfall 2).
        let record_result = {
            let inner = self.inner.as_mut().expect("inner");
            (|| -> Result<(), RenderError> {
                for op in &open_frame.ops {
                    emit_recorded_op_into_cb(inner, store, cb, &open_frame.pins, op)?;
                }
                Ok(())
            })()
        };

        if let Err(e) = record_result {
            // CB never appended to SubmitGroup. Free it ourselves
            // (it was begin'd but is mid-record; freeing in that
            // state is legal — Vulkan spec § "Command Buffer
            // Lifecycle" lists Recording as a freeable state).
            {
                let inner = self.inner.as_mut().expect("inner");
                let device = &inner.vk.device;
                if let Some(pool) = platform.ops_command_pool_handle() {
                    // SAFETY: cb was allocated from `pool` and never
                    // submitted; safe to free in Recording state.
                    unsafe { device.free_command_buffers(pool, &[cb]) };
                }
            }
            rollback_pre_submit(store, &mut open_frame);
            platform.renderer_failed = true;
            {
                let inner = self.inner.as_mut().expect("inner");
                rollback_atlas(
                    inner,
                    open_frame.layouts.atlas,
                    open_frame.atlas_prev_ticket_snapshot.clone(),
                );
                inner.frame_builder.complete_close_failure();
            }
            return Err(e);
        }

        // ── End CB + append to SubmitGroup. This does NOT
        //    vkQueueSubmit2 yet — the cap=1 auto-flush would
        //    submit on next `flush_submit_group` call.
        let append_result = {
            let inner = self.inner.as_mut().expect("inner");
            end_and_submit_op(inner, platform, cb, &frame_ticket)
        };
        if let Err(e) = append_result {
            // end_command_buffer or platform.submit_paint_cb_with_semaphore
            // failed. CB is in the SubmitGroup only if append succeeded;
            // play it safe and free the CB here (post-end_command_buffer
            // a CB is in Executable state which is also freeable).
            {
                let inner = self.inner.as_mut().expect("inner");
                let device = &inner.vk.device;
                if let Some(pool) = platform.ops_command_pool_handle() {
                    unsafe { device.free_command_buffers(pool, &[cb]) };
                }
            }
            rollback_pre_submit(store, &mut open_frame);
            platform.renderer_failed = true;
            {
                let inner = self.inner.as_mut().expect("inner");
                rollback_atlas(
                    inner,
                    open_frame.layouts.atlas,
                    open_frame.atlas_prev_ticket_snapshot.clone(),
                );
                inner.frame_builder.complete_close_failure();
            }
            return Err(e);
        }

        // ── Park a SubmittedOp into pending_group_ops so the
        //    engine's flush_submit_group commits it into `submitted`
        //    when the flush succeeds.
        {
            let inner = self.inner.as_mut().expect("inner");
            inner.acquire_generation += 1;
            let generation = inner.acquire_generation;
            inner.pending_group_ops.push(SubmittedOp {
                cb,
                ticket: frame_ticket.clone(),
                staging: None,    // pins live on FrameSubmittedRecord
                scratch: None,
                atlas_ticket: None,
                generation,
            });
        }
        // ── inner borrow released by leaving the block above.

        // ── Drive the actual vkQueueSubmit2 via
        //    RenderEngine::flush_submit_group (the engine-side
        //    wrapper). This calls platform.flush_submit_group; on
        //    Err the platform's abort_flush frees CBs + sets
        //    renderer_failed; the engine's wrapper clears
        //    pending_group_ops.
        let flush_outcome = self.flush_submit_group(
            platform,
            super::submit_group::FlushReason::FrameBuilder,
        );

        match flush_outcome {
            Ok(_) => {
                // ── Commit-after-Ok.
                let inner = self.inner.as_mut().expect("inner");
                inner.pending_frames.push_back(super::frame_builder::FrameSubmittedRecord {
                    ticket: frame_ticket.clone(),
                    pins: std::mem::take(&mut open_frame.pins),
                    frame_seq,
                });
                let op_count = open_frame.ops.len();
                let glyph_uploads = open_frame.glyph_uploads_in_frame;
                commit_close_success(
                    inner,
                    store,
                    std::mem::take(&mut open_frame.layouts),
                    std::mem::take(&mut open_frame.touched),
                    std::mem::take(&mut open_frame.pending_glyph_inserts),
                    &frame_ticket,
                );
                inner.pending_frame_close_events.push(super::frame_builder::FrameCloseEvent {
                    reason,
                    ops_in_frame: op_count,
                    glyph_uploads_in_frame: glyph_uploads,
                    pin_count: inner.pending_frames.back().map(|r| r.pins.len()).unwrap_or(0),
                });
                inner.frame_builder.complete_close_success();
                Ok(super::frame_builder::CloseOutcome::Submitted {
                    frame_seq,
                    op_count,
                    pin_count: 0, // pins already in pending_frames; ignore here
                    ticket: frame_ticket,
                    reason,
                })
            }
            Err(e) => {
                // ── Submit failed. Platform's abort_flush already:
                //    - freed the CB (do not double-free)
                //    - set renderer_failed = true
                //    - cleared the SubmitGroup
                //    Engine's flush_submit_group already cleared
                //    pending_group_ops on Err. So we just rollback
                //    layouts + drop the local OpenFrame.
                rollback_pre_submit(store, &mut open_frame);
                let atlas_overlay = open_frame.layouts.atlas;
                let atlas_prev = open_frame.atlas_prev_ticket_snapshot.clone();
                let ops_in_frame = open_frame.ops.len();
                let glyph_uploads_in_frame = open_frame.glyph_uploads_in_frame;
                let pin_count = open_frame.pins.len();
                let inner = self.inner.as_mut().expect("inner");
                rollback_atlas(inner, atlas_overlay, atlas_prev);
                inner.pending_frame_close_events.push(super::frame_builder::FrameCloseEvent {
                    reason,
                    ops_in_frame,
                    glyph_uploads_in_frame,
                    pin_count,
                });
                inner.frame_builder.complete_close_failure();
                Err(RenderError::Vk(e))
            }
        }
    }
}

fn emit_recorded_op_into_cb(
    inner: &mut RenderEngineInner,
    store: &mut DrawableStore,
    cb: vk::CommandBuffer,
    pins: &super::frame_builder::FramePinSet,
    op: &super::frame_builder::RecordedOp,
) -> Result<(), RenderError> {
    use super::frame_builder::RecordedOp as Op;
    match op {
        Op::GlyphUpload(up) => {
            let atlas = inner.glyph_atlas.as_mut().ok_or(RenderError::NoVk)?;
            let staging = pins.staging_buffers[up.staging_pin_idx.0 as usize].buffer;
            atlas.record_upload(cb, staging, up.atlas_x, up.atlas_y, up.w, up.h);
            Ok(())
        }
        Op::CompositeGlyphs(cg) => {
            let atlas = inner.glyph_atlas.as_ref().ok_or(RenderError::NoVk)?;
            let atlas_extent = atlas.extent();
            let pipeline = inner.text_pipeline.as_ref().ok_or(RenderError::NoVk)?;
            let drawable = store
                .get_mut(cg.dst_id)
                .ok_or(RenderError::UnknownDrawable(cg.dst_id))?;
            let mut adapter = StorageTextTarget {
                extent: drawable.storage.extent,
                image: drawable.storage.image,
                image_view: drawable.storage.image_view,
                current_layout: drawable.storage.current_layout,
            };
            let glyphs_view: Vec<crate::kms::vk::ops::text::TextGlyph> = cg
                .glyphs
                .iter()
                .map(|g| crate::kms::vk::ops::text::TextGlyph {
                    entry: crate::kms::v2::glyph_atlas::AtlasEntry {
                        atlas_x: g.atlas_x,
                        atlas_y: g.atlas_y,
                        w: g.w,
                        h: g.h,
                        pen_left: 0,
                        pen_top: 0,
                    },
                    dst_x: g.dst_x,
                    dst_y: g.dst_y,
                })
                .collect();
            crate::kms::vk::ops::text::record_text_run_scissored(
                &inner.vk,
                cb,
                &mut adapter,
                crate::kms::vk::ops::text::TextAtlas { extent: atlas_extent },
                pipeline,
                &glyphs_view,
                cg.foreground_rgba,
                &cg.clip_scissors,
            )?;
            drawable.storage.current_layout = adapter.current_layout;
            // Damage mutated at append time — not here. Spec
            // § "Damage accumulation" mandates append-time.
            Ok(())
        }
        Op::LayoutTransition(lt) => {
            // Unused in B.1; B.2+ paint ports may emit these.
            let drawable = store
                .get_mut(lt.drawable_id)
                .ok_or(RenderError::UnknownDrawable(lt.drawable_id))?;
            drawable.record_layout_transition(
                &inner.vk,
                cb,
                lt.target_layout,
                lt.src_stage,
                lt.src_access,
                lt.dst_stage,
                lt.dst_access,
            );
            Ok(())
        }
    }
}

/// Commit overlays + atlas inserts after a successful submit.
/// Called from `close_open_frame` ONLY after `flush_submit_group`
/// returned Ok. `take`d fields (layouts, touched, pending) cannot
/// be re-used by the caller — they're consumed here.
fn commit_close_success(
    inner: &mut RenderEngineInner,
    store: &mut DrawableStore,
    layouts: super::frame_builder::FrameLayoutTable,
    touched: super::frame_builder::TouchedDrawables,
    pending: super::frame_builder::PendingGlyphInserts,
    frame_ticket: &FenceTicket,
) {
    // 1. Drawable layouts: storage was already mutated by the recorder
    //    during the record pass (today's `record_text_run_scissored`
    //    via `StorageTextTarget` writes the adapter's current_layout
    //    back into storage.current_layout). The overlay is a snapshot
    //    for rollback only in B.1; commit here is a no-op for storage.
    //    (B.2+ may flip overlay-as-source-of-truth and add a
    //    `storage.current_layout = entry.current_in_frame_layout` write
    //    here; for B.1 the overlay just goes out of scope.)
    let _ = layouts;
    // 2. Touched drawables: each touched drawable's
    //    `last_render_ticket` was already set to the frame ticket at
    //    append-time via `store.touch_render_fence`. Commit is a no-op
    //    on storage; the overlay drops here (its snapshots are no
    //    longer needed because the frame succeeded).
    let _ = touched;
    let _ = frame_ticket; // (used below for atlas ticket-stamp)
    // 3. Commit pending glyph inserts.
    if let Some(atlas) = inner.glyph_atlas.as_mut() {
        for (key, entry) in pending.entries {
            atlas.insert_entry(key, entry);
        }
        // 4. Stamp atlas last_render_ticket.
        atlas.set_last_render_ticket(frame_ticket.clone());
    }
}

/// Roll storage back to pre-frame layouts when the close path
/// aborts (anywhere from `begin_op_cb` through `flush_submit_group`
/// returning Err). The recorder may have already mutated storage's
/// current_layout values; this walks the overlay and writes the
/// pre-frame snapshot back. Also restores the atlas's pre-frame
/// `last_render_ticket` snapshot. Damage is NOT rolled back (the
/// spec § "Damage accumulation" mandates this; DamageNotify has
/// already fired).
///
/// Called from every error path in `close_open_frame`. After this
/// runs, `renderer_failed` is fatal-after-failure (Phase A
/// discipline) and the diagnostic / shutdown readers see
/// pre-frame values.
fn rollback_pre_submit(
    store: &mut DrawableStore,
    open_frame: &mut super::frame_builder::OpenFrame,
) {
    // 1. Restore drawable layouts to pre_frame.
    for (id, entry) in open_frame.layouts.drawables.drain() {
        if let Some(d) = store.entries_get_mut(id) {
            d.storage.current_layout = entry.pre_frame_layout;
        }
    }
    // 2. Restore touched drawables' last_render_ticket to pre-frame.
    for (id, prior) in open_frame.touched.snapshots.drain() {
        if let Some(d) = store.entries_get_mut(id) {
            d.last_render_ticket = prior;
        }
    }
    // 3. Pin set + pending glyph inserts drop when `open_frame`
    //    goes out of scope at the caller. Atlas layout and atlas
    //    last_render_ticket restore via the helper below — needs
    //    the engine's atlas handle, so it's a separate call.
}

/// Atlas-side rollback. Separate from rollback_pre_submit because
/// it needs `&mut RenderEngineInner`. Caller invokes it once
/// inside a re-borrow scope.
fn rollback_atlas(
    inner: &mut RenderEngineInner,
    layouts_atlas: Option<super::frame_builder::LayoutOverlayEntry>,
    atlas_prev_ticket_snapshot: Option<Option<FenceTicket>>,
) {
    if let Some(atlas) = inner.glyph_atlas.as_mut() {
        if let Some(entry) = layouts_atlas {
            atlas.set_current_layout(entry.pre_frame_layout);
        }
        if let Some(prior) = atlas_prev_ticket_snapshot {
            match prior {
                Some(t) => atlas.set_last_render_ticket(t),
                None => atlas.clear_last_render_ticket(),
            }
        }
    }
}
```

Several APIs are referenced here that don't exist yet — add the trivial accessors:
- `FenceTicket::is_same_fence_as(&self, other: &FenceTicket) -> bool` — compare underlying fence handle for the debug_assert. If the existing `FenceTicket` is `Arc<Inner>`-shaped, this is `Arc::ptr_eq`.
- `V2GlyphAtlas::set_current_layout(&mut self, layout: vk::ImageLayout)` — straight setter; only used by commit/rollback. Public-ish so the engine can drive it.
- `V2GlyphAtlas::clear_last_render_ticket(&mut self)` — sets `self.last_render_ticket = None`.
- `DrawableStore::entries_get_mut(&mut self, id: DrawableId) -> Option<&mut Drawable>` — if the existing `DrawableStore::get_mut` doesn't expose the mutable view the commit/rollback walk needs, lift it here. Otherwise re-use `get_mut`.

- [ ] **Step 2: Add integration test (no synthetic op injection — the real test in Task 14 covers this)**

For Task 12, just compile-test that the close path's plumbing wires up:

```rust
// engine.rs unit-test module

#[test]
fn close_open_frame_with_no_open_frame_returns_already_closed() {
    let mut engine = RenderEngine::stub();
    let mut store = DrawableStore::stub();
    let mut platform = PlatformBackend::for_tests();
    let out = engine
        .close_open_frame(&mut store, &mut platform, super::frame_builder::CloseReason::Shutdown)
        .expect("ok");
    assert!(matches!(
        out,
        super::frame_builder::CloseOutcome::AlreadyClosed
    ));
}
```

- [ ] **Step 3: Build + run + commit**

```bash
cargo build -p yserver
cargo test -p yserver --lib kms::v2
```
Green. Commit:

```bash
git add crates/yserver/src/kms/v2/engine.rs crates/yserver/src/kms/v2/frame_builder.rs crates/yserver/src/kms/v2/glyph_atlas.rs crates/yserver/src/kms/v2/store.rs crates/yserver/src/kms/v2/platform.rs
git commit -m "feat(v2/engine): 3-pass close_open_frame walk (resource/record/finalise) — Phase B.1 Task 12"
```

---

### Task 13: Invariant M3 — `maybe_composite` closes the open frame before legacy compose flush

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs:4604-4615` — add the frame-close call before the existing `flush_submit_group(FlushReason::SceneCompose)`

The spec § "Migration boundaries" — Invariant M3: "Until sub-phase B.4 folds compose into the frame, the existing `maybe_composite` path (`backend.rs:4574`) records the compose CB outside the frame builder ... any open paint frame must close + commit BEFORE compose records."

- [ ] **Step 1: Add the close call**

```rust
// crates/yserver/src/kms/v2/backend.rs — inside maybe_composite,
// between the existing render-batch flush (line 4598-4603) and the
// SubmitGroup flush (line 4610):

// Phase B Invariant M3: close any open frame BEFORE legacy compose
// records. compose samples drawable storage at record time
// (scene.rs:1307), so the open frame's layout + ticket-touch overlays
// must be committed before the compose CB lands. Retires at sub-phase
// B.4 when compose itself ports into the frame builder.
if let Err(e) = self.engine.close_open_frame(
    &mut self.store,
    &mut self.platform,
    crate::kms::v2::frame_builder::CloseReason::LegacyScCompose,
) {
    log::warn!("v2 maybe_composite: close_open_frame failed: {e:?}");
}
```

- [ ] **Step 2: Wire a regression test**

```rust
// crates/yserver/tests/v2_acceptance.rs

#[test]
fn v2_maybe_composite_closes_frame_before_submit_group_flush() {
    let mut backend = KmsBackendV2::for_tests_with_vk().expect("vk");
    // Inject the feature gate so composite_glyphs routes through
    // the frame builder.
    backend.set_frame_builder_enabled_for_tests(true);
    // Drive composite_glyphs to open a frame.
    let dst = backend.allocate_test_pixmap_bgra(64, 32);
    backend.composite_glyphs_for_tests(
        dst,
        /* foreground = */ [1.0, 1.0, 1.0, 1.0],
        /* glyphs = */ &backend.synth_4_glyphs(),
        /* clip = */ None,
    ).expect("ok");
    assert!(backend.frame_builder_is_open_for_tests());
    // Drive a maybe_composite tick.
    backend.tick_maybe_composite_for_tests();
    assert!(!backend.frame_builder_is_open_for_tests(), "M3: maybe_composite must close the frame");
    assert_eq!(
        backend.frame_builder_lifetime_closes_for_tests(),
        1,
    );
}
```

The `*_for_tests` accessors are new wrapper methods on `KmsBackendV2` (in `backend.rs`) exposing engine internals to the integration test crate. Add them at the same time:
- `set_frame_builder_enabled_for_tests(b: bool)` — flips a runtime override on the engine
- `frame_builder_is_open_for_tests() -> bool` — `self.engine.frame_builder_is_open()`
- `frame_builder_lifetime_closes_for_tests() -> u64`
- `tick_maybe_composite_for_tests()` — calls `self.maybe_composite()`, ignoring the IO error result
- `composite_glyphs_for_tests(...)` — calls into `self.engine.composite_glyphs(&mut self.store, &mut self.platform, dst, fg, glyphs, clip)`
- `allocate_test_pixmap_bgra(w, h) -> DrawableId` — uses the existing `RenderEngine::create_pixmap` test helper through `KmsBackendV2`

- [ ] **Step 3: Run + commit**

```bash
cargo build -p yserver
cargo test -p yserver --test v2_acceptance v2_maybe_composite_closes_frame_before_submit_group_flush -- --nocapture
```

Green. Commit:

```bash
git add crates/yserver/src/kms/v2/backend.rs crates/yserver/tests/v2_acceptance.rs
git commit -m "feat(v2/backend): Invariant M3 — maybe_composite closes open frame before legacy compose (Phase B.1 Task 13)"
```

---

### Task 14: Invariant M2 — non-ported paint ops close the open frame first

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs` — add `RenderEngine::close_open_frame_for_non_ported_op` helper and call it at the top of every non-ported paint entry point

The 10 entry points needing M2 wiring (everything BUT `composite_glyphs` and `get_image`, which have their own dedicated close triggers):
- `fill_rect` (engine.rs:1207)
- `fill_rect_batch` (engine.rs:1233)
- `logic_fill` (engine.rs:1443)
- `copy_area` (engine.rs:1684)
- `cow_copy_area` (engine.rs:2022)
- `put_image` (engine.rs:3064)
- `image_text` (engine.rs:3412)
- `render_composite` (engine.rs:4086)
- `render_fill_rectangles` (engine.rs:4673)
- `render_traps_or_tris` (engine.rs:4739)

- [ ] **Step 1: Add the helper**

The helper is **a no-op when no frame is open**, so it preserves each non-ported op's existing batch-coalescing discipline (e.g. `render_composite`'s `try_append_render_batch` coalesce-on-key-match, `cow_copy_area`'s deliberate no-flush-of-existing-cow-batch). When a frame IS open (post-composite_glyphs), the helper flushes batches BEFORE closing — chronological order requires it.

Codex round-3 finding 1 flagged that unconditional batch flushes in M2 would disable render/COW coalescing for `render_composite` and `cow_copy_area`. The conditional-on-frame-open design avoids that.

```rust
// engine.rs — alongside flush_submit_group

impl RenderEngine {
    /// Phase B Invariant M2: close the open frame (if any) BEFORE a
    /// non-ported paint op records its own CB. The non-ported op
    /// samples committed `Drawable::storage.current_layout` and
    /// `last_render_ticket`; without the close, it would race against
    /// the deferred frame on the GPU. Retires when every paint op is
    /// ported (end of sub-phase B.3 at the latest).
    ///
    /// Fast path: no frame open → no-op. Preserves existing
    /// batch-coalescing discipline in `render_composite`,
    /// `cow_copy_area`, etc.
    ///
    /// Slow path: frame open → flush pre-existing batches first
    /// (chronological ordering: pre-frame batches must submit before
    /// the frame's CB), then close the frame. Each non-ported op's
    /// own batch prelude runs afterward against an empty batch state.
    pub(crate) fn close_open_frame_for_non_ported_op(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) -> Result<(), RenderError> {
        let frame_open = self
            .inner
            .as_ref()
            .is_some_and(|i| i.frame_builder.is_open());
        if !frame_open {
            return Ok(());
        }
        self.flush_cow_batch(store, platform)?;
        self.flush_render_batch(store, platform)?;
        match self.close_open_frame(
            store,
            platform,
            super::frame_builder::CloseReason::NonPortedPaintOp,
        )? {
            super::frame_builder::CloseOutcome::Submitted { .. }
            | super::frame_builder::CloseOutcome::AlreadyClosed => Ok(()),
        }
    }
}
```

- [ ] **Step 2: Wire into each of the 10 entry points**

At the *very top* of each paint op, BEFORE the existing legacy preludes that call `flush_cow_batch` + `flush_render_batch`, add:

```rust
self.close_open_frame_for_non_ported_op(store, platform)?;
```

**Do NOT delete the existing batch-prelude calls.** The helper is a no-op when no frame is open; when it IS open, it flushes batches first AND closes the frame, leaving the entry-point's existing batch logic to open fresh batches as needed. Each entry point's batch-coalescing semantics are preserved end-to-end.

Cite each of the 10 line ranges in the commit message so a bisect can spot a mis-wiring.

Example for `render_composite`:

```rust
// engine.rs:4113 (existing entry start)
pub(crate) fn render_composite(
    &mut self,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    op: u8,
    src: ResolvedSource,
    // …
) -> Result<CompositeStats, RenderError> {
    // Phase B Invariant M2: close any open composite_glyphs frame
    // first (no-op if no frame open). Preserves render-batch
    // coalescing in the common case where no frame was open.
    self.close_open_frame_for_non_ported_op(store, platform)?;
    // … rest of existing body, INCLUDING try_append_render_batch
    // coalescing logic, unchanged …
}
```

Repeat for the other 9. **Audit each existing prelude** during the port: the helper goes at the very top; everything else (existing batch logic, `renderer_failed` check, etc.) stays unchanged.

- [ ] **Step 3: Acceptance test**

```rust
// crates/yserver/tests/v2_acceptance.rs

#[test]
fn v2_non_ported_paint_op_closes_open_frame_first() {
    let mut backend = KmsBackendV2::for_tests_with_vk().expect("vk");
    backend.set_frame_builder_enabled_for_tests(true);
    let dst = backend.allocate_test_pixmap_bgra(128, 64);
    // Open a frame via composite_glyphs.
    backend.composite_glyphs_for_tests(dst, [1.0, 1.0, 1.0, 1.0], &backend.synth_4_glyphs(), None).expect("ok");
    assert!(backend.frame_builder_is_open_for_tests());
    // Issue any non-ported paint op (fill_rect is the smallest).
    backend.fill_rect_for_tests(dst, /* x= */ 0, /* y= */ 0, /* w= */ 8, /* h= */ 8, 0x00FFFFFF).expect("ok");
    assert!(
        !backend.frame_builder_is_open_for_tests(),
        "M2: non-ported paint op must close the frame first"
    );
    assert_eq!(backend.frame_builder_lifetime_closes_for_tests(), 1);
}
```

Add `fill_rect_for_tests` to `KmsBackendV2` mirroring the existing test surface.

- [ ] **Step 4: Run + commit**

```bash
cargo build -p yserver
cargo test -p yserver --test v2_acceptance v2_non_ported_paint_op_closes_open_frame_first -- --nocapture
cargo test -p yserver
```
All green. Commit:

```bash
git add crates/yserver/src/kms/v2/engine.rs crates/yserver/tests/v2_acceptance.rs
git commit -m "feat(v2/engine): Invariant M2 — non-ported paint ops close open frame first (10 entry points) — Phase B.1 Task 14"
```

---

### Task 15: Port `composite_glyphs` to FrameBuilder (feature-gated, **default OFF** in this task)

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs:3736-4039` — split into a legacy branch (unchanged) and a new `composite_glyphs_via_frame_builder` branch; flip on `frame_builder_enabled()`
- Modify: `crates/yserver/src/kms/v2/backend.rs` — runtime override accessor + env-var read at engine construction

**This task does NOT fix bee yet.** The frame builder code lands here but defaults to OFF. Tests flip it ON locally via `set_frame_builder_enabled_for_tests`. The actual production gate flip is Task 24 (after Tasks 16-21 wire every close trigger + telemetry); flipping there gives a single, well-attributed bee-fix commit.

- [ ] **Step 1: Decide gate semantics — default OFF in B.1 land, flip ON in Task 24**

Add `frame_builder_enabled: bool` to `RenderEngineInner`, initialized at engine construction from the env knob:

```rust
// engine.rs — RenderEngineInner

frame_builder_enabled: bool,

// RenderEngine::new initializer (B.1 land — default off):
frame_builder_enabled: std::env::var_os("YSERVER_FRAME_BUILDER")
    .as_deref()
    .and_then(|s| s.to_str())
    .is_some_and(|s| matches!(s, "1" | "on" | "true" | "yes")),
```

Default is **off** during Tasks 15-23. Task 24 flips the `map_or(false, ...)`-equivalent to `map_or(true, |s| !matches!(s, "0" | "off" | "false" | "no"))` (default-on with explicit-off escape hatch). Splitting the flip into its own commit makes the bee-fix attribution unambiguous in `git log` and `git bisect`.

Plus `pub(crate) fn set_frame_builder_enabled(&mut self, enabled: bool)` for test override.

- [ ] **Step 2: Write the integration test FIRST (TDD)**

```rust
// crates/yserver/tests/v2_acceptance.rs

#[test]
fn v2_frame_builder_composite_glyphs_one_submit() {
    let mut backend = KmsBackendV2::for_tests_with_vk().expect("vk");
    backend.set_frame_builder_enabled_for_tests(true);
    let dst = backend.allocate_test_pixmap_bgra(640, 480);
    // 32 glyph composite_glyphs call. Use the existing test
    // helper that fabricates 32 fake-r8-pixel glyphs at distinct
    // (gs_xid, glyph_id) keys so they all miss the atlas.
    let glyphs = backend.synth_32_glyphs_at_origin();
    let pre_submit_count = backend.platform_queue_submit2_count_for_tests();

    backend
        .composite_glyphs_for_tests(dst, [1.0, 1.0, 1.0, 1.0], &glyphs, None)
        .expect("ok");

    // Frame still open (composite_glyphs doesn't close on its own;
    // a maybe_composite tick or another trigger drives the close).
    assert!(backend.frame_builder_is_open_for_tests());

    // Drive the close.
    backend.tick_maybe_composite_for_tests();

    // Now exactly one new vkQueueSubmit2 should have happened for the
    // frame (which collapsed N glyph uploads + 1 draw into one CB
    // through SubmitGroup cap=1 auto-flush).
    let delta = backend.platform_queue_submit2_count_for_tests() - pre_submit_count;
    assert_eq!(
        delta,
        1,
        "Phase B.1: 32-glyph composite_glyphs collapses to 1 vkQueueSubmit2"
    );
}
```

`platform_queue_submit2_count_for_tests` exposes `crate::vk_count!` totals; reuse the existing `vk_count` accessor pattern (Phase A added per-call counters in `crate::kms::vk`). If the counter accessor doesn't exist, add it as a small `pub fn` on the count registry. Note that `maybe_composite` itself may emit a compose `vkQueueSubmit2`; the test should either subtract that off or use a wrapper that records "frame_builder-attributable submits" specifically. Simplest: query `inner.frame_seq` before and after (one increment = one frame submitted).

Refactor the assertion if needed:

```rust
let frame_seq_before = backend.engine_frame_seq_for_tests();
backend.composite_glyphs_for_tests(dst, [1.0, 1.0, 1.0, 1.0], &glyphs, None).expect("ok");
backend.tick_maybe_composite_for_tests();
let frame_seq_after = backend.engine_frame_seq_for_tests();
assert_eq!(frame_seq_after - frame_seq_before, 1, "exactly one frame submitted");
```

Both assertions belong in the test; the queue-submit2 delta is the load-bearing one (it proves the spec's quantitative target).

- [ ] **Step 3: Implement `composite_glyphs_via_frame_builder`**

In `engine.rs:3736`, replace the existing `composite_glyphs` body with a branching dispatch + the new frame-builder path. The frame-builder path MUST start by flushing pre-existing cow/render batches (codex round-1 finding 2 — composite_glyphs in the frame builder must preserve the legacy "flush batches first" discipline, otherwise a pre-opened cow batch's CBs would land out of chronological order relative to the new frame's draws).

```rust
pub(crate) fn composite_glyphs(
    &mut self,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    dst_id: DrawableId,
    foreground_rgba: [f32; 4],
    glyphs: &[CompositeGlyphInput<'_>],
    clip_rects: Option<&[Rectangle16]>,
) -> Result<ImageTextStats, RenderError> {
    // Branch on the gate. The legacy path is the existing
    // implementation moved into composite_glyphs_legacy; B.5 deletes
    // it together with SubmitGroup.
    if self
        .inner
        .as_ref()
        .is_some_and(|i| i.frame_builder_enabled)
    {
        self.composite_glyphs_via_frame_builder(
            store,
            platform,
            dst_id,
            foreground_rgba,
            glyphs,
            clip_rects,
        )
    } else {
        self.composite_glyphs_legacy(
            store,
            platform,
            dst_id,
            foreground_rgba,
            glyphs,
            clip_rects,
        )
    }
}

fn composite_glyphs_legacy(/* … existing body, unchanged …*/) -> Result<ImageTextStats, RenderError> {
    // existing engine.rs:3744-4039 body, copy-pasted verbatim
}

fn composite_glyphs_via_frame_builder(
    &mut self,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    dst_id: DrawableId,
    foreground_rgba: [f32; 4],
    glyphs: &[CompositeGlyphInput<'_>],
    clip_rects: Option<&[Rectangle16]>,
) -> Result<ImageTextStats, RenderError> {
    let mut stats = ImageTextStats::default();
    if glyphs.is_empty() {
        return Ok(stats);
    }

    // (0) **Flush pre-existing cow/render batches** before opening
    //     the frame. Same discipline as legacy composite_glyphs
    //     (engine.rs:3750-3751). If any non-frame-builder op left a
    //     batch open earlier in the tick, its CBs must submit BEFORE
    //     the frame's draws (chronological X11 order). With M2 wired
    //     on every non-ported paint op, batches normally close before
    //     a frame opens — but the M2 path keeps the frame OPEN across
    //     calls, so a sequence like
    //     `composite_glyphs → composite_glyphs` (no M2 fire) would
    //     still see batches empty. A sequence like
    //     `cow_copy_area → composite_glyphs` would see the cow batch
    //     pending; flush it here defensively.
    self.flush_cow_batch(store, platform)?;
    self.flush_render_batch(store, platform)?;

    // (1) Resolve dst format gating — identical to legacy.
    let Some(inner) = self.inner.as_mut() else {
        return Err(RenderError::NoVk);
    };
    if platform.renderer_failed {
        return Err(RenderError::RendererFailed);
    }
    let (dst_extent, dst_format) = {
        let d = store.get(dst_id).ok_or(RenderError::UnknownDrawable(dst_id))?;
        (d.storage.extent, d.storage.format)
    };
    if dst_format != vk::Format::B8G8R8A8_UNORM {
        log::warn!(
            "v2 composite_glyphs (frame_builder): dst xid={:?} has format {:?}; text \
             pipeline only supports B8G8R8A8_UNORM — dropping run",
            store.get(dst_id).map(|d| d.xid),
            dst_format,
        );
        return Ok(stats);
    }

    // (2) Lazy-init atlas + text pipeline — identical to legacy.
    if inner.glyph_atlas.is_none() {
        match V2GlyphAtlas::new(Arc::clone(&inner.vk)) {
            Ok(a) => inner.glyph_atlas = Some(a),
            Err(e) => {
                log::error!("v2 composite_glyphs: V2GlyphAtlas::new failed: {e:?}");
                return Err(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED));
            }
        }
    }
    if inner.text_pipeline.is_none() {
        let atlas_view = inner.glyph_atlas.as_ref().expect("just built").image_view();
        match TextPipeline::new(Arc::clone(&inner.vk), vk::Format::B8G8R8A8_UNORM, atlas_view) {
            Ok(p) => inner.text_pipeline = Some(p),
            Err(e) => {
                log::error!("v2 composite_glyphs: TextPipeline::new failed: {e:?}");
                return Err(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED));
            }
        }
    }

    // (3) Open the frame if not open.
    if !inner.frame_builder.is_open() {
        let ticket = platform.submit_group_ticket_or_open()?;
        inner.frame_builder.open_for_paint(ticket);
    }

    // (4) Ticket-touch dst + snapshot prior ticket (first-touch only)
    //     + FIRST-TOUCH dst layout overlay (codex round-1 finding 3
    //     fix — the overlay's pre_frame_layout snapshot is what
    //     `rollback_pre_submit` writes back on close-failure).
    let frame_ticket = inner
        .frame_builder
        .open
        .as_ref()
        .expect("just opened")
        .ticket
        .clone();
    let prior_dst_ticket = store
        .get(dst_id)
        .and_then(|d| d.last_render_ticket.clone());
    let dst_pre_frame_layout = store
        .get(dst_id)
        .map(|d| d.storage.current_layout)
        .unwrap_or(vk::ImageLayout::UNDEFINED);
    {
        let open = inner.frame_builder.open.as_mut().expect("just opened");
        open.touched.first_touch(dst_id, prior_dst_ticket);
        open.layouts.first_touch_drawable(dst_id, dst_pre_frame_layout);
    }
    store.touch_render_fence(dst_id, frame_ticket.clone());

    // (5) Snapshot atlas prev ticket + atlas layout (first-touch
    //     only). The atlas snapshot is the rollback target if the
    //     close fails AFTER any upload op recorded; record_upload
    //     mutates V2GlyphAtlas::current_layout in place.
    {
        let atlas_pre_ticket: Option<FenceTicket> = inner
            .glyph_atlas
            .as_ref()
            .and_then(|a| a.last_render_ticket().cloned());
        let atlas_pre_layout: vk::ImageLayout = inner
            .glyph_atlas
            .as_ref()
            .map(|a| a.current_layout())
            .unwrap_or(vk::ImageLayout::UNDEFINED);
        let open = inner.frame_builder.open.as_mut().expect("open");
        if open.atlas_prev_ticket_snapshot.is_none() {
            open.atlas_prev_ticket_snapshot = Some(atlas_pre_ticket);
            open.layouts.first_touch_atlas(atlas_pre_layout);
        }
    }
    // `V2GlyphAtlas::current_layout()` accessor is new — add a public
    // getter alongside the existing `image()` / `image_view()` /
    // `extent()` accessors in glyph_atlas.rs.

    // (6a) PRE-PASS — count UNIQUE atlas misses without
    //     packing/allocating. Codex round-3 finding 2: a call with
    //     repeated uncached keys would otherwise count N misses
    //     where one upload suffices, triggering premature
    //     close+reopens. Dedupe keys against (a) the committed
    //     atlas, (b) the frame's already-queued pending_glyph_inserts,
    //     and (c) prior misses in THIS pre-pass.
    let pending_pins_before_call = inner
        .frame_builder
        .open
        .as_ref()
        .map(|o| o.pins.len())
        .unwrap_or(0);
    let ceiling = inner.frame_builder.max_pinned_resources_per_frame();
    let mut prospective_miss_keys: std::collections::HashSet<GlyphKey> =
        std::collections::HashSet::new();
    for g in glyphs {
        let key = GlyphKey {
            font_xid: g.gs_xid,
            codepoint: g.glyph_id,
        };
        if g.w == 0 || g.h == 0 {
            continue;
        }
        // (a) committed atlas hit?
        if inner
            .glyph_atlas
            .as_ref()
            .expect("init")
            .lookup(key)
            .is_some()
        {
            continue;
        }
        // (b) pending insert already queued in the open frame?
        let pending_hit = inner
            .frame_builder
            .open
            .as_ref()
            .is_some_and(|o| o.pending_glyph_inserts.entries.iter().any(|(k, _)| *k == key));
        if pending_hit {
            continue;
        }
        // (c) duplicate within this call?
        prospective_miss_keys.insert(key);
    }
    let prospective_misses = prospective_miss_keys.len();
    let needs_close_reopen = pending_pins_before_call + prospective_misses > ceiling;
    if needs_close_reopen {
        // Force a close+reopen NOW (pre-allocation). Log the ceiling
        // hit once per process. Scope the inner borrow tightly so it
        // releases before we call &mut self methods.
        {
            inner
                .frame_builder
                .note_pin_ceiling_hit_once(pending_pins_before_call + prospective_misses);
        }
        // `inner` goes out of scope at the end of the surrounding
        // function block; explicit shadow-rebind below ensures the
        // borrow is released by THIS point. Using a `let _ = inner;`
        // is the conventional "I'm done with this reference" cue
        // without invoking `drop()` on a reference (which clippy can
        // warn about).
        let _ = inner; // signal: borrow ends here
        self.close_open_frame(
            store,
            platform,
            super::frame_builder::CloseReason::PinCeiling,
        )?;
        // Re-open fresh frame.
        let new_ticket = platform.submit_group_ticket_or_open()?;
        let inner = self.inner.as_mut().expect("inner");
        inner.frame_builder.open_for_paint(new_ticket);
        let frame_ticket_reopened = inner
            .frame_builder
            .open
            .as_ref()
            .expect("just opened")
            .ticket
            .clone();
        let dst_pre_layout_reopened = store
            .get(dst_id)
            .map(|d| d.storage.current_layout)
            .unwrap_or(vk::ImageLayout::UNDEFINED);
        let atlas_pre_layout_reopened = inner
            .glyph_atlas
            .as_ref()
            .map(|a| a.current_layout())
            .unwrap_or(vk::ImageLayout::UNDEFINED);
        let atlas_pre_ticket_reopened = inner
            .glyph_atlas
            .as_ref()
            .and_then(|a| a.last_render_ticket().cloned());
        let prior_dst_reopened = store
            .get(dst_id)
            .and_then(|d| d.last_render_ticket.clone());
        {
            let open = inner.frame_builder.open.as_mut().expect("open");
            open.touched.first_touch(dst_id, prior_dst_reopened);
            open.layouts.first_touch_drawable(dst_id, dst_pre_layout_reopened);
            open.atlas_prev_ticket_snapshot = Some(atlas_pre_ticket_reopened);
            open.layouts.first_touch_atlas(atlas_pre_layout_reopened);
        }
        store.touch_render_fence(dst_id, frame_ticket_reopened);
        // If the SINGLE call still exceeds the ceiling — drop excess
        // glyphs. The spec accepts atlas-slot leakage in the rare-
        // failure regime; we extend that to "pathological single call".
        if prospective_misses > ceiling {
            log::warn!(
                "v2 composite_glyphs (frame_builder): single call requested {} \
                 atlas misses but per-frame ceiling is {}; will drop excess",
                prospective_misses,
                ceiling,
            );
        }
    }
    // Re-acquire `inner` for the per-glyph walk below. (Whether or
    // not we closed-and-reopened, the `inner` borrow was scoped.)
    let inner = self.inner.as_mut().expect("inner");
    // Recompute pending_pins_before_call AFTER any close+reopen. On
    // reopen, pins start at zero; without the recompute, the per-
    // glyph guard below would use the stale pre-close value and
    // prematurely drop glyphs (codex round-3 finding 2a).
    let pending_pins_before_call = inner
        .frame_builder
        .open
        .as_ref()
        .map(|o| o.pins.len())
        .unwrap_or(0);

    // (6b) Per-glyph walk — actually allocate staging + pack atlas
    //      slots for each miss. Deduplicate against (a) committed
    //      atlas, (b) pending_glyph_inserts in the open frame,
    //      (c) new_uploads already collected in this walk. Stop
    //      allocating once the ceiling is hit (drop excess glyphs).
    let mut glyphs_to_draw: Vec<super::frame_builder::RecordedTextGlyph> = Vec::with_capacity(glyphs.len());
    let mut new_uploads: Vec<(GlyphKey, AtlasEntry, Arc<StagingBuffer>)> = Vec::new();
    let mut new_zero_inserts: Vec<(GlyphKey, AtlasEntry)> = Vec::new();
    let mut damage_min_x = i32::MAX;
    let mut damage_min_y = i32::MAX;
    let mut damage_max_x = i32::MIN;
    let mut damage_max_y = i32::MIN;
    for g in glyphs {
        let key = GlyphKey {
            font_xid: g.gs_xid,
            codepoint: g.glyph_id,
        };
        // (a) committed atlas hit?
        let committed_hit = inner.glyph_atlas.as_ref().expect("init").lookup(key);
        // (b) pending-insert hit in the open frame?
        let pending_hit = inner
            .frame_builder
            .open
            .as_ref()
            .and_then(|o| o.pending_glyph_inserts.entries.iter().find(|(k, _)| *k == key).map(|(_, e)| *e));
        // (c) new-uploads dedupe (same call earlier)?
        let dedupe_hit = new_uploads.iter().find(|(k, _, _)| *k == key).map(|(_, e, _)| *e);
        let entry = if let Some(hit) = committed_hit.or(pending_hit).or(dedupe_hit) {
            hit
        } else {
            // Zero-size glyphs use a degenerate entry; no atlas slot
            // is consumed (the legacy path packs them anyway but the
            // returned slot is unused; we skip pack here to avoid
            // wasting one row on the packer).
            if g.w == 0 || g.h == 0 {
                let e = AtlasEntry {
                    atlas_x: 0,
                    atlas_y: 0,
                    w: 0,
                    h: 0,
                    pen_left: 0,
                    pen_top: 0,
                };
                new_zero_inserts.push((key, e));
                continue;
            }
            // Pin-ceiling enforcement: check BEFORE calling pack() so
            // dropped glyphs don't leak atlas slots (codex round-4
            // finding: pack consumes a shelf advance regardless of
            // whether the glyph ends up uploaded).
            if new_uploads.len() + 1 + pending_pins_before_call > ceiling {
                stats.glyphs_dropped += 1;
                continue;
            }
            // Pre-validate pixels length BEFORE pack() to avoid leaking
            // a packed slot on malformed input (codex round-5
            // implementation note).
            let copy_len = (g.w as usize) * (g.h as usize);
            if g.pixels.len() < copy_len {
                log::warn!(
                    "v2 composite_glyphs (frame_builder): glyph pixels {} < {}; dropping pre-pack",
                    g.pixels.len(),
                    copy_len,
                );
                stats.glyphs_dropped += 1;
                continue;
            }
            let Some((atlas_x, atlas_y)) = inner.glyph_atlas.as_mut().expect("init").pack(g.w, g.h) else {
                inner.glyph_atlas.as_mut().expect("init").note_full_once();
                stats.glyphs_dropped += 1;
                continue;
            };
            stats.atlas_interns += 1;
            let upload_bytes = u64::from(g.w) * u64::from(g.h);
            let staging = Arc::new(StagingBuffer::new(
                Arc::clone(&inner.vk),
                upload_bytes.max(1),
            )?);
            let src_slice = &g.pixels[..copy_len];
            unsafe {
                std::ptr::copy_nonoverlapping(src_slice.as_ptr(), staging.mapped.as_ptr(), copy_len);
            }
            let new_entry = AtlasEntry {
                atlas_x,
                atlas_y,
                w: g.w,
                h: g.h,
                pen_left: 0,
                pen_top: 0,
            };
            new_uploads.push((key, new_entry, staging));
            stats.glyph_uploads += 1;
            new_entry
        };
        if entry.w == 0 || entry.h == 0 {
            continue;
        }
        damage_min_x = damage_min_x.min(g.dst_x);
        damage_min_y = damage_min_y.min(g.dst_y);
        let max_x = g.dst_x.saturating_add(entry.w as i32);
        let max_y = g.dst_y.saturating_add(entry.h as i32);
        damage_max_x = damage_max_x.max(max_x);
        damage_max_y = damage_max_y.max(max_y);
        glyphs_to_draw.push(super::frame_builder::RecordedTextGlyph {
            atlas_x: entry.atlas_x,
            atlas_y: entry.atlas_y,
            w: entry.w,
            h: entry.h,
            dst_x: g.dst_x,
            dst_y: g.dst_y,
        });
    }

    if glyphs_to_draw.is_empty() && new_uploads.is_empty() && new_zero_inserts.is_empty() {
        return Ok(stats);
    }

    // (6c) Commit new uploads + zero-inserts + glyph_uploads counter.
    //      Pin-ceiling enforcement happened in pre-pass + per-glyph
    //      drop above; we know new_uploads.len() ≤ ceiling - pending.
    {
        let open = inner.frame_builder.open.as_mut().expect("open");
        for (key, entry, staging) in new_uploads.drain(..) {
            let staging_pin_idx = open.pins.pin_staging(Arc::clone(&staging));
            open.ops
                .push(super::frame_builder::RecordedOp::GlyphUpload(
                    super::frame_builder::RecordedGlyphUpload {
                        staging_pin_idx,
                        atlas_x: entry.atlas_x,
                        atlas_y: entry.atlas_y,
                        w: entry.w,
                        h: entry.h,
                        insert_key: key,
                        insert_entry: entry,
                    },
                ));
            open.pending_glyph_inserts.push(key, entry);
            open.glyph_uploads_in_frame = open.glyph_uploads_in_frame.saturating_add(1);
        }
        for (key, entry) in new_zero_inserts.drain(..) {
            open.pending_glyph_inserts.push(key, entry);
        }
    }

    if glyphs_to_draw.is_empty() {
        return Ok(stats);
    }

    // (7) Build the clip scissor list — identical to legacy.
    let clip_scissors: Vec<vk::Rect2D> = match clip_rects {
        None => vec![vk::Rect2D { offset: vk::Offset2D::default(), extent: dst_extent }],
        Some(cr) => {
            let mut out = Vec::with_capacity(cr.len());
            for r in cr {
                if r.width == 0 || r.height == 0 {
                    continue;
                }
                let x0 = i32::from(r.x).max(0);
                let y0 = i32::from(r.y).max(0);
                let x1 = (i32::from(r.x) + i32::from(r.width)).min(i32::try_from(dst_extent.width).unwrap_or(i32::MAX));
                let y1 = (i32::from(r.y) + i32::from(r.height)).min(i32::try_from(dst_extent.height).unwrap_or(i32::MAX));
                if x1 <= x0 || y1 <= y0 {
                    continue;
                }
                out.push(vk::Rect2D {
                    offset: vk::Offset2D { x: x0, y: y0 },
                    extent: vk::Extent2D {
                        #[allow(clippy::cast_sign_loss)]
                        width: (x1 - x0) as u32,
                        #[allow(clippy::cast_sign_loss)]
                        height: (y1 - y0) as u32,
                    },
                });
            }
            if out.is_empty() {
                return Ok(stats);
            }
            out
        }
    };

    // (8) Append-time damage mutation. Spec § "Damage accumulation"
    //     mandates append-time mutation (the X11 client's
    //     XRender / XCopyArea / XPutImage request already happened
    //     the moment the server accepted it; DamageNotify fires on
    //     acceptance, before GPU work). Frame failure does NOT
    //     roll damage back — restoration would lose a DamageNotify
    //     the client has already been told about.
    if damage_max_x > damage_min_x && damage_max_y > damage_min_y {
        let dx = damage_min_x.max(0);
        let dy = damage_min_y.max(0);
        let w = u32::try_from(damage_max_x - dx).unwrap_or(0);
        let h = u32::try_from(damage_max_y - dy).unwrap_or(0);
        if w > 0 && h > 0 {
            store.damage(
                dst_id,
                clamp_rect(
                    vk::Rect2D {
                        offset: vk::Offset2D { x: dx, y: dy },
                        extent: vk::Extent2D { width: w, height: h },
                    },
                    dst_extent,
                ),
            );
        }
    }

    // (9) Append the draw op. No damage_rect carried — damage was
    //     already mutated at append time above. The
    //     RecordedCompositeGlyphs::damage_rect field stays as
    //     `None` for B.1; it's a reserved slot for B.2+ ops that
    //     may carry close-time-committed damage in the unlikely
    //     case append-time mutation isn't feasible.
    inner.frame_builder.open.as_mut().unwrap().ops.push(super::frame_builder::RecordedOp::CompositeGlyphs(
        super::frame_builder::RecordedCompositeGlyphs {
            dst_id,
            foreground_rgba,
            glyphs: glyphs_to_draw,
            clip_scissors,
            damage_rect: None,
        }
    ));

    // (10) Do NOT auto-close. Frame closes via M2 (next non-ported op),
    //      M3 (maybe_composite), timeout, sync_wait, or shutdown.
    Ok(stats)
}
```

The `Inner` borrow / re-borrow dance is tedious; restructure once the body compiles. Use `inner` as a local re-bind after each `&mut self`-taking helper call.

- [ ] **Step 4: Run the integration test**

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_composite_glyphs_one_submit -- --nocapture
```
Expected: PASS — 32 glyph uploads + 1 draw collapse into one `vkQueueSubmit2`.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/engine.rs crates/yserver/src/kms/v2/backend.rs crates/yserver/tests/v2_acceptance.rs
git commit -m "feat(v2/engine): port composite_glyphs to FrameBuilder behind YSERVER_FRAME_BUILDER gate (default OFF; Task 24 flips it ON) — Phase B.1 Task 15"
```

---

### Task 16: Close trigger — COW PRESENT-completion semaphore submit

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs:9413` (`KmsBackendV2::enqueue_present_completion`) — close any open frame BEFORE the existing `flush_submit_group(PresentCompletionSignal)` call that precedes `submit_present_completion_signal`

Codex round-1 finding 5: `attach_cow_present_completion` (engine.rs:2519) merely appends a completion event to a pending batch; it has no `&mut store` / `&mut platform` parameters and no semaphore-export side effects. The actual semaphore-export path that needs the close trigger is `KmsBackendV2::enqueue_present_completion` around backend.rs:9413, which:
1. Flushes the cow batch (line 9401-9406)
2. Flushes the render batch (line 9407-9412)
3. Flushes the SubmitGroup with `FlushReason::PresentCompletionSignal` (line 9413-9420)
4. Acquires a fresh `PresentCompletionSignal` semaphore + ticket
5. Calls `submit_present_completion_signal(...)` which issues a signal-only `vkQueueSubmit2`
6. Calls `signal.export_sync_file_fd()` to get the SYNC_FD for `PendingPresentBatch`

The frame-builder close must fire BETWEEN step 3 (the SubmitGroup flush of pre-existing batch work) and step 4 (so the semaphore the close's frame submits AGAINST is the same one that signals here). Actually no — the close itself drives ITS OWN `flush_submit_group(FrameBuilder)`. The right ordering is:

1. flush_cow_batch (existing)
2. flush_render_batch (existing)
3. **close_open_frame(PresentCompletionSignal)** ← NEW
4. flush_submit_group(PresentCompletionSignal) (existing — typically a no-op after the close drained everything, but keep it as the canonical "no buffered paint CBs before signal-only submit" gate)
5. Acquire signal + ticket + submit_present_completion_signal
6. export_sync_file_fd

That keeps the chronological "all paint CBs submit before the signal-only submit" invariant Phase A's Task 6.1 already established.

Spec § "Frame close triggers" — trigger 1b: same VUID-VkFenceGetFdInfoKHR-handleType-01457 / Task 6.1 yoga hang rationale.

- [ ] **Step 1: Wire the close trigger into `enqueue_present_completion`**

```rust
// backend.rs:9412-9413, between flush_render_batch and flush_submit_group:

// Phase B.1 close trigger 1b: close any open frame before the
// signal-only submit so the semaphore-export's SYNC_FD captures a
// queued signal-op for ANY paint work that came through the frame
// builder. Same hazard as Task 6.1 (VUID-VkFenceGetFdInfoKHR-handleType-01457).
if let Err(e) = self.engine.close_open_frame(
    &mut self.store,
    &mut self.platform,
    crate::kms::v2::frame_builder::CloseReason::PresentCompletionSignal,
) {
    log::warn!("v2 enqueue_present_completion: close_open_frame failed: {e:?}");
}
```

- [ ] **Step 2: Q5 semaphore-export pass-through test**

Spec § Open question 5 calls for an explicit pass-through test for the submit-then-export shape. Add an integration test that:
1. Opens a frame via composite_glyphs.
2. Triggers `enqueue_present_completion` on the dst.
3. Asserts the resulting `PendingPresentBatch::wait` is `PresentBatchWait::Fd(_)` (NOT `PresentBatchWait::Poll`), meaning the SYNC_FD export succeeded (which it can only do if the semaphore had a queued signal-op when `vkGetSemaphoreFdKHR(SYNC_FD)` was called).

```rust
#[test]
fn v2_frame_builder_closes_on_present_completion_and_exports_sync_fd() {
    let mut backend = KmsBackendV2::for_tests_with_vk().expect("vk");
    backend.set_frame_builder_enabled_for_tests(true);
    let dst = backend.allocate_test_pixmap_bgra(64, 64);
    backend.composite_glyphs_for_tests(dst, [1.0, 1.0, 1.0, 1.0], &backend.synth_4_glyphs(), None).expect("ok");
    assert!(backend.frame_builder_is_open_for_tests());
    // Drive the COW present completion path.
    let batch = backend.enqueue_present_completion_for_tests(dst);
    assert!(!backend.frame_builder_is_open_for_tests(), "close trigger fired");
    assert!(
        matches!(batch.wait, PresentBatchWait::Fd(_)),
        "SYNC_FD export succeeded → semaphore had queued signal-op when polled"
    );
}
```

`enqueue_present_completion_for_tests` wraps the existing `enqueue_present_completion` method on `KmsBackendV2`, returning the last-registered `PendingPresentBatch` for inspection. Add this accessor in the test-only block on `KmsBackendV2`.

- [ ] **Step 3: Run + commit**

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_closes_on_present_completion_and_exports_sync_fd -- --nocapture
git add crates/yserver/src/kms/v2/backend.rs crates/yserver/tests/v2_acceptance.rs
git commit -m "feat(v2/backend): frame-builder close trigger 1b at enqueue_present_completion + Q5 SYNC_FD pass-through test (Phase B.1 Task 16)"
```

---

### Task 17: Close trigger — `get_image` sync wait

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs:3228` (`get_image`) — close any open frame BEFORE the readback `ticket.wait()`

Spec § "Frame close triggers" — trigger 2: `get_image` is the only `ticket.wait()` site after Task 15. The frame-builder close must commit before the readback CB starts recording.

- [ ] **Step 1: Add the close**

```rust
// engine.rs:3228 — top of get_image, BEFORE the existing
// flush_submit_group(SyncBoundary) call:

self.close_open_frame(
    store,
    platform,
    super::frame_builder::CloseReason::SyncWait,
)?;
// existing flush_submit_group(SyncBoundary) becomes a no-op when the
// SubmitGroup is empty (which it is after our close), so leave it in
// place for B.2+ paths where unported ops are still appending.
```

- [ ] **Step 2: Regression test**

```rust
#[test]
fn v2_frame_builder_closes_on_get_image() {
    let mut backend = KmsBackendV2::for_tests_with_vk().expect("vk");
    backend.set_frame_builder_enabled_for_tests(true);
    let dst = backend.allocate_test_pixmap_bgra(64, 64);
    backend.composite_glyphs_for_tests(dst, [1.0, 1.0, 1.0, 1.0], &backend.synth_4_glyphs(), None).expect("ok");
    assert!(backend.frame_builder_is_open_for_tests());
    let _img = backend.get_image_for_tests(dst, /* x = */ 0, /* y = */ 0, /* w = */ 64, /* h = */ 64).expect("ok");
    assert!(!backend.frame_builder_is_open_for_tests());
}
```

- [ ] **Step 3: Run + commit**

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_closes_on_get_image -- --nocapture
git add crates/yserver/src/kms/v2/engine.rs crates/yserver/tests/v2_acceptance.rs
git commit -m "feat(v2/engine): frame-builder close trigger 2 — get_image sync wait (Phase B.1 Task 17)"
```

---

### Task 18: Close trigger — timeout (16 ms default, `YSERVER_FRAME_BUILDER_TIMEOUT_MS` env knob)

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs` — track `opened_at: Instant` on `OpenFrame`
- Modify: `crates/yserver/src/kms/v2/backend.rs:4574-4689` — at the top of `maybe_composite` (before the existing scene-compose can_submit_scene branch), if a frame has been open > timeout, drive a `Timeout` close
- Modify: `crates/yserver/src/kms/v2/engine.rs` — `RenderEngine::close_open_frame_if_timed_out` helper

Spec § "Frame close triggers" — trigger 4: "A frame that's been open for > T ms without a ready output forces a close to release pinned resources. Default T = 16 ms (one vblank at 60 Hz). Configurable via env knob `YSERVER_FRAME_BUILDER_TIMEOUT_MS`."

- [ ] **Step 1: Track open time**

```rust
// frame_builder.rs

use std::time::{Duration, Instant};

#[derive(Debug)]
pub(crate) struct OpenFrame {
    // … existing fields …
    pub(crate) opened_at: Instant,
}

impl FrameBuilder {
    pub(crate) fn open_for_paint(&mut self, ticket: FenceTicket) {
        // … existing body …
        self.open = Some(Box::new(OpenFrame {
            // …
            opened_at: Instant::now(),
            // …
        }));
    }

    pub(crate) fn open_for_at_least(&self, dur: Duration) -> bool {
        match self.open.as_ref() {
            None => false,
            Some(o) => o.opened_at.elapsed() >= dur,
        }
    }
}

impl FrameBuilder {
    /// Read once at engine construction; cached so the env-var read
    /// isn't a hot-path call.
    pub(crate) fn timeout_from_env_default_16ms() -> Duration {
        let ms = std::env::var("YSERVER_FRAME_BUILDER_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(16);
        Duration::from_millis(ms)
    }
}
```

Cache the duration on `RenderEngineInner::frame_builder_timeout: Duration` next to `frame_builder_enabled`.

- [ ] **Step 2: Wire the close-if-timed-out check into maybe_composite**

```rust
// backend.rs — at the very top of maybe_composite, before
// telemetry.advance_frame():

if self
    .engine
    .frame_builder_open_for_at_least_timeout_for_tests()
{
    if let Err(e) = self.engine.close_open_frame(
        &mut self.store,
        &mut self.platform,
        crate::kms::v2::frame_builder::CloseReason::Timeout,
    ) {
        log::warn!("v2 maybe_composite: frame builder timeout close failed: {e:?}");
    }
}
```

(The `_for_tests` suffix is misleading; rename to `frame_builder_open_for_at_least_timeout()` — it's a production-side check.)

- [ ] **Step 3: Regression test using a manual time-travel knob**

```rust
#[test]
fn v2_frame_builder_timeout_closes_after_threshold() {
    std::env::set_var("YSERVER_FRAME_BUILDER_TIMEOUT_MS", "1");
    let mut backend = KmsBackendV2::for_tests_with_vk().expect("vk");
    backend.set_frame_builder_enabled_for_tests(true);
    let dst = backend.allocate_test_pixmap_bgra(64, 64);
    backend.composite_glyphs_for_tests(dst, [1.0, 1.0, 1.0, 1.0], &backend.synth_4_glyphs(), None).expect("ok");
    assert!(backend.frame_builder_is_open_for_tests());
    std::thread::sleep(std::time::Duration::from_millis(5));
    backend.tick_maybe_composite_for_tests();
    assert!(!backend.frame_builder_is_open_for_tests());
    std::env::remove_var("YSERVER_FRAME_BUILDER_TIMEOUT_MS");
}
```

(Process-wide env var mutation — fine for a serially-run integration test crate; if cargo test ever runs them in parallel, add `#[serial]` via the `serial_test` crate or refactor to read the duration from a constructor arg.)

- [ ] **Step 4: Run + commit**

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_timeout_closes_after_threshold -- --nocapture
git add crates/yserver/src/kms/v2/frame_builder.rs crates/yserver/src/kms/v2/engine.rs crates/yserver/src/kms/v2/backend.rs crates/yserver/tests/v2_acceptance.rs
git commit -m "feat(v2/frame_builder): close trigger 4 — timeout (16 ms default, env knob) — Phase B.1 Task 18"
```

---

### Task 19: Close trigger — shutdown (add `RenderEngine::shutdown`, keep `drain_all` intact)

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs` — add new method `RenderEngine::shutdown(&mut self, store, platform)`; do NOT change `drain_all`'s signature
- Modify: `crates/yserver/src/kms/v2/engine.rs:5767-5785` (`impl Drop for RenderEngine`) — also wait on `pending_frames` ticket and drop records
- Modify: `crates/yserver/src/kms/v2/backend.rs:2074` (and the other backend.rs site found by `grep -n "engine.drain_all" crates/yserver/src/kms/v2/backend.rs`) — call `shutdown` instead of `drain_all`

Codex round-1 finding 8: `drain_all` has 47 call sites (45 of them in engine.rs unit tests, 2 in backend.rs production). Changing its signature is too disruptive. Better: introduce a NEW shutdown-with-store method; have production callers use it; tests keep the simpler `drain_all` form (none of them have an open frame to worry about).

Spec § "Frame close triggers" — trigger 5: shutdown closes any open frame before tearing down platform state.

- [ ] **Step 1: Add the new method**

```rust
// engine.rs — alongside drain_all

impl RenderEngine {
    /// Phase B.1: production-side shutdown. Closes any open frame
    /// first, then defers to `drain_all` for the existing
    /// SubmitGroup + submitted-queue + pending_frames drain.
    ///
    /// Test call sites that construct a fresh engine/platform/store
    /// and never open a frame can keep using `drain_all` directly.
    pub(crate) fn shutdown(
        &mut self,
        store: &mut DrawableStore,
        platform: &mut PlatformBackend,
    ) {
        if let Err(e) = self.close_open_frame(
            store,
            platform,
            super::frame_builder::CloseReason::Shutdown,
        ) {
            log::warn!("v2 shutdown: close_open_frame failed: {e:?}");
        }
        self.drain_all(platform);
    }
}
```

- [ ] **Step 2: Update production callers**

```bash
grep -n "engine.drain_all\|self\.engine\.drain_all" /home/jos/Projects/yserver/crates/yserver/src/kms/v2/backend.rs
```

Replace those 2 production calls with `engine.shutdown(&mut self.store, &mut self.platform)`. Leave the 45 test call sites alone — they're test-side scaffolding that constructs fresh engines/platforms without ever opening a frame.

- [ ] **Step 3: Update `drain_all` to also drain `pending_frames`**

The existing `drain_all` (engine.rs:706-751) drains `pending_group_ops` (via the SubmitGroup `Shutdown` flush) and the `submitted` queue. With Task 9 adding `pending_frames`, `drain_all` must wait on each `FrameSubmittedRecord`'s ticket too (so the pinned `Arc<StagingBuffer>`s drop only after the GPU finishes reading them).

```rust
// engine.rs:706 drain_all — append after the existing while-let loop over `submitted`:

while let Some(mut record) = inner.pending_frames.pop_front() {
    let _ = record.ticket.wait(&inner.vk);
    // record drops here; pins drop; Arcs decrement.
    drop(record);
}
```

- [ ] **Step 4: Update `Drop for RenderEngine` (engine.rs:5767-5785)**

Today's `Drop` waits on each `submitted.ticket` and drops staging. With `pending_frames` added, do the same for it. The `Drop` runs after `drain_all`/`shutdown` in well-behaved teardown, so `pending_frames` is usually empty — this is a defensive cleanup for the irregular-teardown path.

```rust
impl Drop for RenderEngine {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            for op in inner.submitted.drain(..) {
                let _ = op.ticket.wait(&inner.vk);
                drop(op.staging);
                let _ = op.cb;
            }
            for mut record in inner.pending_frames.drain(..) {
                let _ = record.ticket.wait(&inner.vk);
                drop(record); // pins (Arcs) decrement here
            }
        }
    }
}
```

- [ ] **Step 5: Run + commit**

```bash
cargo build -p yserver
cargo test -p yserver
```
All Phase A tests still green (they use `drain_all` directly). Commit:

```bash
git add crates/yserver/src/kms/v2/engine.rs crates/yserver/src/kms/v2/backend.rs
git commit -m "feat(v2/engine): RenderEngine::shutdown closes open frame; drain_all+Drop also drain pending_frames (Phase B.1 Task 19)"
```

---

### Task 20: Close trigger — pin-set ceiling (`max_pinned_resources_per_frame = 1024`, log-once)

**Files:**
- Modify: `crates/yserver/src/kms/v2/frame_builder.rs` — `OpenFrame` carries a `pin_ceiling_logged_once: bool` so the warning only emits once per process lifetime

Spec § "Frame close triggers" — trigger 6: "`max_pinned_resources_per_frame` = 1024 (initial guess, retune from telemetry). Catches a pathological glyph-storm or self-aliasing-readback loop that would otherwise exhaust descriptor pool / staging pools between vblanks. Reaching the ceiling forces a close and logs `frame_builder: pin set ceiling at {n}` once."

The check is already wired in Task 15 inside `composite_glyphs_via_frame_builder` (the `would_exceed_pin_ceiling` test before pinning a new staging buffer). Task 20 adds the once-per-process log and a regression test that artificially lowers the ceiling.

- [ ] **Step 1: Add the log-once latch**

```rust
// frame_builder.rs

pub(crate) struct FrameBuilder {
    // … existing fields …
    pin_ceiling_warned: bool,
}

impl FrameBuilder {
    pub(crate) fn note_pin_ceiling_hit_once(&mut self, n: usize) {
        if !self.pin_ceiling_warned {
            log::warn!("frame_builder: pin set ceiling at {} — forcing close", n);
            self.pin_ceiling_warned = true;
        }
    }
}
```

Call `inner.frame_builder.note_pin_ceiling_hit_once(...)` inside the ceiling branch in `composite_glyphs_via_frame_builder` (Task 15) before `close_open_frame(...)`.

- [ ] **Step 2: Regression test**

```rust
#[test]
fn v2_frame_builder_pin_ceiling_force_closes_and_logs_once() {
    let mut backend = KmsBackendV2::for_tests_with_vk().expect("vk");
    backend.set_frame_builder_enabled_for_tests(true);
    backend.set_frame_builder_pin_ceiling_for_tests(4); // tiny ceiling
    let dst = backend.allocate_test_pixmap_bgra(256, 256);
    // 5 unique glyphs ⇒ 5 staging pins ⇒ ceiling triggers on the 5th.
    let glyphs = backend.synth_n_unique_glyphs(5);
    backend.composite_glyphs_for_tests(dst, [1.0, 1.0, 1.0, 1.0], &glyphs, None).expect("ok");
    // Frame should have closed at least once mid-call.
    assert!(backend.frame_builder_lifetime_closes_for_tests() >= 1);
}
```

- [ ] **Step 3: Run + commit**

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_pin_ceiling_force_closes_and_logs_once -- --nocapture
git add crates/yserver/src/kms/v2/frame_builder.rs crates/yserver/src/kms/v2/engine.rs crates/yserver/tests/v2_acceptance.rs
git commit -m "feat(v2/frame_builder): close trigger 6 — pin-set ceiling 1024 + log-once (Phase B.1 Task 20)"
```

---

### Task 21: Telemetry — `frame_builder_*` counters

**Files:**
- Modify: `crates/yserver/src/kms/v2/telemetry.rs` — new fields on `Bucket`, new `record_frame_builder_*` methods, new emit-line in `maybe_emit`
- Modify: `crates/yserver/src/kms/v2/engine.rs` — call `telemetry.record_frame_builder_close(...)` at the end of `close_open_frame` (success path) and `record_frame_builder_abort()` on the failure path
- Modify: `crates/yserver/src/kms/v2/backend.rs` — sample the open frame's pin count high water in `maybe_composite`'s telemetry-drain block

Spec § "Telemetry" lists the counter set.

- [ ] **Step 1: Failing telemetry tests**

```rust
// telemetry.rs — extend Bucket

pub struct Bucket {
    // … existing …
    pub(crate) frame_builder_opens: u64,
    pub(crate) frame_builder_closes: u64,
    pub(crate) frame_builder_close_reason_scene_compose: u64,
    pub(crate) frame_builder_close_reason_non_ported_paint_op: u64,
    pub(crate) frame_builder_close_reason_legacy_sc_compose: u64,
    pub(crate) frame_builder_close_reason_present_completion_signal: u64,
    pub(crate) frame_builder_close_reason_sync_wait: u64,
    pub(crate) frame_builder_close_reason_timeout: u64,
    pub(crate) frame_builder_close_reason_shutdown: u64,
    pub(crate) frame_builder_close_reason_pin_ceiling: u64,
    pub(crate) frame_builder_ops_per_frame_total: u64,
    pub(crate) frame_builder_ops_per_frame_max_in_window: u64,
    pub(crate) frame_builder_ops_per_frame_hist: [u64; 8],
    pub(crate) frame_builder_active_pins_high_water: u64,
    pub(crate) frame_builder_aborts: u64,
    pub(crate) frame_builder_glyph_uploads_per_frame_total: u64,
    pub(crate) frame_builder_glyph_uploads_per_frame_max_in_window: u64,
}

impl Telemetry {
    pub(crate) fn record_frame_builder_open(&mut self) {
        self.bucket.frame_builder_opens += 1;
        self.lifetime.frame_builder_opens += 1;
    }

    pub(crate) fn record_frame_builder_close(
        &mut self,
        reason: super::frame_builder::CloseReason,
        ops_in_frame: usize,
        glyph_uploads_in_frame: u32,
    ) {
        self.bucket.frame_builder_closes += 1;
        self.lifetime.frame_builder_closes += 1;
        let ops = u64::try_from(ops_in_frame).unwrap_or(u64::MAX);
        self.bucket.frame_builder_ops_per_frame_total += ops;
        self.lifetime.frame_builder_ops_per_frame_total += ops;
        if ops > self.bucket.frame_builder_ops_per_frame_max_in_window {
            self.bucket.frame_builder_ops_per_frame_max_in_window = ops;
        }
        if ops > self.lifetime.frame_builder_ops_per_frame_max_in_window {
            self.lifetime.frame_builder_ops_per_frame_max_in_window = ops;
        }
        let hist_idx = match ops_in_frame {
            0 | 1 => 0,
            2..=3 => 1,
            4..=7 => 2,
            8..=15 => 3,
            16..=31 => 4,
            32..=63 => 5,
            64..=127 => 6,
            _ => 7,
        };
        self.bucket.frame_builder_ops_per_frame_hist[hist_idx] += 1;
        self.lifetime.frame_builder_ops_per_frame_hist[hist_idx] += 1;
        let uploads = u64::from(glyph_uploads_in_frame);
        self.bucket.frame_builder_glyph_uploads_per_frame_total += uploads;
        self.lifetime.frame_builder_glyph_uploads_per_frame_total += uploads;
        if uploads > self.bucket.frame_builder_glyph_uploads_per_frame_max_in_window {
            self.bucket.frame_builder_glyph_uploads_per_frame_max_in_window = uploads;
        }
        if uploads > self.lifetime.frame_builder_glyph_uploads_per_frame_max_in_window {
            self.lifetime.frame_builder_glyph_uploads_per_frame_max_in_window = uploads;
        }
        use super::frame_builder::CloseReason as R;
        let (b, l) = match reason {
            R::SceneCompose => (&mut self.bucket.frame_builder_close_reason_scene_compose, &mut self.lifetime.frame_builder_close_reason_scene_compose),
            R::NonPortedPaintOp => (&mut self.bucket.frame_builder_close_reason_non_ported_paint_op, &mut self.lifetime.frame_builder_close_reason_non_ported_paint_op),
            R::LegacyScCompose => (&mut self.bucket.frame_builder_close_reason_legacy_sc_compose, &mut self.lifetime.frame_builder_close_reason_legacy_sc_compose),
            R::PresentCompletionSignal => (&mut self.bucket.frame_builder_close_reason_present_completion_signal, &mut self.lifetime.frame_builder_close_reason_present_completion_signal),
            R::SyncWait => (&mut self.bucket.frame_builder_close_reason_sync_wait, &mut self.lifetime.frame_builder_close_reason_sync_wait),
            R::Timeout => (&mut self.bucket.frame_builder_close_reason_timeout, &mut self.lifetime.frame_builder_close_reason_timeout),
            R::Shutdown => (&mut self.bucket.frame_builder_close_reason_shutdown, &mut self.lifetime.frame_builder_close_reason_shutdown),
            R::PinCeiling => (&mut self.bucket.frame_builder_close_reason_pin_ceiling, &mut self.lifetime.frame_builder_close_reason_pin_ceiling),
        };
        *b += 1;
        *l += 1;
    }

    pub(crate) fn record_frame_builder_abort(&mut self) {
        self.bucket.frame_builder_aborts += 1;
        self.lifetime.frame_builder_aborts += 1;
    }

    pub(crate) fn record_frame_builder_active_pins_high_water(&mut self, n: u64) {
        if n > self.bucket.frame_builder_active_pins_high_water {
            self.bucket.frame_builder_active_pins_high_water = n;
        }
        if n > self.lifetime.frame_builder_active_pins_high_water {
            self.lifetime.frame_builder_active_pins_high_water = n;
        }
    }
}
```

- [ ] **Step 2: Wire into engine.close_open_frame**

```rust
// engine.rs — in close_open_frame's Ok(()) finalise branch, after the
// FrameBuilder::complete_close_success() call:

// Pull the engine's Telemetry reference. close_open_frame doesn't
// have it today — pass `telemetry: &mut Telemetry` as an extra arg
// from every caller, OR move the telemetry into RenderEngineInner.
// Path B (move into engine) avoids a 10-callsite signature churn.
// Pick Path B and add `telemetry_proxy: TelemetryProxy` (a tiny shim
// that defers to a callback the backend installs) or simpler: have
// the backend drain a `pending_frame_close_events: Vec<FrameCloseEvent>`
// queue from engine after each `maybe_composite` tick.

// Use the queue-drain pattern (mirrors the existing
// pending_flush_outcomes path):
inner.pending_frame_close_events.push(super::frame_builder::FrameCloseEvent {
    reason,
    ops_in_frame: open_frame.ops.len(),
    glyph_uploads_in_frame: open_frame.glyph_uploads_in_frame,
    pin_count: /* pre-move pin count */,
});
```

Define `FrameCloseEvent` in `frame_builder.rs`:

```rust
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameCloseEvent {
    pub(crate) reason: CloseReason,
    pub(crate) ops_in_frame: usize,
    pub(crate) glyph_uploads_in_frame: u32,
    pub(crate) pin_count: usize,
}
```

Add `pending_frame_close_events: Vec<FrameCloseEvent>` to `RenderEngineInner` + `drain_frame_close_events()` on `RenderEngine`. **Add a cap of 1024 events** — push past the cap drops the oldest with a once-per-process `log::warn!`; this prevents unbounded growth if a backend pause keeps `maybe_composite` from draining.

Wrap the drain into a backend helper that pushes each event into telemetry, and call it from EVERY site where a frame can close — codex round-2 finding 4 flagged that one-site (maybe_composite) draining loses events if get_image / present-completion / shutdown drives a close without a subsequent maybe_composite tick.

```rust
// backend.rs — new helper

impl KmsBackendV2 {
    fn drain_frame_builder_telemetry(&mut self) {
        for event in self.engine.drain_frame_close_events() {
            self.telemetry.record_frame_builder_close(
                event.reason,
                event.ops_in_frame,
                event.glyph_uploads_in_frame,
            );
            self.telemetry.record_frame_builder_active_pins_high_water(
                u64::try_from(event.pin_count).unwrap_or(u64::MAX),
            );
        }
    }
}
```

Call sites that drive a close MUST drain immediately after:
- `maybe_composite` — at the end (after `drain_flush_outcomes`, same place as the original Task 21 sketch). This is the steady-state drain.
- `KmsBackendV2::enqueue_present_completion` (Task 16) — call after the close trigger fires.
- `KmsBackendV2::get_image` wrapper (Task 17) — call after the close trigger.
- `KmsBackendV2::shutdown` (Task 19) — call BEFORE returning so shutdown-driven close events make it into the lifetime telemetry.
- **`KmsBackendV2::render_composite_glyphs` wrapper** (backend.rs:8193) — codex round-3 finding 3: composite_glyphs can drive a pin-ceiling close INSIDE the engine call (Task 15 step 6a's pre-pass close+reopen) before returning to the backend. Without this drain, repeated pin-ceiling fires can leave close events queued until a later maybe_composite tick (or eventually hit the 1024-event cap and undercount exactly the condition the telemetry is meant to expose).
- Every non-ported paint op wrapper on `KmsBackendV2` that re-enters from the backend → engine boundary AND could drive a close via M2. Simplest implementation: factor the drain into a `with_telemetry_drain` closure wrapper if the backend has many such methods; otherwise call it explicitly after each engine call that can close a frame.

Same pattern for `record_frame_builder_open` (push an event on `FrameBuilder::open_for_paint`) and `record_frame_builder_abort` (on close-failure).

- [ ] **Step 3: Emit lines in maybe_emit**

Add to the existing `maybe_emit` log block:

```rust
log::info!(
    "v2_telemetry: frame_builder_opens={} closes={} aborts={} ops/frame_avg={:.1} max={} hist={:?} glyph_uploads/frame_avg={:.1} max={} active_pins_hw={} \
     close_reasons[scene_compose={} non_ported={} legacy_sc={} present_completion={} sync_wait={} timeout={} shutdown={} pin_ceiling={}]",
    self.bucket.frame_builder_opens,
    self.bucket.frame_builder_closes,
    self.bucket.frame_builder_aborts,
    self.bucket.frame_builder_ops_per_frame_total as f64 / self.bucket.frame_builder_closes.max(1) as f64,
    self.bucket.frame_builder_ops_per_frame_max_in_window,
    self.bucket.frame_builder_ops_per_frame_hist,
    self.bucket.frame_builder_glyph_uploads_per_frame_total as f64 / self.bucket.frame_builder_closes.max(1) as f64,
    self.bucket.frame_builder_glyph_uploads_per_frame_max_in_window,
    self.bucket.frame_builder_active_pins_high_water,
    self.bucket.frame_builder_close_reason_scene_compose,
    self.bucket.frame_builder_close_reason_non_ported_paint_op,
    self.bucket.frame_builder_close_reason_legacy_sc_compose,
    self.bucket.frame_builder_close_reason_present_completion_signal,
    self.bucket.frame_builder_close_reason_sync_wait,
    self.bucket.frame_builder_close_reason_timeout,
    self.bucket.frame_builder_close_reason_shutdown,
    self.bucket.frame_builder_close_reason_pin_ceiling,
);
```

- [ ] **Step 4: Acceptance test**

```rust
#[test]
fn v2_frame_builder_telemetry_increments_on_open_and_close() {
    let mut backend = KmsBackendV2::for_tests_with_vk().expect("vk");
    backend.set_frame_builder_enabled_for_tests(true);
    let dst = backend.allocate_test_pixmap_bgra(128, 128);
    let pre = backend.telemetry_lifetime_snapshot_for_tests();
    backend.composite_glyphs_for_tests(dst, [1.0, 1.0, 1.0, 1.0], &backend.synth_4_glyphs(), None).expect("ok");
    backend.tick_maybe_composite_for_tests();
    let post = backend.telemetry_lifetime_snapshot_for_tests();
    assert_eq!(post.frame_builder_opens - pre.frame_builder_opens, 1);
    assert_eq!(post.frame_builder_closes - pre.frame_builder_closes, 1);
    assert!(post.frame_builder_close_reason_legacy_sc_compose > pre.frame_builder_close_reason_legacy_sc_compose);
}
```

- [ ] **Step 5: Run + commit**

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_telemetry_increments_on_open_and_close -- --nocapture
git add crates/yserver/src/kms/v2/telemetry.rs crates/yserver/src/kms/v2/engine.rs crates/yserver/src/kms/v2/backend.rs crates/yserver/src/kms/v2/frame_builder.rs crates/yserver/tests/v2_acceptance.rs
git commit -m "feat(v2/telemetry): frame_builder_* counters + open/close/abort event wiring (Phase B.1 Task 21)"
```

---

### Task 22: Renderer-failed integration test — submit failure rolls back overlays

**Files:**
- Modify: `crates/yserver/tests/v2_acceptance.rs`

Inject a forced submit failure (using the existing `force_next_submit_failure_for_integration_tests` from Phase A T10) at the moment the frame builder calls `end_and_submit_op`. Assert:
1. `renderer_failed` is set.
2. The dst drawable's `last_render_ticket` is restored to its pre-frame value (None if it had none).
3. The atlas's `last_render_ticket` is unchanged.
4. `Drawable::storage.current_layout` is restored to pre-frame value.
5. The atlas cache does NOT contain the glyph keys we would have inserted.

- [ ] **Step 1: Write the test**

```rust
#[test]
fn v2_frame_builder_renderer_failed_on_submit_failure() {
    let mut backend = KmsBackendV2::for_tests_with_vk().expect("vk");
    backend.set_frame_builder_enabled_for_tests(true);
    let dst = backend.allocate_test_pixmap_bgra(64, 64);

    // Snapshot pre-frame state.
    let pre_layout = backend.drawable_current_layout_for_tests(dst);
    let pre_last_ticket = backend.drawable_last_render_ticket_for_tests(dst);
    assert!(pre_last_ticket.is_none(), "fresh pixmap has no prior ticket");

    // Fabricate one fresh glyph key + arm the failure latch BEFORE
    // the composite_glyphs call. The latch fires on the NEXT
    // vkQueueSubmit2 — which is the frame's submit.
    let glyphs = backend.synth_n_unique_glyphs(1);
    let key_about_to_insert = glyphs[0].atlas_key();
    backend.force_next_submit_failure_for_integration_tests();

    let result = backend.composite_glyphs_for_tests(dst, [1.0, 1.0, 1.0, 1.0], &glyphs, None);
    // composite_glyphs doesn't close on its own; the failure surfaces
    // at the close trigger.
    assert!(result.is_ok(), "composite_glyphs doesn't fail synchronously");
    let close_result = backend.tick_maybe_composite_for_tests_returning_result();
    // The IO error is logged but maybe_composite returns Ok(()) — but
    // the engine's renderer_failed should be set.
    let _ = close_result;
    assert!(backend.renderer_failed_for_tests(), "renderer_failed set after forced failure");

    // Verify rollback.
    assert_eq!(
        backend.drawable_current_layout_for_tests(dst),
        pre_layout,
        "drawable layout restored to pre-frame value"
    );
    let post_last_ticket = backend.drawable_last_render_ticket_for_tests(dst);
    assert!(
        post_last_ticket.is_none(),
        "drawable last_render_ticket restored to None after failure",
    );
    assert!(
        backend.glyph_atlas_lookup_for_tests(key_about_to_insert).is_none(),
        "pending glyph insert dropped on failure",
    );
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_renderer_failed_on_submit_failure -- --nocapture
git add crates/yserver/tests/v2_acceptance.rs crates/yserver/src/kms/v2/backend.rs
git commit -m "test(v2/frame_builder): renderer_failed rollback covers layout + ticket + atlas insert (Phase B.1 Task 22)"
```

---

### Task 23: Mixed-sequence smoke test — paint → composite_glyphs → paint

**Files:**
- Modify: `crates/yserver/tests/v2_acceptance.rs`

Spec § "Acceptance tests" — `v2_frame_builder_mixed_sequence_smoke`: realistic ordering produces exactly the expected sequence of submits.

- [ ] **Step 1: Write the test**

```rust
#[test]
fn v2_frame_builder_mixed_sequence_smoke() {
    let mut backend = KmsBackendV2::for_tests_with_vk().expect("vk");
    backend.set_frame_builder_enabled_for_tests(true);
    let dst = backend.allocate_test_pixmap_bgra(256, 256);
    let pre_submit_count = backend.platform_queue_submit2_count_for_tests();

    // Step 1: a fill_rect (non-ported) → goes through SubmitGroup
    // cap=1 → 1 submit. No frame opens.
    backend.fill_rect_for_tests(dst, 0, 0, 64, 64, 0xFFFF0000).expect("ok");
    let after_fill = backend.platform_queue_submit2_count_for_tests();
    assert_eq!(after_fill - pre_submit_count, 1, "fill_rect submits via SubmitGroup");
    assert!(!backend.frame_builder_is_open_for_tests());

    // Step 2: composite_glyphs (ported) → opens the frame.
    backend.composite_glyphs_for_tests(dst, [1.0, 1.0, 1.0, 1.0], &backend.synth_n_unique_glyphs(4), None).expect("ok");
    assert!(backend.frame_builder_is_open_for_tests());
    // No submit yet (frame stays open).
    assert_eq!(backend.platform_queue_submit2_count_for_tests(), after_fill);

    // Step 3: another fill_rect → M2 closes the frame, then submits
    // its own CB. Two submits total.
    backend.fill_rect_for_tests(dst, 64, 0, 64, 64, 0xFF00FF00).expect("ok");
    let after_mixed = backend.platform_queue_submit2_count_for_tests();
    assert_eq!(after_mixed - after_fill, 2, "M2 close + fill_rect submit");
    assert!(!backend.frame_builder_is_open_for_tests());
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_mixed_sequence_smoke -- --nocapture
git add crates/yserver/tests/v2_acceptance.rs
git commit -m "test(v2/frame_builder): mixed-sequence smoke (paint → glyphs → paint with M2 close) — Phase B.1 Task 23"
```

---

### Task 24: Flip `YSERVER_FRAME_BUILDER` default to ON — the bee fix lands here

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs` (the `frame_builder_enabled` initializer added in Task 15)

This is the single commit that activates the bee fix. Until this commit, every test in Tasks 15–23 ran with `set_frame_builder_enabled_for_tests(true)` flipping the gate locally; production stayed on the legacy `composite_glyphs` path. After this commit, production composite_glyphs routes through the FrameBuilder by default, and bee MATE survives.

- [ ] **Step 1: Flip the default**

```rust
// engine.rs — RenderEngine::new initializer
// (Task 15 left this as default-off — opt-in.)
frame_builder_enabled: std::env::var_os("YSERVER_FRAME_BUILDER")
    .as_deref()
    .and_then(|s| s.to_str())
    .map_or(true, |s| !matches!(s, "0" | "off" | "false" | "no")),
```

(Default-on with explicit-off escape hatch.)

- [ ] **Step 2: Re-run the full suite**

```bash
cargo test -p yserver
```

All green. Previously-locally-overridden tests now run on the default-on path.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver/src/kms/v2/engine.rs
git commit -m "feat(v2/engine): default YSERVER_FRAME_BUILDER=on — bee MATE-load fix activates (Phase B.1 Task 24)"
```

The commit message names the bee fix explicitly so `git bisect bad ...` lands here for any bee-related regression.

---

### Task 25: cargo +nightly fmt + clippy pedantic + full suite green

**Files:** all touched

- [ ] **Step 1: Format**

The project's `rustfmt.toml` enables `unstable_features = true` + `imports_granularity = "Crate"`, both of which require nightly rustfmt. Use:

```bash
cargo +nightly fmt
```

- [ ] **Step 2: Clippy pedantic**

```bash
cargo clippy --workspace --all-targets -- -W clippy::pedantic
```

Fix every warning surfaced by the new B.1 code. Acceptable suppressions:
- `#[allow(clippy::too_many_arguments)]` on `emit_recorded_op_into_cb` if the arg count crosses 7. Inherits the project pattern.
- `#[allow(clippy::cast_possible_wrap)]` / `#[allow(clippy::cast_sign_loss)]` for the integer casts copied verbatim from `composite_glyphs_legacy`.

- [ ] **Step 3: Full test suite**

```bash
cargo test --workspace
```

All green.

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "chore(v2/frame_builder): cargo fmt + clippy pedantic clean — Phase B.1 Task 25"
```

---

### Task 26: Status doc update + bee hardware-smoke gate placeholder

**Files:**
- Modify: `docs/status.md` — append Phase B.1 entry

- [ ] **Step 1: Add the status entry**

```markdown
## 2026-05-2X Phase B.1 — IN PROGRESS

Phase B sub-phase B.1 lands on `feature/frame-builder-submit-rate` (or
a successor branch). Tasks 1–24 of
`docs/superpowers/plans/2026-05-24-frame-builder-phase-b1.md` complete;
B.1 acceptance gate is the bee MATE-load smoke (below) plus
green-on-the-other-three regression check.

**Acceptance gates:**

1. **bee MATE-load survival** — boot MATE, drag for 30 s with
   `YSERVER_FRAME_BUILDER=on` (default), expect zero
   `ERROR_DEVICE_LOST` and zero RADV GPUVM faults. Same telemetry +
   submit-trace harness as the 2026-05-23 freeze capture, comparing
   the `composite_glyphs` submit-rate to the pre-B.1 baseline.
   Expected reduction: `composite_glyphs` collapses from N submits per
   call to 1; total `queue_submit2/s` on bee MATE drag drops well
   below the pre-B.1 peak (target 200–400/s by end of B.4 per the
   spec; B.1 alone hits an intermediate band because non-glyph paint
   ops still submit per-op under cap=1). The load-bearing assertion
   is "no fault", not the rate.

2. **yoga / iMac / fuji regression check** — same MATE drag, expect
   no new `ERROR_DEVICE_LOST` events and `queue_submit2/s` ≤ Phase A
   peak. B.1 will TEMPORARILY raise these platforms' submit rate
   (cap=1 reverts every non-glyph paint op to pre-Phase-A cadence);
   that's acceptable as long as no platform regresses below
   functional correctness. Recovery is at sub-phases B.2–B.3 when
   more paint ops join the frame builder.

3. **silence dual-output regression check** — confirm the frame
   builder's empty multi-output path (B.1 doesn't fold compose, so
   single-output is the only flavor exercised) still presents both
   outputs correctly via the existing per-output compose path. M3
   close fires before compose; compose runs unchanged.

**Open follow-ups:**

- Spec § "Phase B open questions for the implementation plan":
  - Q1 (op variant sizing) — measured at B.1 close; if `RecordedOp`
    grew past 256 B, B.2 must Box the offender.
  - Q3 (feature gate retirement timing) — B.1 ships with the gate
    flipped on at Task 24 (a separate, single-line commit for clean
    bisect); final removal of the env var deferred to B.5 alongside
    SubmitGroup deletion.
  - Q5 (semaphore export ordering) — Task 16's
    `v2_frame_builder_closes_on_present_completion_and_exports_sync_fd`
    integration test verifies the submit-then-export round-trip
    against the COW PRESENT-completion path.
  - Q6 (telemetry overlap window) — B.1 ↔ B.4 shows both
    `submit_group_*` AND `frame_builder_*` counters; the sum
    `paint_submits = submit_group_paint_submits + frame_builder_paint_submits`
    is the dashboard total during the rollout.
```

- [ ] **Step 2: Commit**

```bash
git add docs/status.md docs/superpowers/plans/2026-05-24-frame-builder-phase-b1.md
git commit -m "docs(status): Phase B.1 implementation plan + acceptance gate documented (Task 26)"
```

---

## Self-review checklist (run after Task 26; before opening the PR)

- [ ] Spec § "Phase B architecture" → "Frame lifecycle": Closed / OpenForPaint covered (B.1); ClosingWithCompose deferred to B.4. ✓
- [ ] Spec § "Op representation": `RecordedOp` enum has B.1-relevant variants only (CompositeGlyphs, GlyphUpload, LayoutTransition); 256 B size test pinned. ✓
- [ ] Spec § "Drawable lifetime": `touched_drawables` overlay first-touch + restore-on-failure (rollback_pre_submit); atlas `last_render_ticket` field with shutdown gate. ✓
- [ ] Spec § "Glyph upload — speculative atlas overlay": pack stays monotonic; `pending_glyph_inserts` collected during walk + committed only post-`flush_submit_group`-Ok via `commit_close_success`; upload + draw share one CB. ✓
- [ ] Spec § "Damage accumulation": damage mutated at APPEND time inside `composite_glyphs_via_frame_builder` (Task 15 step 3, section "(8) Append-time damage mutation"); NOT rolled back on failure. ✓
- [ ] Spec § "Transactional layout state": `FrameLayoutTable` first-touches snapshot pre_frame at Task 15 steps (4)+(5); commit-on-success is a no-op (recorder mutated storage directly); rollback on close-failure via `rollback_pre_submit` + `rollback_atlas` — best-effort path the spec allows (Pitfall 2). ✓
- [ ] Spec § "Multi-output topology": out of scope for B.1 (no compose folding). ✓
- [ ] Spec § "Per-output partial-failure handling": deferred to B.4 (compose-in-frame is B.4). ✓
- [ ] Spec § "Frame close triggers": 8 reasons covered. `LegacyScCompose` (Task 13) is B.1's compose-close reason; `SceneCompose` variant defined but goes unused until B.4. Other triggers: M2 NonPortedPaintOp (Task 14), PresentCompletionSignal at the REAL semaphore-submit site (Task 16), SyncWait (Task 17), Timeout (Task 18), Shutdown (Task 19), PinCeiling (Task 20). ✓
- [ ] Spec § "Frame-wide resource pinning": Mechanism 1 (Arc StagingBuffer) ✓; Mechanism 2 (descriptor watermark) deferred to B.2; Mechanism 3 (Arc-wrap scratch) deferred to B.2. ✓
- [ ] Spec § "CB recording at close": 3-pass walk codified in Task 12 with the load-bearing commit-after-`flush_submit_group`-Ok ordering (Pitfall 1). ✓
- [ ] Spec § "Migration boundaries": M1 (Task 10 — type-level default 1 in `SubmitGroup::new()` + env override disabled), M2 (Task 14 — 10 entry points), M3 (Task 13). ✓
- [ ] Spec § "Error handling and rollback": every error path drops the local `OpenFrame` (pins, overlays, pending atlas inserts evaporate); pre-`end_and_submit_op` errors free the CB manually; post-`flush_submit_group` errors rely on `abort_flush` already freeing CBs + setting `renderer_failed` — no double-free. Layout/ticket rollback via `rollback_pre_submit` + `rollback_atlas`. Damage NOT rolled back. ✓
- [ ] Spec § "Telemetry": all counters in Task 21 + new `FlushReason::FrameBuilder` arm in Task 10. ✓
- [ ] Spec § "Acceptance tests" integration: `v2_frame_builder_composite_glyphs_one_submit` (Task 15), Q5 SYNC_FD pass-through (Task 16), `v2_frame_builder_renderer_failed_on_submit_failure` (Task 22), `v2_frame_builder_mixed_sequence_smoke` (Task 23). All tests use `set_frame_builder_enabled_for_tests(true)` to flip the gate locally because the production default doesn't flip on until Task 24. ✓
- [ ] Spec § "Acceptance tests" hardware: bee MATE survival is the headline criterion for Task 24's commit; Task 26 documents the gate; user runs it. ✓
- [ ] Spec § "Open questions": Q1 (variant sizing) tested in Task 2; Q3 (feature gate default-flip timing) handled by deferring the flip to Task 24 — separate commit, clean bisect; Q5 (semaphore export ordering) tested in Task 16; Q6 (telemetry overlap window) wired in Task 21. Q2, Q4, Q7 deferred to B.4. ✓
- [ ] Codex round-1 findings 1-8: (1) commit-after-submit-Ok ordering → Pitfall 1 + Task 12 rewrite; (2) M2/composite_glyphs ordering vs batches → Task 15 step (0) flush; (3) layout rollback wiring → Task 15 steps (4)+(5) first_touch calls; (4) gate flip after triggers → Task 24 separate flip; (5) Task 16 wired at real site → Task 16 rewrite (`enqueue_present_completion` at backend.rs:9413); (6) borrow checker → Pitfall 3; (7) SubmitGroup default → Task 10 type-level default 1 + env override disabled; (8) drain_all churn → Task 19 keeps `drain_all` signature, adds `shutdown` + extends `Drop`. ✓
- [ ] Codex round-2 findings: (R2.1 atlas rollback not invoked) → every Task 12 error path now also calls `rollback_atlas`; (R2.2 pin ceiling not enforced per-call) → Task 15 step (6a) pre-pass counts misses before allocating, drops excess; (R2.3 M2 brittle) → Task 14's helper conditional on frame-open + flushes batches BEFORE closing; (R2.4 telemetry drain coverage) → Task 21 adds `drain_frame_builder_telemetry` helper called from every close-driving site; (R2.5 gate default contradictions) → Task 15 commit message + open-questions text aligned to "Task 24 flips ON"; (R2.6 cargo +nightly fmt nit) → Task 25 uses nightly. ✓
- [ ] Codex round-3 findings: (R3.1 M2 helper killed batch coalescing) → Task 14 helper is now no-op when no frame open; existing batch preludes stay; (R3.2 pin-ceiling accounting holes) → pre-pass dedupes via HashSet against committed atlas + pending inserts; per-glyph walk consults committed + pending + new_uploads (in that order); `pending_pins_before_call` recomputed after close+reopen; (R3.3 telemetry drain misses pin-ceiling close) → added `drain_frame_builder_telemetry()` call after `composite_glyphs` engine call in `KmsBackendV2::render_composite_glyphs` wrapper. ✓
- [ ] Codex round-4 findings: (R4.1 pin-ceiling drop after pack leaks atlas slots) → ceiling guard now fires BEFORE `pack()` so dropped glyphs don't consume shelf advance; (R4.2 nit: drop(inner) → use lexical scope) → replaced with tight `{ … }` scope + `let _ = inner;` signal. ✓

## Test-helper inventory

Several integration tests reference helper methods that don't exist today. Add each one as part of the task that first uses it; this section is the index so they don't fall through the cracks. All live on `KmsBackendV2` in `crates/yserver/src/kms/v2/backend.rs` under `#[cfg(test)]` or `pub(crate)` (the latter when the integration tests in `crates/yserver/tests/v2_acceptance.rs` need them).

| Helper | First task | Purpose |
|---|---|---|
| `set_frame_builder_enabled_for_tests(b: bool)` | 13 | Override the engine's `frame_builder_enabled` runtime gate. |
| `frame_builder_is_open_for_tests() -> bool` | 13 | `self.engine.frame_builder_is_open()`. |
| `frame_builder_lifetime_closes_for_tests() -> u64` | 13 | Mirror of `FrameBuilder::lifetime_closes()`. |
| `frame_builder_lifetime_opens_for_tests() -> u64` | 21 | Mirror of `FrameBuilder::lifetime_opens()`. |
| `tick_maybe_composite_for_tests()` | 13 | `self.maybe_composite().ok();` (ignore IO error). |
| `tick_maybe_composite_for_tests_returning_result() -> io::Result<()>` | 22 | Same, but returns the result for assertions. |
| `composite_glyphs_for_tests(dst, fg, glyphs, clip) -> Result<…>` | 13 | Calls `self.engine.composite_glyphs(...)` with stored store + platform. |
| `fill_rect_for_tests(dst, x, y, w, h, color) -> Result<…>` | 14 | Calls `self.engine.fill_rect(...)` similarly. |
| `allocate_test_pixmap_bgra(w, h) -> DrawableId` | 13 | Wraps the existing `RenderEngine::create_pixmap` test helper. |
| `synth_4_glyphs() -> Vec<CompositeGlyphInput<'static>>` | 13 | 4 unique fake-R8 glyphs at distinct `(gs_xid, glyph_id)` keys; sized 8×12 with 96 bytes of `vec![0xFF; 96]` pixels each. |
| `synth_32_glyphs_at_origin() -> Vec<CompositeGlyphInput<'static>>` | 15 | 32 unique glyphs, all positioned at dst origin. Used for the load-bearing one-submit test. |
| `synth_n_unique_glyphs(n: usize) -> Vec<CompositeGlyphInput<'static>>` | 20, 22 | Generic n-glyph factory. |
| `platform_queue_submit2_count_for_tests() -> u64` | 15 | Reads the existing `crate::vk_count` counter registry's `queue_submit2` total. |
| `platform_submit_group_max_size_for_tests() -> usize` | 10 | `self.platform.submit_group_max_size()`. |
| `engine_frame_seq_for_tests() -> u64` | 15 | Mirror of `RenderEngineInner::frame_seq`. |
| `drawable_current_layout_for_tests(id: DrawableId) -> vk::ImageLayout` | 22 | `self.store.get(id).map(\|d\| d.storage.current_layout).unwrap_or(UNDEFINED)`. |
| `drawable_last_render_ticket_for_tests(id: DrawableId) -> Option<FenceTicket>` | 22 | Mirror access. |
| `renderer_failed_for_tests() -> bool` | 22 | `self.platform.renderer_failed`. |
| `glyph_atlas_lookup_for_tests(key: GlyphKey) -> Option<AtlasEntry>` | 22 | Calls `V2GlyphAtlas::lookup`. |
| `enqueue_present_completion_for_tests(dst) -> PendingPresentBatch` | 16 | Drives the real `enqueue_present_completion` path and returns the last-registered batch. |
| `set_frame_builder_pin_ceiling_for_tests(n: usize)` | 20 | `self.engine.frame_builder.set_max_pinned_resources_per_frame(n)`. |
| `telemetry_lifetime_snapshot_for_tests() -> Bucket` | 21 | Clone of `self.telemetry.lifetime`. |
| `FenceTicket::for_tests_stub() -> Self` | 7 | Stub constructor in `platform.rs`: null fence + `signaled_cache = true` + empty weak pool. Used by no-Vk unit tests. |
| `attach_present_completion_for_tests` | (renamed in round 1) — *deleted; Task 16 uses `enqueue_present_completion_for_tests` instead*. | |

## Out of scope (intentional — these belong to future sub-phases)

- **Porting other paint ops** — only `composite_glyphs` is ported in B.1. The other 10 entry points stay on the legacy path with M2 wrapping. B.2 ports the next three (render_composite, render_fill, glyph_upload-standalone); B.3 the remainder.
- **DescriptorPoolRing `acquire_for_frame` watermark** — text pipeline uses a static descriptor set, so B.1 doesn't need Mechanism 2. B.2 adds it when render_composite ports.
- **Arc-wrapping singleton scratch (Mechanism 3)** — composite_glyphs doesn't use scratch. B.2 adds Mechanism 3 alongside render_composite.
- **Folding compose into the frame** — B.4. B.1's frame is paint-only; M3 keeps compose separate.
- **Multi-output rendering instances** — entailed by compose-in-frame; B.4.
- **Removing SubmitGroup** — B.5.
- **Bee RADV/Mesa bug characterisation** — out of yserver's repo, per the spec.
