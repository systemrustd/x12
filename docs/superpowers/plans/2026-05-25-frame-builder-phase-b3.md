# Frame-builder Phase B.3 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port the remaining 8 non-ported paint ops (`copy_area`, `cow_copy_area`, `put_image`, `fill_rect`, `fill_rect_batch`, `logic_fill`, `image_text`, `render_traps_or_tris`) into the FrameBuilder. Each call accumulates a `RecordedOp` into the open frame instead of recording and submitting its own CB. After B.3, all 8 sources contribute zero `non_ported` close events in steady state — the only surviving call site of `close_open_frame_for_non_ported_op` is the B.2 kill-switch fallback in `render_composite_legacy` (engine.rs:5877). On the bee MATE workload the per-frame absorption should push `submit_group_flushes/s` down by another 30–50 % beyond B.2's ~75 % absorption.

**Architecture:** Rip-and-replace — each op gets its REWRITTEN body in place, no `_legacy`/`_via_frame_builder` split, no `YSERVER_FRAME_BUILDER_B3_*` env gate, no flip commits. The new bodies reuse B.2 infrastructure verbatim (BatchResource three-tier retire, `OpenFrame::frame_generation` watermark, `FrameLayoutTable` overlay, atomic `push_op_and_set_layouts`, `RecordedOp::*` variants on `open_frame.ops`, `emit_recorded_*_into_cb` dispatch). One structural extension: `SubmittedOp::scratch: Option<ScratchImage>` becomes `Vec<ScratchImage>` to carry the on-`RecordedCopyArea` self-overlap scratch through close (N8). One protocol-load-bearing migration: `pending_cow_batch.present_completions` moves to `OpenFrame::pending_present_completions` so X PRESENT completions still fire when `cow_copy_area` is frame-builder-resident (N10).

**Tech Stack:** Rust, `ash` Vulkan bindings, existing v2 FrameBuilder (B.1 + B.2) + `DescriptorPoolRing` + `DstReadback` + `SolidColorImage` + `MaskScratch` + `RenderPipelineCache` + `LogicFillPipelineCache` + `TextPipeline` + `V2GlyphAtlas` + `TrapPipeline` + per-op vk recorders.

**Reference docs:**

- Phase B.3 spec: `docs/superpowers/specs/2026-05-25-frame-builder-phase-b3-design.md` (the spec went through 17 rounds of codex review; every N-invariant referenced here corresponds to a section in the spec).
- Phase B.2 plan: `docs/superpowers/plans/2026-05-24-frame-builder-phase-b2.md` (the template — re-use the task shape, the close-path patterns, the integration-test discipline).
- B.1 implementation plan: `docs/superpowers/plans/2026-05-24-frame-builder-phase-b1.md` (the original frame-builder shape; B.3's GLYPH family reuses `RecordedOp::GlyphUpload` from B.1 verbatim).
- AGENTS.md (project conventions): plain `cargo clippy` only, NOT pedantic; `cargo +nightly fmt`; `git push` in sandbox needs `GIT_SSH_COMMAND="ssh -F /dev/null"`.

**File structure (locked in before tasks):**

- **Modify**: `crates/yserver/src/kms/v2/frame_builder.rs`
  - Add 6 new `RecordedOp::*` variants (`CopyArea`, `PutImage`, `FillRect`, `LogicFill`, `ImageText`, `RenderTrapsOrTris`), each Box-wrapped, with `RecordedCopyArea` / `RecordedPutImage` / `RecordedFillRect` / `RecordedLogicFill` / `RecordedImageText` / `RecordedRenderTrapsOrTris` payload structs (full field listings per N5 + the variant catalog).
  - Add a `RecordedOp::dst_id(&self) -> Option<DrawableId>` helper (per N10 — covers every variant in the enum).
  - Add `OpenFrame::pending_present_completions: Vec<PendingPresentEntry>` slot (per N10).
  - Extend the size-budget test to assert each new payload ≤ 512B individually and `RecordedOp` itself stays ≤ 256B.
- **Modify**: `crates/yserver/src/kms/v2/engine.rs`
  - **Rename `SubmittedOp::scratch: Option<ScratchImage>` → `SubmittedOp::scratch: Vec<ScratchImage>`** (engine.rs:225-229). Every `SubmittedOp { ..., scratch: None, ... }` literal becomes `scratch: Vec::new()`; the existing self-overlap path at engine.rs:2937-2945 transiently becomes `scratch: vec![scratch]` (this site itself is rewritten in Task 2 to use the frame builder; the Task 1 patch is a transient mechanical conversion to keep HEAD green).
  - Extend `close_open_frame` (engine.rs:1459 region) with a **scratch walk** over `open_frame.ops` BEFORE the `SubmittedOp` literal — `std::mem::take` each `RecordedCopyArea::self_overlap_scratch` into a local `Vec<ScratchImage>`, then pass it to `scratch:`. The walk runs BEFORE `flush_submit_group` so close-failure drops the local vec on the stack (scratches die cleanly via `ScratchImage::Drop`).
  - Rewrite `copy_area` (engine.rs:2735), `cow_copy_area` (engine.rs:3079), `put_image` (engine.rs:4127), `fill_rect` (engine.rs:2244) + `fill_rect_batch` (engine.rs:2274), `logic_fill` (engine.rs:2489), `image_text` (engine.rs:4486), `render_traps_or_tris` (engine.rs:7183) — in-place, no `_legacy` suffix. Each body's `close_open_frame_for_non_ported_op` call is DELETED; `flush_cow_batch` calls are DELETED (the helper itself is deleted in Task 4); `flush_render_batch` calls stay per N9.
  - For `cow_copy_area` (Task 4): delete `pending_cow_batch` field on `RenderEngineInner` (engine.rs:622), delete `flush_cow_batch` helper (engine.rs:3412), delete `cow_copy_area_open_first` (engine.rs:3289), delete `drain_cow_flush_records` (engine.rs:3614), delete `cow_flush_records` field (engine.rs:630), rewrite `attach_cow_present_completion` (engine.rs:3581) to push into the open frame's slot per N10's predicate (`RecordedOp::dst_id() == Some(cow_id)`), rewrite `close_open_frame` to acquire `PresentCompletionSignal` BEFORE `end_and_submit_op_with_signal` and force-enqueue the degraded `PendingPresentBatch` on flush-failure. **Delete the legacy cow-batch unit tests** (engine.rs:9974 `cow_copy_area_coalesces_four_srcs_into_one_submit`, engine.rs:10171 `cow_copy_area_repeated_src_skips_redundant_transition`, engine.rs:10312 `cow_copy_area_flush_via_non_cow_op`, engine.rs:10948 `cow_copy_area_open_marks_src_last_render_ticket_immediately`, plus the two SubmitGroup-cap tests at engine.rs:~13725 and engine.rs:~14130 that explicitly orchestrate `flush_cow_batch` calls) — their assertions read `inner.pending_cow_batch` or call `flush_cow_batch` / `drain_cow_flush_records` directly. Replace with Task 4's frame-builder collapse tests (see Step 1 below). The externally-observable behaviors these tests covered (same-dst coalesce, repeated-src layout-transition skip, mid-batch flush, src-fence-touched-at-batch-open, SubmitGroup deferred-graduation) are now covered structurally by the frame builder's per-frame collapse + the existing FrameBuilder lifecycle tests; the value of preserving them is zero once the API they exercise is gone.
  - Update `has_pending_batches_for_tests` (engine.rs:1718) to check `frame_builder.open.is_some() || pending_render_batch.is_some()`.
  - Add 6 new `emit_recorded_<op>_into_cb` helpers + 6 new match arms in `emit_recorded_op_into_cb` (engine.rs:8425). The 6 new variants (CopyArea, PutImage, FillRect, LogicFill, ImageText, RenderTrapsOrTris) map 1:1 to 6 new helpers + 6 new arms. `image_text`'s per-glyph uploads reuse B.1's existing `Op::GlyphUpload` arm verbatim per N7 — no new arm for the upload side.
- **Modify**: `crates/yserver/src/kms/v2/backend.rs`
  - At backend.rs:9904 / backend.rs:9918-9920 — drop the `flush_cow_batch` fallback in `enqueue_present_completion`'s flush-on-non-attach branch (it becomes unreachable when `pending_cow_batch` is deleted; the `attach_cow_present_completion` failure path stays — backend still has to enqueue the completion immediately when no open frame is writing to `cow_id`, but the "force-flush a stale batch" line is gone).
  - The 6 other `flush_cow_batch` call sites (backend.rs:2185-2193, 2523-2525, 4991-4993, 5066-5068, 6129-6131, 9918-9920) are all deleted: there is no batch to flush after Task 4.
  - **Delete `drain_cow_telemetry`** (backend.rs:1543-1573 — the helper that calls `engine.drain_cow_flush_records()` + `telemetry.record_cow_batch_flushed`) and its **two call sites** (backend.rs:5155 in `maybe_composite` tick, backend.rs:6140 in `release_overlay_window`). The doc-comment reference at backend.rs:6953 ("`drain_cow_telemetry`") is also stale and gets dropped with the rewritten surrounding code.
- **Modify**: `crates/yserver/src/kms/v2/telemetry.rs`
  - **Delete the cow-flush telemetry surface**: `record_cow_batch_flushed` helper (telemetry.rs:571-591), the `cow_batches_flushed: u64` bucket field (telemetry.rs:82), the `cow_copies_coalesced: u64` bucket field (telemetry.rs:89), and the `cow_batches_flushed/s` + `cow_copies_coalesced/s` slots in the per-second log format string (telemetry.rs:307 + 347-348). The stale doc-comment reference at telemetry.rs:848 ("Mirrors `record_cow_batch_flushed`") is updated to point at the new B.3 reality. `frame_builder_close_reason_non_ported_paint_op` stays (still counts the kill-switch path).
- **Modify**: `crates/yserver/tests/v2_acceptance.rs`
  - 8 new integration tests (one collapse-test per ported op + the cross-frame mask-scratch-grow test for MASK + the rollback / non-BGRA8 / PRESENT-completion tests for GLYPH).
  - Update narrative comments at v2_acceptance.rs:3440 (`Step 2: fill_rectangle on non-cow dst → flush_cow_batch fires`) and v2_acceptance.rs:3502 (`flush_cow_batch (no-op)`) to reflect the new flow — the surrounding tests themselves orchestrate mixed cow+fill+composite sequences and their externally-observable assertions (final submit count, draw counts) remain valid; only the narrative drift gets corrected. If the test still passes against the rewritten flow it stays; if its submit-count assertion is now wrong because the frame builder collapses what used to be N separate submits into one, update the asserted count.
- **Modify**: `docs/status.md` — Phase B.3 status entry + bee hardware-smoke gate placeholder for the post-B.3 capture.

**Rollout style.** Rip-and-replace; HEAD must compile + test-green at every task boundary. Order:

- **Task 1** is foundation: every new `RecordedOp` variant exists with `unimplemented!()` emit arms; `SubmittedOp::scratch` is renamed; the close-path scratch walk is in place yielding empty vecs. No ported body uses the new code yet; existing tests stay green.
- **Tasks 2–17** rewrite the 8 op bodies in place (paired with integration tests). Each body's existing `close_open_frame_for_non_ported_op` call is deleted; the new body appends to the open frame and the emit arm in Task 1's dispatch fills out at the same time. Each task gives a self-contained green build + tests.
- **Task 4 is atomic**: cow_copy_area body rewrite + delete `pending_cow_batch` + delete `flush_cow_batch` + migrate PRESENT-completion attach to `OpenFrame::pending_present_completions`. Partial deletion leaves dangling references that don't compile, so the whole change ships in one commit.
- **Task 18** plain cargo fmt + clippy (NOT pedantic per AGENTS.md).
- **Task 19** status doc update + hardware-gate placeholder.

---

## Invariant inventory (load-bearing — every task that adds code must respect)

These are net-new to B.3. All B.2 invariants (M1, M3, ticket-touch, renderer_failed fatal-after-failure, descriptor watermark, layout overlay, atomic push) apply verbatim.

- **N1 — TRANSFER ops normalize to SHADER_READ_ONLY_OPTIMAL at end + mirror legacy barrier shapes exactly.** Every TRANSFER op's emit MUST emit a final barrier transitioning dst (and src when it was transitioned) to `SHADER_READ_ONLY_OPTIMAL` before returning. The append's overlay update sets BOTH dst and (if applicable) src to `SHADER_READ_ONLY_OPTIMAL` via `push_op_and_set_layouts(op, &[(dst_id, SHADER_READ_ONLY_OPTIMAL), (src_id, SHADER_READ_ONLY_OPTIMAL)])`. The exact stage/access masks MUST mirror the legacy paths' shapes (engine.rs:2951 for copy_area pre/post, engine.rs:3300 for cow_copy_area, engine.rs:4210 for put_image — see spec § N1 for the verbatim mask lists). Generic "TRANSFER_WRITE only" producer masks would recreate the B.2-class RAW hazard.
- **N2 — `put_image` staging buffer uses the B.1 `FramePinSet::staging_buffers` slot.** `put_image` pins the staging Arc via `open.pins.pin_staging(Arc::clone(&staging))` at append; emit reads the buffer via `pins.staging_buffers[idx.0 as usize].buffer`. Identical pattern to B.1's `RecordedGlyphUpload`.
- **N3 — `cow_copy_area`'s dst is a regular Drawable; no special cow-layout machinery.** The cow is registered in the v2 store like any other drawable; `cow_copy_area` records a `RecordedCopyArea` with `dst_id: DrawableId` pointing at the cow's entry. No `OpenFrame::cow_layout_in_frame` slot, no per-cow predicate; the existing `current_layout_for_drawable` accessor handles it.
- **N4 — `fill_rect_batch` records ONE `RecordedFillRect` per call** carrying the entire rect slice. No `RecordedFillRectBatch` variant. `fill_rect` records ONE `RecordedFillRect` with `N=1` rects.
- **N5 — `render_traps_or_tris` records ONE recorded op covering both raster + composite stages.** Pipeline / descriptor / mask_scratch identity / dst_readback view are NOT recorded — emit re-resolves all four fresh from engine state. The full field listing is in the spec § "N5 RecordedRenderTrapsOrTris payload" (~22 fields including `op_byte: u8` separate from `std_op: StdPictOp` per the `needs_full_dst` byte-pattern test, `RecordedTrapSrcKind`, `clip_scissors: Vec<vk::Rect2D>` pre-clamped at append, `vertex_pool_pin: Arc<StagingBuffer>` for trap/tri vertex buffer). Cross-frame mask-scratch-grow integration test is REQUIRED. Post-emit CPU-layout writeback (`engine.mask_scratch.set_current_layout(SHADER_READ_ONLY_OPTIMAL)`) is REQUIRED after the composite-close barrier — mirror engine.rs:7745-7749.
- **N6 — `logic_fill` uses its own pipeline cache (separate sub-shape inside FILL).** `LogicFillPipelineCache::get(logic_mode, opaque_alpha)` per engine.rs:2530-2538. NO descriptor set traffic. The recorded payload carries `dst_id` + `logic_mode` + `dst_format` + `opaque_alpha: bool` (caller-provided GC state, NOT derived — part of the cache key) + color + pre-clamped rect slice + dst extent + `dst_old_layout`. Emit records a logic-op render pass with push constants directly.
- **N7 — `image_text` is in the GLYPH family, NOT MASK.** Reuses B.1's `RecordedOp::GlyphUpload` variant verbatim for the per-glyph upload side. Distinct `RecordedOp::ImageText` for the draw side, mirroring B.1's `RecordedCompositeGlyphs`. **Target-format gate (load-bearing):** body MUST early-return when `dst_format != B8G8R8A8_UNORM` (mirror engine.rs:4519-4527); the gate fires BEFORE any atlas first-touch / glyph upload / op append — no rollback needed because nothing was recorded. **Atlas transactional discipline:** `image_text`'s body MUST call the same first-touch-atlas snapshot helper that `composite_glyphs_via_frame_builder` does (engine.rs:5318-5333 — snapshot `atlas_prev_ticket_snapshot` + `layouts.first_touch_atlas`). The existing close-success `commit_close_success` + close-failure `rollback_atlas` paths handle the rest.
- **N8 — Live concrete-scratch ownership model (single source of truth on `RecordedCopyArea`).** `copy_area`'s self-overlap scratch (engine.rs:2814-2918) lives in EXACTLY ONE place. The chain: allocator local → `RecordedCopyArea::self_overlap_scratch: Option<ScratchImage>` (from append until close) → `Vec<ScratchImage>` local during close walk → `SubmittedOp::scratch: Vec<ScratchImage>` (from close success until fence retire) → `ScratchImage::Drop` at retire. There is NO sibling `OpenFrame::live_scratches` collector. **Allocation-first ordering:** the rewritten `copy_area` body MUST allocate the scratch BEFORE any open-frame state mutation, so an allocation failure leaves the frame untouched.
- **N9 — Every B.3 body MUST flush `pending_render_batch` at entry; `pending_cow_batch` is deleted.** Empty-input fast-path → `renderer_failed` check → `self.flush_render_batch(store, platform)?` → preflight / allocation / mutation (per N8) → `push_op_and_set_layouts`. The flush can itself close an open frame, so it precedes any open-frame mutation. No `flush_cow_batch` call — that helper is deleted in Task 4.
- **N10 — COW PRESENT-completion attach migrates from `pending_cow_batch.present_completions` to `OpenFrame::pending_present_completions`.** Predicate at attach is `open.ops.iter().any(|op| op.dst_id() == Some(cow_id))` (writes, NOT just touched). At close: acquire `PresentCompletionSignal` BEFORE `end_and_submit_op_with_signal`, thread the semaphore into the submit's signal list, drain the events into `PendingPresentBatch` post-`flush_submit_group` success, OR force-enqueue a degraded `PendingPresentBatch { wait: Ready, ticket: None, signal: None, events }` post-`flush_submit_group` failure (mirror engine.rs:3556-3567 — never silent-drop).

---

## Close-path correctness pattern (inherits from B.2)

The three pitfalls codified in the B.2 plan (commit-after-submit-Ok, layout overlay model, borrowck pattern) apply verbatim to every B.3 body. The new wrinkles B.3 introduces:

### Pitfall 7 — Single source of truth for the self-overlap scratch (N8)

**Wrong models (codex history caught these):**

- An `OpenFrame::live_scratches: Vec<ScratchImage>` sibling collector — codex round-8 catch. Would be a second copy of the ownership state, contradicting the on-`RecordedCopyArea` slot.
- A `Box<dyn BatchResource>` wrapper — the self-overlap scratch is LIVE during emit, not a retired-on-grow backing. The BatchResource three-tier routing (N1's case in the spec) is for grown-away mask/dst_readback backings, NOT for one-shot live scratches.

**Mandated model.** The scratch lives in `RecordedCopyArea::self_overlap_scratch: Option<ScratchImage>` from append until close. Close walks `open_frame.ops` BEFORE constructing the `SubmittedOp`, `std::mem::take`s every `self_overlap_scratch` into a local `Vec<ScratchImage>`, passes it to `scratch: Vec<ScratchImage>`. Close-failure drops the local on the stack — `ScratchImage::Drop` destroys the Vk handles cleanly (no fence ticket exists, GPU never saw the image, immediate destruction is safe per the existing Drop impl at engine.rs:307-314).

### Pitfall 8 — PRESENT-completion signal ordering (N10)

**Wrong models:**

- "Acquire the semaphore AFTER `flush_submit_group` succeeds" — the submit has already happened with NO semaphore in its signal list, so the exported sync_file fd will never fire. Caller blocks forever.
- "Acquire the semaphore but don't store it on the `PendingPresentBatch`" — `PresentCompletionSignal` owns the semaphore handle; the backend explicitly keeps it alive until both the sync primitive is ready AND the batch ticket signals (per backend.rs's existing post-flush ownership rule). Dropping the signal earlier is a use-after-free hazard.
- "Silent drop on flush failure" — X PRESENT protocol semantically delivers these events; silent drop would be a protocol-visible regression. Force-enqueue a degraded `PendingPresentBatch { wait: Ready, ticket: None, signal: None, events }` even when the submit failed (mirror engine.rs:3556-3567).

**Mandated order in `close_open_frame` (per Task 4):**

```
1. Acquire `completion_signal` if `open_frame.pending_present_completions` is non-empty.
2. Extract `completion_semaphore = signal.as_ref().map(|s| s.semaphore())`.
3. Call `end_and_submit_op_with_signal(inner, platform, cb, &frame_ticket, completion_semaphore)`.
   On Err: route through the existing append-failure rollback (engine.rs:1410-1446 — 8-step cleanup);
   the `completion_signal` drops with the local variable.
4. Push the SubmittedOp (frame's CB + frame_ticket + ...).
5. Drive `self.flush_submit_group(...)`.
   On Ok: drain `open_frame.pending_present_completions` into a new PendingPresentBatch with
          (wait, signal) derived from `signal.export_sync_file_fd()` (Fd / Ready / Poll).
   On Err: force-enqueue `PendingPresentBatch { wait: Ready, ticket: None, signal: None, events }`
           BEFORE returning Err. The `completion_signal` drops with the local — the failed submit
           never queued a signal-op.
```

### Pitfall 9 — Atomic cow_copy_area deletion

Splitting cow_copy_area's port across multiple commits leaves dangling references. The order inside Task 4 MUST be: add `OpenFrame::pending_present_completions` slot + `RecordedOp::dst_id()` helper + extend `close_open_frame` with the N10 three-branch plumbing in ONE local edit, then rewrite `cow_copy_area` body, then rewrite `attach_cow_present_completion`, then delete `pending_cow_batch` + `flush_cow_batch` + `cow_copy_area_open_first` + `cow_flush_records` + all `flush_cow_batch` call sites in backend.rs, then update `has_pending_batches_for_tests`. The commit only lands when `cargo build -p yserver` is clean across the whole sequence.

---

## Tasks

### Task 1: Foundation — variants, scratch slot rename, close-path scratch walk

Pre-flight for every B.3 body rewrite: the `RecordedOp` enum must grow 6 variants, `SubmittedOp::scratch` must accept multiple scratches via `Vec`, and the close path must walk the ops and migrate the live scratch state. The emit arms are stubbed with `unimplemented!()` and filled out by the per-op rewrites in Phases 2–5.

**Files:**

- Modify: `crates/yserver/src/kms/v2/frame_builder.rs` — add 6 payload structs + 6 `RecordedOp::*` variants + `RecordedOp::dst_id(&self) -> Option<DrawableId>` helper + `OpenFrame::pending_present_completions` slot + size-budget tests.
- Modify: `crates/yserver/src/kms/v2/engine.rs` — rename `SubmittedOp::scratch: Option<ScratchImage>` to `Vec<ScratchImage>`; extend close-path scratch walk; add 6 `unimplemented!()` arms to `emit_recorded_op_into_cb`.
- Test: same files (unit tests for size budget, dst_id() exhaustiveness, scratch walk).

- [ ] **Step 1: Add the `RecordedTrapSrcKind` discriminant**

Lives at the top of frame_builder.rs's "RecordedOp payloads" region (after `RecordedLayoutTransition`, before `enum RecordedOp`):

```rust
// crates/yserver/src/kms/v2/frame_builder.rs

use crate::kms::vk::ops::render::AffineXform;

/// Phase B.3 (N5): trap source resolved at append-time. Drawable holds the
/// id + the pict_format-aware swizzle class snapshot (see engine.rs:7360 —
/// the swizzle class is computed from `src_pict_format` + drawable depth at
/// append, pinned because RenderPictFormat resize could land between append
/// and emit); Solid carries the clear color snapshot (emit calls
/// `record_solid_color_clear` against `engine.solid_src_image`); Gradient
/// carries the picture xid (emit re-looks up `picture_paint[xid]` because
/// gradients are CPU-immutable per B.2 R3 finding 9) + the intrinsic axis
/// projection snapped here at append.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RecordedTrapSrcKind {
    Drawable {
        id: DrawableId,
        swizzle_class: crate::kms::vk::render_pipeline::SwizzleClass,
    },
    Solid([f32; 4]),
    Gradient {
        xid: u32,
        intrinsic_axis_projection: AffineXform,
    },
}
```

If `SwizzleClass` lives in a different module, adjust the path. The type is referenced from `swizzle_class_for_pict_format` (search `grep -nE 'SwizzleClass' crates/yserver/src/kms/vk/render_pipeline.rs`).

- [ ] **Step 2: Add the 6 payload structs**

All immediately follow `RecordedRenderComposite` (frame_builder.rs:479). Drop `#[derive(Debug)]` on each — the trait derives transitively from primitives + Arcs (all Debug). Field listings come from spec § "Op-variant catalog" + § "N5 RecordedRenderTrapsOrTris payload":

```rust
// crates/yserver/src/kms/v2/frame_builder.rs (after `RecordedRenderComposite`):

/// Phase B.3 (TRANSFER family — N1, N3, N8). Covers both `copy_area` and
/// `cow_copy_area` (the cow is just a regular DrawableId per N3). Emits two
/// barrier pairs + cmd_copy_image (disjoint case) or three pairs +
/// cmd_copy_image × 2 (self-overlap case — N8). The `self_overlap_scratch`
/// slot is the SINGLE source of truth for the per-op scratch lifetime —
/// see Pitfall 7 + N8 + Task 1's close-path scratch walk.
#[derive(Debug)]
pub(crate) struct RecordedCopyArea {
    pub(crate) dst_id: DrawableId,
    pub(crate) src_id: DrawableId,
    pub(crate) src_rect: vk::Rect2D,
    pub(crate) dst_rect: vk::Rect2D,
    pub(crate) src_format: vk::Format,
    pub(crate) src_extent: vk::Extent2D,
    pub(crate) dst_extent: vk::Extent2D,
    pub(crate) src_image: vk::Image,
    pub(crate) dst_image: vk::Image,
    pub(crate) src_old_layout: vk::ImageLayout,
    pub(crate) dst_old_layout: vk::ImageLayout,
    /// `Some(scratch)` when `src_id == dst_id` (self-overlap path,
    /// engine.rs:2814-2918 mirror). The scratch is owned by this
    /// payload from append until close; the close-path scratch walk
    /// `std::mem::take`s it into a `Vec<ScratchImage>` passed to
    /// `SubmittedOp::scratch`. NEVER an `OpenFrame::live_scratches`
    /// sibling — single source of truth per N8 + Pitfall 7.
    pub(crate) self_overlap_scratch: Option<super::engine::ScratchImage>,
}

/// Phase B.3 (TRANSFER family — N1, N2). Staging buffer is pinned via
/// `open.pins.pin_staging` at append; emit fetches the buffer handle from
/// `pins.staging_buffers[staging_pin_idx.0 as usize].buffer`.
#[derive(Debug)]
pub(crate) struct RecordedPutImage {
    pub(crate) dst_id: DrawableId,
    pub(crate) dst_rect: vk::Rect2D,
    pub(crate) dst_image: vk::Image,
    pub(crate) dst_extent: vk::Extent2D,
    pub(crate) dst_old_layout: vk::ImageLayout,
    pub(crate) staging_pin_idx: PinnedStagingIdx,
}

/// Phase B.3 (FILL family — N4). One variant covers both `fill_rect`
/// (N=1) and `fill_rect_batch` (N≥1). Emit uses `cmd_clear_attachments`
/// directly — NO composite pipeline, NO descriptor (codex round-7 catch
/// rewrote the spec). `load_op = LOAD` is LOAD-BEARING per the FILL
/// pseudocode in the spec — outside-rect pixels must be preserved.
#[derive(Debug)]
pub(crate) struct RecordedFillRect {
    pub(crate) dst_id: DrawableId,
    pub(crate) dst_image_view: vk::ImageView,
    pub(crate) dst_extent: vk::Extent2D,
    pub(crate) dst_format: vk::Format,
    pub(crate) dst_old_layout: vk::ImageLayout,
    pub(crate) color: [f32; 4],
    pub(crate) rects: Vec<vk::Rect2D>,  // pre-clamped at append, non-empty
}

/// Phase B.3 (FILL family — N6). Distinct from `RecordedFillRect`:
/// uses LogicFillPipelineCache + push constants + per-rect scissor draws.
/// `opaque_alpha` is a caller-provided GC parameter (cache key), NOT
/// derived from `dst_format` — codex round-8 catch.
#[derive(Debug)]
pub(crate) struct RecordedLogicFill {
    pub(crate) dst_id: DrawableId,
    pub(crate) dst_image_view: vk::ImageView,
    pub(crate) dst_extent: vk::Extent2D,
    pub(crate) dst_format: vk::Format,
    pub(crate) dst_old_layout: vk::ImageLayout,
    pub(crate) logic_mode: yserver_core::backend::GcFunction,
    pub(crate) opaque_alpha: bool,
    pub(crate) color: [f32; 4],
    pub(crate) rects: Vec<vk::Rect2D>,  // pre-clamped at append, non-empty
}

/// Phase B.3 (GLYPH family — N7). Companion to B.1's `RecordedGlyphUpload`
/// (reused verbatim per N7). Same shape as `RecordedCompositeGlyphs` but
/// emit calls into the text-pipeline path. `dst_format == B8G8R8A8_UNORM`
/// is implicit (append-side gate per N7).
#[derive(Debug)]
pub(crate) struct RecordedImageText {
    pub(crate) dst_id: DrawableId,
    pub(crate) dst_extent: vk::Extent2D,
    pub(crate) dst_old_layout: vk::ImageLayout,
    pub(crate) foreground_rgba: [f32; 4],
    pub(crate) glyphs: Vec<RecordedTextGlyph>,
}

/// Phase B.3 (MASK family — N5). Single variant covers both raster + composite
/// stages. Emit re-resolves `engine.mask_scratch`, `engine.dst_readback`,
/// composite pipeline, and descriptor set fresh; none of those four are
/// recorded.
#[derive(Debug)]
pub(crate) struct RecordedRenderTrapsOrTris {
    // Dst identity and layout (N1 / B.2).
    pub(crate) dst_id: DrawableId,
    pub(crate) dst_image: vk::Image,
    pub(crate) dst_view: vk::ImageView,
    pub(crate) dst_old_layout: vk::ImageLayout,
    pub(crate) dst_extent: vk::Extent2D,
    pub(crate) dst_format: vk::Format,
    pub(crate) dst_has_alpha: bool,
    // Composite operator + derived gates.
    pub(crate) std_op: crate::kms::vk::render_pipeline::StdPictOp,
    pub(crate) op_byte: u8,  // raw byte for the `needs_full_dst` pattern test (engine.rs:7472).
    // Source kind + per-kind snapshot data.
    pub(crate) src_kind: RecordedTrapSrcKind,
    // CompositeAttrs inputs.
    pub(crate) src_extent: vk::Extent2D,
    pub(crate) src_is_synthetic_1x1: bool,
    pub(crate) src_repeat: u32,           // pre-resolved shader constant.
    pub(crate) src_force_opaque: bool,
    pub(crate) user_src_xform: AffineXform,
    // Trap raster phase inputs.
    pub(crate) prim_kind: super::engine::TrapPrimKind,
    pub(crate) bbox_x: i32,
    pub(crate) bbox_y: i32,
    pub(crate) bbox_w: u32,
    pub(crate) bbox_h: u32,
    pub(crate) instance_count: u32,
    // Composite phase rect layout (clip pre-clamped at append).
    pub(crate) clip_scissors: Vec<vk::Rect2D>,
    // Pinned resources.
    pub(crate) vertex_pool_pin: PinnedStagingIdx,
}
```

Notes:

- `super::engine::ScratchImage` and `super::engine::TrapPrimKind` need `pub(crate)` if they aren't already. Check with `grep -nE '^(pub\(crate\) )?struct ScratchImage|^(pub\(crate\) )?enum TrapPrimKind' crates/yserver/src/kms/v2/engine.rs` — if either is private, change it to `pub(crate)` in this step.
- `yserver_core::backend::GcFunction` is the existing GC-function enum — `grep -n 'pub enum GcFunction' crates/yserver-core/src/backend.rs` confirms the path.
- The `vertex_pool_pin: PinnedStagingIdx` for `RecordedRenderTrapsOrTris` reuses B.1's `FramePinSet::staging_buffers` mechanism — the trap vertex `StagingBuffer` is wrapped in `Arc::new(StagingBuffer::new_with_usage(...))` at append, cloned into the pin set via `open.pins.pin_staging(...)`, and the returned `PinnedStagingIdx` is stored on the payload.

- [ ] **Step 3: Extend `RecordedOp` with the 6 new variants**

```rust
// crates/yserver/src/kms/v2/frame_builder.rs (replace the existing enum):
#[derive(Debug)]
pub(crate) enum RecordedOp {
    CompositeGlyphs(RecordedCompositeGlyphs),
    GlyphUpload(RecordedGlyphUpload),
    RenderComposite(Box<RecordedRenderComposite>),
    LayoutTransition(RecordedLayoutTransition),
    // Phase B.3 — all Box-wrapped per the size-budget rule.
    CopyArea(Box<RecordedCopyArea>),
    PutImage(Box<RecordedPutImage>),
    FillRect(Box<RecordedFillRect>),
    LogicFill(Box<RecordedLogicFill>),
    ImageText(Box<RecordedImageText>),
    RenderTrapsOrTris(Box<RecordedRenderTrapsOrTris>),
}
```

Remove the now-stale `dead_code` allow on `LayoutTransition` if its arm becomes load-bearing (B.3 doesn't add new uses, but Task 1's `dst_id()` helper exhaustively covers it — see Step 4).

- [ ] **Step 4: Add the `dst_id()` helper (N10)**

```rust
// crates/yserver/src/kms/v2/frame_builder.rs (in `impl RecordedOp` — add the
// block if not present):
impl RecordedOp {
    /// Phase B.3 (N10): the drawable this op WRITES to, or `None` for
    /// utility variants without a writable drawable destination.
    /// `attach_cow_present_completion`'s predicate uses this to decide
    /// whether to attach the completion to the open frame (per the spec's
    /// N10 — `touched` is the wrong predicate because it includes sampled-
    /// only references).
    pub(crate) fn dst_id(&self) -> Option<DrawableId> {
        match self {
            RecordedOp::CompositeGlyphs(g) => Some(g.dst_id),
            RecordedOp::RenderComposite(rc) => Some(rc.dst_id),
            RecordedOp::GlyphUpload(_) => None,        // writes to atlas, not a drawable
            RecordedOp::LayoutTransition(_) => None,   // utility variant
            RecordedOp::CopyArea(ca) => Some(ca.dst_id),
            RecordedOp::PutImage(pi) => Some(pi.dst_id),
            RecordedOp::FillRect(fr) => Some(fr.dst_id),
            RecordedOp::LogicFill(lf) => Some(lf.dst_id),
            RecordedOp::ImageText(it) => Some(it.dst_id),
            RecordedOp::RenderTrapsOrTris(rt) => Some(rt.dst_id),
        }
    }
}
```

Match ALL variants explicitly (no `_ =>` arm) so a future variant addition fails compilation rather than silently returning `None` and breaking N10's PRESENT-completion attach predicate.

- [ ] **Step 5: Add `OpenFrame::pending_present_completions` slot (N10)**

```rust
// crates/yserver/src/kms/v2/frame_builder.rs — in `struct OpenFrame`
// (around line 306-335; add after `glyph_uploads_in_frame`):
pub(crate) struct OpenFrame {
    pub(crate) ticket: FenceTicket,
    pub(crate) frame_generation: u64,
    pub(crate) ops: Vec<RecordedOp>,
    pub(crate) pins: FramePinSet,
    pub(crate) layouts: FrameLayoutTable,
    pub(crate) touched: TouchedDrawables,
    pub(crate) pending_glyph_inserts: PendingGlyphInserts,
    pub(crate) atlas_prev_ticket_snapshot: Option<Option<FenceTicket>>,
    pub(crate) glyph_uploads_in_frame: u32,
    pub(crate) close_reason_on_open: Option<CloseReason>,
    pub(crate) opened_at: Instant,
    /// Phase B.3 (N10): X PRESENT completions attached to this open frame
    /// via `attach_cow_present_completion`. Drained at close-success into
    /// `pending_present_batches` (alongside the acquired
    /// `PresentCompletionSignal`'s semaphore queued on the submit).
    /// Force-enqueued as a degraded `PendingPresentBatch { wait: Ready,
    /// ticket: None, signal: None, events }` on close-failure (mirror
    /// engine.rs:3556-3567's flush_cow_batch failure path — never silent
    /// drop, the X PRESENT protocol observes the events regardless of
    /// submit success).
    pub(crate) pending_present_completions: Vec<super::present_completion::PendingPresentEntry>,
}
```

Initialize the new field in `FrameBuilder::open_for_paint` (frame_builder.rs:186 — search `Box::new(OpenFrame {`):

```rust
self.open = Some(Box::new(OpenFrame {
    ticket,
    frame_generation,
    ops: Vec::new(),
    pins: FramePinSet::new(),
    layouts: FrameLayoutTable::new(),
    touched: TouchedDrawables::new(),
    pending_glyph_inserts: PendingGlyphInserts::new(),
    atlas_prev_ticket_snapshot: None,
    glyph_uploads_in_frame: 0,
    close_reason_on_open: None,
    opened_at: Instant::now(),
    pending_present_completions: Vec::new(),  // NEW (B.3 N10)
}));
```

Also initialize in the test helper `OpenFrame::new_for_tests` if present (search `grep -n 'OpenFrame {' crates/yserver/src/kms/v2/frame_builder.rs` — every constructor needs the field).

- [ ] **Step 6: Rename `SubmittedOp::scratch` and update existing call sites**

```bash
grep -nE 'scratch: (None|Some|Option<ScratchImage>)' crates/yserver/src/kms/v2/engine.rs
```

Expect about 16 sites: the field definition at engine.rs:229 + ~14 `SubmittedOp { ... }` literals + the self-overlap path at engine.rs:2937-2945.

Change the field:

```rust
// crates/yserver/src/kms/v2/engine.rs — `struct SubmittedOp` around line 218:
struct SubmittedOp {
    cb: vk::CommandBuffer,
    ticket: FenceTicket,
    staging: Option<Arc<StagingBuffer>>,
    /// Phase B.3 (N8): per-op self-overlap scratch images. Renamed from
    /// `Option<ScratchImage>` to `Vec<ScratchImage>` so the frame builder
    /// close-path walk over `open_frame.ops` can `std::mem::take` every
    /// `RecordedCopyArea::self_overlap_scratch` into one batch's
    /// `SubmittedOp`. Legacy `copy_area` self-overlap path (engine.rs:2937)
    /// transiently pushes a single-element Vec until that body is rewritten
    /// in Task 2.
    scratch: Vec<ScratchImage>,
    atlas_ticket: Option<FenceTicket>,
    generation: u64,
    retired_resources: Vec<Box<dyn crate::kms::scheduler::paint_batch::BatchResource>>,
}
```

Update every `SubmittedOp { ... }` literal:

- `scratch: None,` → `scratch: Vec::new(),` (~13 sites)
- `scratch: Some(scratch),` (the self-overlap path at engine.rs:2941) → `scratch: vec![scratch],`

Mechanically apply via:

```bash
sed -i 's/scratch: None,$/scratch: Vec::new(),/g' crates/yserver/src/kms/v2/engine.rs
```

(Then manually inspect the self-overlap site and replace `scratch: Some(scratch),` → `scratch: vec![scratch],` by hand.)

- [ ] **Step 7: Extend `close_open_frame` with the scratch walk (foundation)**

The walk runs in the post-record success path at engine.rs:1456-1467 (just before the `SubmittedOp` literal). Insert BEFORE the `inner.pending_group_ops.push(SubmittedOp { ... });` block:

```rust
// crates/yserver/src/kms/v2/engine.rs (insert at ~line 1456, before the
// `let inner = self.inner.as_mut().expect("inner");` block that pushes
// the SubmittedOp):

// Phase B.3 (N8): collect every self-overlap scratch from the recorded
// ops into a local Vec — the SubmittedOp will own them through fence
// retire. std::mem::take leaves the ops in place with `None` for the
// scratch slot (idempotent if the op never carried one). Done BEFORE
// flush_submit_group so close-failure drops the local on the stack
// (ScratchImage::Drop destroys Vk handles cleanly — no fence ticket
// exists yet at this point).
let frame_scratches: Vec<ScratchImage> = {
    open_frame
        .ops
        .iter_mut()
        .filter_map(|op| match op {
            super::frame_builder::RecordedOp::CopyArea(ca) => ca.self_overlap_scratch.take(),
            _ => None,
        })
        .collect()
};
```

Then in the `SubmittedOp` push at engine.rs:1459-1467, replace `scratch: Vec::new()` with `scratch: frame_scratches`:

```rust
inner.pending_group_ops.push(SubmittedOp {
    cb,
    ticket: frame_ticket.clone(),
    staging: None,
    scratch: frame_scratches,  // NEW (B.3 N8)
    atlas_ticket: None,
    generation,
    retired_resources: Vec::new(),
});
```

- [ ] **Step 8: Stub the new emit arms**

`emit_recorded_op_into_cb` at engine.rs:8425 already dispatches on the existing variants. Add 6 new arms, each `unimplemented!()`-with-a-message — the body rewrites in Phases 2–5 fill them in. Insert at the end of the match block (before the closing `}`):

```rust
// crates/yserver/src/kms/v2/engine.rs — in fn emit_recorded_op_into_cb:
use super::frame_builder::RecordedOp as Op;
match op {
    Op::GlyphUpload(up) => { /* existing — unchanged */ }
    Op::CompositeGlyphs(cg) => { /* existing — unchanged */ }
    Op::LayoutTransition(lt) => { /* existing — unchanged */ }
    Op::RenderComposite(rc) => emit_recorded_render_composite_into_cb(inner, cb, pins, rc),
    // Phase B.3 stubs — implemented per-op in Phases 2-5.
    Op::CopyArea(_) => unimplemented!("Phase B.3 Task 2: emit_recorded_copy_area_into_cb"),
    Op::PutImage(_) => unimplemented!("Phase B.3 Task 6: emit_recorded_put_image_into_cb"),
    Op::FillRect(_) => unimplemented!("Phase B.3 Task 8: emit_recorded_fill_rect_into_cb"),
    Op::LogicFill(_) => unimplemented!("Phase B.3 Task 10: emit_recorded_logic_fill_into_cb"),
    Op::ImageText(_) => unimplemented!("Phase B.3 Task 14: emit_recorded_image_text_into_cb"),
    Op::RenderTrapsOrTris(_) => unimplemented!(
        "Phase B.3 Task 12: emit_recorded_render_traps_or_tris_into_cb"
    ),
}
```

These stubs are unreachable in Task 1 (no body appends the new variants yet); each is filled by the corresponding Phase 2–5 task.

- [ ] **Step 9: Write failing test — size budget**

```rust
// crates/yserver/src/kms/v2/frame_builder.rs — inside `mod tests`:
#[test]
fn b3_recorded_op_size_budget() {
    use std::mem::size_of;
    // RecordedOp tag + Box ptr (B.2's invariant; B.3 keeps).
    assert!(
        size_of::<RecordedOp>() <= 256,
        "RecordedOp grew past 256B: {}",
        size_of::<RecordedOp>(),
    );
    // Each payload (un-boxed) stays under 512B individually.
    assert!(size_of::<RecordedCopyArea>() <= 512);
    assert!(size_of::<RecordedPutImage>() <= 512);
    assert!(size_of::<RecordedFillRect>() <= 512);
    assert!(size_of::<RecordedLogicFill>() <= 512);
    assert!(size_of::<RecordedImageText>() <= 512);
    assert!(size_of::<RecordedRenderTrapsOrTris>() <= 512);
}
```

If `RecordedRenderTrapsOrTris` exceeds 512B (Open Question 1 in the spec), Box-wrap inner fields (`Box<Vec<vk::Rect2D>>` for clip_scissors, or pull `RecordedTrapSrcKind` into a `Box<RecordedTrapSrcKind>`). Don't redesign — payload-layout detail only.

- [ ] **Step 10: Write failing test — `dst_id()` exhaustiveness**

```rust
// crates/yserver/src/kms/v2/frame_builder.rs — inside `mod tests`:
#[test]
fn b3_recorded_op_dst_id_covers_every_variant() {
    // Construct each variant with the minimum data and assert dst_id()
    // returns the expected `Some(id)` / `None`. The pattern match in
    // RecordedOp::dst_id() is exhaustive (no `_ =>` arm); this test
    // fails compilation if a new variant is added without updating
    // dst_id() — that's the primary signal we want.
    //
    // Build a concrete sample of each variant. For Box-wrapped variants
    // construct minimal payloads with safe placeholder Vk handles
    // (vk::Image::null(), vk::ImageView::null()). dst_id() reads only
    // the dst_id field, so null Vk handles don't matter.
    let id7 = DrawableId::for_tests(7);

    let composite_glyphs = RecordedOp::CompositeGlyphs(RecordedCompositeGlyphs {
        dst_id: id7,
        foreground_rgba: [0.0; 4],
        glyphs: Vec::new(),
        clip_scissors: Vec::new(),
        damage_rect: None,
    });
    assert_eq!(composite_glyphs.dst_id(), Some(id7));

    // GlyphUpload returns None (utility variant — writes to atlas).
    let glyph_upload = RecordedOp::GlyphUpload(RecordedGlyphUpload {
        staging_pin_idx: PinnedStagingIdx(0),
        atlas_x: 0, atlas_y: 0, w: 0, h: 0,
        insert_key: GlyphKey { font_xid: 0, codepoint: 0 },
        insert_entry: AtlasEntry { atlas_x: 0, atlas_y: 0, w: 0, h: 0, pen_left: 0, pen_top: 0 },
    });
    assert_eq!(glyph_upload.dst_id(), None);

    // LayoutTransition returns None.
    let layout_transition = RecordedOp::LayoutTransition(RecordedLayoutTransition {
        drawable_id: id7,
        src_stage: vk::PipelineStageFlags2::empty(),
        src_access: vk::AccessFlags2::empty(),
        dst_stage: vk::PipelineStageFlags2::empty(),
        dst_access: vk::AccessFlags2::empty(),
        target_layout: vk::ImageLayout::UNDEFINED,
    });
    assert_eq!(layout_transition.dst_id(), None);

    // The 6 B.3 variants all return Some(dst_id). Build minimal payloads.
    // (Spelling out one — repeat the pattern for the others or use a
    // helper closure if preferred. Keep the test code explicit so a
    // future variant addition surfaces here, too.)
    let copy_area = RecordedOp::CopyArea(Box::new(RecordedCopyArea {
        dst_id: id7,
        src_id: DrawableId::for_tests(8),
        src_rect: vk::Rect2D::default(),
        dst_rect: vk::Rect2D::default(),
        src_format: vk::Format::B8G8R8A8_UNORM,
        src_extent: vk::Extent2D::default(),
        dst_extent: vk::Extent2D::default(),
        src_image: vk::Image::null(),
        dst_image: vk::Image::null(),
        src_old_layout: vk::ImageLayout::UNDEFINED,
        dst_old_layout: vk::ImageLayout::UNDEFINED,
        self_overlap_scratch: None,
    }));
    assert_eq!(copy_area.dst_id(), Some(id7));
    // ... repeat for PutImage, FillRect, LogicFill, ImageText, RenderTrapsOrTris ...
}
```

If the placeholder payload construction is too verbose, factor a `fn _bg_payload<T: Default>()` helper — but **show the explicit construction inline for at least 3 variants** so a reader can grep this test for "what does Variant X look like under construction?".

- [ ] **Step 11: Write failing test — close-path scratch walk yields empty vec when no CopyArea appended**

```rust
// crates/yserver/tests/v2_acceptance.rs — new test:
#[test]
#[ignore = "requires Vk fixture — gated to v2_acceptance harness"]
fn b3_close_path_scratch_walk_yields_empty_for_no_copy_area_frames() {
    // Spec acceptance gate: a frame with NO RecordedCopyArea ops produces
    // a SubmittedOp with `scratch: Vec::new()` — the walk doesn't allocate
    // and doesn't push spurious entries.
    //
    // Setup: open a frame with one composite_glyphs op (B.1 path —
    // doesn't allocate scratch), close it via the timeout helper.
    // Inspect the post-close `SubmittedOp` via the existing test surface.
    //
    // Reuse the `composite_glyphs_*_collapses_*` test fixture pattern at
    // v2_acceptance.rs:3960 — same Vk-required gate, same `with_v2_backend`
    // helper.
    //
    // Assertion shape:
    //   let pre = be.pending_group_ops_count_for_tests();
    //   /* push glyph op + close */
    //   /* peek the most recent submitted op's `scratch.len()` via a
    //      new accessor `most_recent_submitted_op_scratch_len_for_tests()`
    //      — add it to engine.rs as a `#[cfg(test)]` helper mirroring
    //      `pending_group_ops_count_for_tests` */
    //   assert_eq!(scratch_len, 0);
}
```

The accessor is one line on `RenderEngine`:

```rust
// crates/yserver/src/kms/v2/engine.rs:
#[cfg(test)]
pub(crate) fn most_recent_submitted_op_scratch_len_for_tests(&self) -> usize {
    self.inner
        .as_ref()
        .and_then(|i| i.pending_group_ops.last().or(i.submitted.back()))
        .map_or(0, |op| op.scratch.len())
}
```

Plumb to `KmsBackendV2` if v2_acceptance uses the backend wrapper.

- [ ] **Step 12: Run + verify + commit**

```bash
cargo build -p yserver
cargo test -p yserver --lib b3_
cargo +nightly fmt --check
cargo clippy -p yserver --all-targets
```

Expected: clean build; size-budget + dst_id tests pass; existing tests stay green (the scratch rename is mechanical; the close-path walk yields empty vecs because no op variant produces scratch yet).

```bash
git add -u
git commit -m "feat(v2/frame_builder): B.3 Task 1 — new RecordedOp variants + scratch slot extension

Adds 6 new RecordedOp variants (CopyArea, PutImage, FillRect, LogicFill,
ImageText, RenderTrapsOrTris) with stub unimplemented!() emit arms. Each
variant is Box-wrapped per the 256B RecordedOp size budget; per-payload
size assertions cap at 512B individually.

Adds RecordedOp::dst_id() helper (N10) covering every variant explicitly
— a future variant addition without updating dst_id() fails to compile.
Adds OpenFrame::pending_present_completions slot (N10) initialized empty
at open_for_paint.

Renames SubmittedOp::scratch: Option<ScratchImage> → Vec<ScratchImage>
(N8). Existing call sites mechanically updated: `scratch: None` →
`scratch: Vec::new()`; the legacy copy_area self-overlap path at
engine.rs:2941 transiently pushes vec![scratch] until rewritten in Task 2.

Extends close_open_frame with a per-op scratch walk (N8 + Pitfall 7):
before the SubmittedOp push, std::mem::take every
RecordedCopyArea::self_overlap_scratch into a local Vec<ScratchImage>
and pass it as `scratch:`. Empty vec for any frame without CopyArea
ops — preserves B.1/B.2 behavior."
```

---

## Phase 2 — TRANSFER family (6 tasks: 2 per op × 3 ops)

Each TRANSFER op rewrite: body + integration test. All three share the `RecordedCopyArea` variant for `copy_area` + `cow_copy_area`; `put_image` uses `RecordedPutImage`. **`cow_copy_area`'s task is atomic** — rewrites the body, deletes `pending_cow_batch`, migrates PRESENT-completion attach.

### Task 2: copy_area body rewrite (in-place, no `_legacy` suffix)

**Files:**

- Modify: `crates/yserver/src/kms/v2/engine.rs` — replace the `copy_area` body at engine.rs:2735-2949 (the entire function up to the disjoint-path tail at line 2950 — that disjoint path is also rewritten, see below) with the new frame-builder-resident body. Add `emit_recorded_copy_area_into_cb` helper.
- Modify: `crates/yserver/src/kms/v2/engine.rs` — fill in the `Op::CopyArea` arm in `emit_recorded_op_into_cb` (replacing the Task 1 stub).

- [ ] **Step 1: Write the failing integration test (TDD)**

```rust
// crates/yserver/tests/v2_acceptance.rs:
#[test]
#[ignore = "requires Vk fixture — gated to v2_acceptance harness"]
fn v2_frame_builder_copy_area_collapses_two_in_one_frame() {
    // Spec acceptance gate (Phase 2): two consecutive copy_area calls in
    // the same open frame produce exactly ONE SubmittedOp + ONE
    // vkQueueSubmit2. Pre-B.3 each call submitted independently.
    //
    // Setup: create two pixmaps (src + dst), open a frame via a prior
    // composite_glyphs (or render_composite) call, then issue two
    // copy_area calls with disjoint src+dst (so no self-overlap path).
    //
    // Assertions:
    //   - After both calls: `frame_builder_is_open() == true` (not
    //     closed by an M2 close).
    //   - `peek_ops_kinds_for_tests()` returns at least
    //     [..., CopyArea, CopyArea] in order.
    //   - Force-close via the timeout helper.
    //   - `telemetry_submit_group_flushes_for_tests` delta == 1
    //     (one flush for the closed frame).
    //   - `frame_builder_close_reason_non_ported_paint_op` lifetime
    //     counter UNCHANGED from pre-test value (copy_area no longer
    //     fires the M2 close).
}
```

Run + confirm the test fails (compiles to "CopyArea variant not appended by `copy_area` yet"):

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_copy_area_collapses_two_in_one_frame
# Expected: test ignored without VK; with VK fixture: fails on the close-
# reason or peek assertion.
```

- [ ] **Step 2: Replace the `copy_area` body**

The new body's order is fixed by N8 + N9 (allocation FIRST, then preflight mutation, then push). Replace the entire function body (engine.rs:2735-2949):

```rust
// crates/yserver/src/kms/v2/engine.rs (replace fn copy_area):
pub(crate) fn copy_area(
    &mut self,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    src: DrawableId,
    dst: DrawableId,
    src_rect: vk::Rect2D,
    dst_pos: vk::Offset2D,
) -> Result<(), RenderError> {
    // Phase B.3 (N9): empty-input fast-path FIRST — before flush_render_batch.
    if src_rect.extent.width == 0 || src_rect.extent.height == 0 {
        return Ok(());
    }
    // Phase B.3 (N9): renderer_failed check before any open-frame mutation.
    if platform.renderer_failed {
        return Err(RenderError::RendererFailed);
    }
    // Phase B.3 (N9): flush pending_render_batch at entry. May close an
    // open frame (chronological X11 ordering with pre-existing batches).
    self.flush_render_batch(store, platform)?;

    // Preflight: read src + dst metadata + format check WITHOUT mutating
    // anything in the open frame.
    let Some(inner) = self.inner.as_mut() else {
        return Err(RenderError::NoVk);
    };
    let (src_image, src_extent, src_format) = {
        let d = store.get(src).ok_or(RenderError::UnknownDrawable(src))?;
        (d.storage.image, d.storage.extent, d.storage.format)
    };
    let (dst_image, dst_extent, dst_format) = {
        let d = store.get(dst).ok_or(RenderError::UnknownDrawable(dst))?;
        (d.storage.image, d.storage.extent, d.storage.format)
    };
    if src_format != dst_format {
        return Err(RenderError::UnsupportedDepth(0));
    }

    // Clamp + project (preserve legacy arithmetic from the pre-B.3 body).
    let src_rect = clamp_rect(src_rect, src_extent);
    let dst_pos_clamped = vk::Offset2D {
        x: dst_pos.x.max(0),
        y: dst_pos.y.max(0),
    };
    let copy_w = u32::try_from(
        (i32::from_le_bytes(i32::to_le_bytes(dst_pos.x))
            + i32::try_from(src_rect.extent.width).unwrap_or(0))
        .min(i32::try_from(dst_extent.width).unwrap_or(i32::MAX))
            - dst_pos_clamped.x,
    )
    .unwrap_or(0)
    .min(src_rect.extent.width);
    let copy_h = u32::try_from(
        (i32::from_le_bytes(i32::to_le_bytes(dst_pos.y))
            + i32::try_from(src_rect.extent.height).unwrap_or(0))
        .min(i32::try_from(dst_extent.height).unwrap_or(i32::MAX))
            - dst_pos_clamped.y,
    )
    .unwrap_or(0)
    .min(src_rect.extent.height);
    if copy_w == 0 || copy_h == 0 {
        return Ok(());
    }
    let dst_rect = vk::Rect2D {
        offset: dst_pos_clamped,
        extent: vk::Extent2D { width: copy_w, height: copy_h },
    };

    // Phase B.3 (N8): self-overlap path allocates the scratch FIRST,
    // BEFORE any open-frame mutation. Allocation failure returns Err
    // with the frame untouched (no rollback needed).
    let self_overlap_scratch: Option<ScratchImage> = if src == dst {
        Some(allocate_scratch_image(
            &inner.vk.clone(),
            platform,
            copy_w,
            copy_h,
            src_format,
        )?)
    } else {
        None
    };

    // Open the frame if not already open. Phase B.2 Mechanism 2: bump
    // acquire_generation at open + capture on OpenFrame. (Same pattern
    // as composite_glyphs_via_frame_builder at engine.rs:5278-5287.)
    if !inner.frame_builder.is_open() {
        let _ = inner;
        let ticket = platform.submit_group_ticket_or_open()?;
        let inner = self.inner.as_mut().expect("inner");
        inner.acquire_generation = inner.acquire_generation.saturating_add(1);
        let frame_generation = inner.acquire_generation;
        inner.frame_builder.open_for_paint(ticket, frame_generation);
    }
    let inner = self.inner.as_mut().expect("inner");
    let frame_ticket = inner
        .frame_builder
        .open
        .as_ref()
        .expect("just opened")
        .ticket
        .clone();

    // Prelude state mutations — overlay first-touch + ticket-touch for
    // BOTH src and dst. (For self-overlap, src == dst; first_touch is
    // idempotent so doing both is safe.)
    let dst_pre_layout = store.get(dst).map(|d| d.storage.current_layout).unwrap_or(vk::ImageLayout::UNDEFINED);
    let src_pre_layout = if src == dst {
        dst_pre_layout
    } else {
        store.get(src).map(|d| d.storage.current_layout).unwrap_or(vk::ImageLayout::UNDEFINED)
    };
    let prior_dst_ticket = store.get(dst).and_then(|d| d.last_render_ticket.clone());
    let prior_src_ticket = if src == dst {
        prior_dst_ticket.clone()
    } else {
        store.get(src).and_then(|d| d.last_render_ticket.clone())
    };
    {
        let open = inner.frame_builder.open.as_mut().expect("open");
        open.touched.first_touch(dst, prior_dst_ticket);
        open.layouts.first_touch_drawable(dst, dst_pre_layout);
        if src != dst {
            open.touched.first_touch(src, prior_src_ticket);
            open.layouts.first_touch_drawable(src, src_pre_layout);
        }
    }
    store.touch_render_fence(dst, frame_ticket.clone());
    if src != dst {
        store.touch_render_fence(src, frame_ticket.clone());
    }
    store.damage(dst, dst_rect);

    // Phase B.3 (N1 + N8): append the op + set BOTH dst and src overlays
    // to SHADER_READ_ONLY_OPTIMAL (single-terminal-layout rule).
    let payload = Box::new(super::frame_builder::RecordedCopyArea {
        dst_id: dst,
        src_id: src,
        src_rect,
        dst_rect,
        src_format,
        src_extent,
        dst_extent,
        src_image,
        dst_image,
        src_old_layout: src_pre_layout,
        dst_old_layout: dst_pre_layout,
        self_overlap_scratch,
    });
    let layout_updates: &[(DrawableId, vk::ImageLayout)] = if src == dst {
        &[(dst, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
    } else {
        &[
            (dst, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
            (src, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL),
        ]
    };
    {
        let open = inner.frame_builder.open.as_mut().expect("open");
        open.push_op_and_set_layouts(super::frame_builder::RecordedOp::CopyArea(payload), layout_updates);
    }
    Ok(())
}
```

Notes for the implementer:

- The exact `clamp_rect` / `dst_pos_clamped` arithmetic is preserved verbatim from the pre-B.3 body (engine.rs:2774-2806). Don't refactor it — these expressions handle X11 wire negative offsets, and `i32::from_le_bytes(i32::to_le_bytes(...))` is a no-op identity that the legacy code uses as a "trust this signed cast" marker.
- The `let _ = inner;` cue releases the borrow before `submit_group_ticket_or_open` (mirror engine.rs:5279-5282).
- `allocate_scratch_image` lives at engine.rs:8901 — its signature is `fn allocate_scratch_image(vk: &Arc<VkContext>, platform: &mut PlatformBackend, w: u32, h: u32, format: vk::Format) -> Result<ScratchImage, RenderError>`. Confirm before pasting.

- [ ] **Step 3: Implement `emit_recorded_copy_area_into_cb`**

The disjoint case mirrors engine.rs:2951-3045 (the pre-B.3 disjoint tail). The self-overlap case mirrors engine.rs:2814-2918 verbatim — three barrier pairs + two `cmd_copy_image` calls. Single function handles both:

```rust
// crates/yserver/src/kms/v2/engine.rs (place near
// emit_recorded_render_composite_into_cb at line ~8526):

fn emit_recorded_copy_area_into_cb(
    inner: &mut RenderEngineInner,
    cb: vk::CommandBuffer,
    ca: &super::frame_builder::RecordedCopyArea,
) -> Result<(), RenderError> {
    let device = &inner.vk.device;
    if let Some(scratch) = ca.self_overlap_scratch.as_ref() {
        // Self-overlap: mirror engine.rs:2814-2918's three-barrier sequence.
        barrier_to_layout(
            device, cb, ca.src_image,
            ca.src_old_layout, vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::PipelineStageFlags2::ALL_COMMANDS,
            vk::AccessFlags2::SHADER_SAMPLED_READ
                | vk::AccessFlags2::TRANSFER_WRITE
                | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
        );
        barrier_to_layout(
            device, cb, scratch.image,
            vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::PipelineStageFlags2::TOP_OF_PIPE,
            vk::AccessFlags2::empty(),
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
        );
        let region1 = [vk::ImageCopy::default()
            .src_subresource(color_layers())
            .src_offset(vk::Offset3D { x: ca.src_rect.offset.x, y: ca.src_rect.offset.y, z: 0 })
            .dst_subresource(color_layers())
            .dst_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .extent(vk::Extent3D {
                width: ca.dst_rect.extent.width,
                height: ca.dst_rect.extent.height,
                depth: 1,
            })];
        unsafe {
            device.cmd_copy_image(
                cb,
                ca.src_image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                scratch.image, vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region1,
            );
        }
        barrier_to_layout(
            device, cb, scratch.image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
        );
        barrier_to_layout(
            device, cb, ca.src_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL, vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_READ,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
        );
        let region2 = [vk::ImageCopy::default()
            .src_subresource(color_layers())
            .src_offset(vk::Offset3D { x: 0, y: 0, z: 0 })
            .dst_subresource(color_layers())
            .dst_offset(vk::Offset3D { x: ca.dst_rect.offset.x, y: ca.dst_rect.offset.y, z: 0 })
            .extent(vk::Extent3D {
                width: ca.dst_rect.extent.width,
                height: ca.dst_rect.extent.height,
                depth: 1,
            })];
        unsafe {
            device.cmd_copy_image(
                cb,
                scratch.image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                ca.src_image, vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &region2,
            );
        }
        // src (== dst) → SHADER_READ_ONLY_OPTIMAL (N1 terminal-layout rule).
        barrier_to_layout(
            device, cb, ca.src_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COPY,
            vk::AccessFlags2::TRANSFER_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );
        return Ok(());
    }
    // Disjoint case: two-barrier sequence (N1 exact masks).
    barrier_to_layout(
        device, cb, ca.src_image,
        ca.src_old_layout, vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ
            | vk::AccessFlags2::TRANSFER_WRITE
            | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_READ,
    );
    barrier_to_layout(
        device, cb, ca.dst_image,
        ca.dst_old_layout, vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ
            | vk::AccessFlags2::TRANSFER_WRITE
            | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_WRITE,
    );
    let region = [vk::ImageCopy::default()
        .src_subresource(color_layers())
        .src_offset(vk::Offset3D { x: ca.src_rect.offset.x, y: ca.src_rect.offset.y, z: 0 })
        .dst_subresource(color_layers())
        .dst_offset(vk::Offset3D { x: ca.dst_rect.offset.x, y: ca.dst_rect.offset.y, z: 0 })
        .extent(vk::Extent3D {
            width: ca.dst_rect.extent.width,
            height: ca.dst_rect.extent.height,
            depth: 1,
        })];
    unsafe {
        device.cmd_copy_image(
            cb,
            ca.src_image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            ca.dst_image, vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &region,
        );
    }
    // Post-barriers: BOTH src and dst → SHADER_READ_ONLY_OPTIMAL.
    barrier_to_layout(
        device, cb, ca.src_image,
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_READ,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    barrier_to_layout(
        device, cb, ca.dst_image,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    Ok(())
}
```

Then in `emit_recorded_op_into_cb` replace the Task 1 stub:

```rust
Op::CopyArea(ca) => emit_recorded_copy_area_into_cb(inner, cb, ca),
```

Helpers used: `barrier_to_layout` (engine.rs near the layout-transition helpers), `color_layers()` (engine.rs scoped helper). Confirm both are in scope; if `color_layers` is private to a different module, qualify the path.

- [ ] **Step 4: Run unit tests + integration test**

```bash
cargo build -p yserver
cargo test -p yserver --lib
cargo test -p yserver --test v2_acceptance v2_frame_builder_copy_area_collapses_two_in_one_frame
```

Expected: clean build; the new integration test passes; existing copy_area tests at engine.rs:9858 + engine.rs:11446 (self-overlap path) stay green (they observe externally-visible behavior — the rewrite preserves it).

- [ ] **Step 5: Commit**

```bash
git add -u
git commit -m "feat(v2/engine): B.3 Task 2 — port copy_area to frame builder

Replaces copy_area's direct-submit body with a frame-builder-resident
append. Body order per N8 + N9: empty-input fast-path → renderer_failed
check → flush_render_batch → preflight (src/dst metadata + format check)
→ self-overlap scratch allocation (N8 — BEFORE any frame state mutation)
→ open frame if not open → first_touch + ticket-touch + damage → append
RecordedOp::CopyArea via push_op_and_set_layouts with both dst and src
overlays set to SHADER_READ_ONLY_OPTIMAL (N1 single-terminal rule).

The close_open_frame_for_non_ported_op call is GONE — copy_area extends
the open frame instead of closing it. The self-overlap scratch lives on
RecordedCopyArea::self_overlap_scratch until close (single source of
truth per Pitfall 7); Task 1's close-path scratch walk takes it into the
SubmittedOp::scratch Vec on close-success.

Emit: emit_recorded_copy_area_into_cb handles BOTH disjoint and self-
overlap cases; barrier shapes mirror the legacy paths verbatim
(engine.rs:2814-2918 self-overlap; engine.rs:2951-3045 disjoint)."
```

### Task 3: copy_area collapse integration test

Wires the test stub from Task 2 step 1 into the v2_acceptance harness with the actual fixture pattern. Cross-checks the existing copy_area unit tests (engine.rs:9858 disjoint round-trip, engine.rs:11446 self-overlap scratch path) still pass.

**Files:**

- Modify: `crates/yserver/tests/v2_acceptance.rs` — add the full integration test body.

- [ ] **Step 1: Locate the fixture pattern**

```bash
grep -nE 'fn v2_frame_builder_.*collapses_to_one|with_v2_backend' crates/yserver/tests/v2_acceptance.rs | head -10
```

Pick the closest existing pattern (e.g. B.2's `v2_frame_builder_render_composite_collapses_to_one_per_frame` near v2_acceptance.rs:3956-4015) as the template.

- [ ] **Step 2: Write the test body**

```rust
// crates/yserver/tests/v2_acceptance.rs:
#[test]
#[ignore = "requires Vk fixture — gated to v2_acceptance harness"]
fn v2_frame_builder_copy_area_collapses_two_in_one_frame() {
    use yserver::kms::v2::frame_builder::CloseReason;
    common::with_v2_backend(|be| {
        // Create src + dst pixmaps. Use the existing `make_pixmap_for_tests`
        // helper at common/mod.rs (search if path differs).
        let src = common::make_pixmap_for_tests(be, 64, 64, 24);
        let dst = common::make_pixmap_for_tests(be, 64, 64, 24);

        let pre_flushes = be.telemetry_submit_group_flushes_for_tests();
        let pre_non_ported = be.telemetry_close_reason_non_ported_for_tests();

        let src_rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 0, y: 0 },
            extent: ash::vk::Extent2D { width: 32, height: 32 },
        };
        be.engine_copy_area_for_tests(src, dst, src_rect, ash::vk::Offset2D { x: 0, y: 0 })
            .unwrap();
        be.engine_copy_area_for_tests(
            src, dst, src_rect,
            ash::vk::Offset2D { x: 32, y: 32 },
        )
        .unwrap();

        // Frame should still be open — copy_area extends, doesn't close.
        assert!(
            be.frame_builder_is_open_for_tests(),
            "open frame should survive two copy_area calls"
        );

        // Force-close via timeout helper.
        be.close_open_frame_for_timeout_for_tests().unwrap();

        let post_flushes = be.telemetry_submit_group_flushes_for_tests();
        let post_non_ported = be.telemetry_close_reason_non_ported_for_tests();

        assert_eq!(
            post_flushes.saturating_sub(pre_flushes),
            1,
            "two copy_area calls collapsed to ONE submit"
        );
        assert_eq!(
            post_non_ported.saturating_sub(pre_non_ported),
            0,
            "copy_area should not fire CloseReason::NonPortedPaintOp"
        );
    });
}
```

If any of these helpers don't exist on `KmsBackendV2`, add a thin pass-through:

- `engine_copy_area_for_tests` — `#[cfg(test)] pub fn engine_copy_area_for_tests(&mut self, src, dst, src_rect, dst_pos) -> Result<(), _> { self.engine.copy_area(&mut self.store, &mut self.platform, src, dst, src_rect, dst_pos) }`. Mirror existing `engine_*_for_tests` wrappers if present.
- `telemetry_close_reason_non_ported_for_tests` — read the lifetime counter from `telemetry.rs:105`'s `frame_builder_close_reason_non_ported_paint_op` field.
- `frame_builder_is_open_for_tests` — pass-through to `RenderEngine::frame_builder_is_open` at engine.rs:1646.

- [ ] **Step 3: Run + commit**

```bash
cargo test -p yserver --test v2_acceptance v2_frame_builder_copy_area_collapses_two_in_one_frame
```

```bash
git add -u
git commit -m "test(v2): B.3 Task 3 — copy_area collapses two-in-one-frame integration test

Two consecutive copy_area calls in one frame produce ONE SubmittedOp
(telemetry_submit_group_flushes delta=1) and ZERO CloseReason::
NonPortedPaintOp events (lifetime counter delta=0)."
```

### Task 4: cow_copy_area atomic rewrite + delete pending_cow_batch + migrate PRESENT-completion attach (N10)

The largest single task in B.3. Partial deletion of `pending_cow_batch` infrastructure leaves dangling references that don't compile, so this whole change ships in ONE commit. Per Pitfall 9, the local-edit order matters.

**Files:**

- Modify: `crates/yserver/src/kms/v2/engine.rs` — rewrite `cow_copy_area` (engine.rs:3079-3283), `cow_copy_area_open_first` (engine.rs:3289-3411) **DELETED**, `flush_cow_batch` (engine.rs:3412-3579) **DELETED**, `attach_cow_present_completion` (engine.rs:3581-3597) rewritten per N10, `drain_cow_flush_records` (engine.rs:3614-3618) **DELETED**, `RenderEngineInner::pending_cow_batch` (engine.rs:622) **DELETED**, `cow_flush_records` (engine.rs:630) **DELETED**, `PendingCowBatch` struct (engine.rs:133) **DELETED**, `close_open_frame` (engine.rs:1264) extended with N10 three-branch PRESENT plumbing, `has_pending_batches_for_tests` (engine.rs:1718) updated.
- Modify: `crates/yserver/src/kms/v2/backend.rs` — every `flush_cow_batch` call site (backend.rs:2185-2193, 2523-2525, 4991-4993, 5066-5068, 6129-6131, 9918-9920) **DELETED**. The `enqueue_present_completion` flow at backend.rs:9904 calls `attach_cow_present_completion` first; the failure-fallback flush stays as a `close_open_frame` with `CloseReason::PresentCompletionSignal` instead.
- Tests: per Step 9 below, **delete** the 6 legacy cow-batch unit tests (engine.rs:9974 `cow_copy_area_coalesces_four_srcs_into_one_submit`, engine.rs:10171 `cow_copy_area_repeated_src_skips_redundant_transition`, engine.rs:10312 `cow_copy_area_flush_via_non_cow_op`, engine.rs:10948 `cow_copy_area_open_marks_src_last_render_ticket_immediately`, the two SubmitGroup-cap cow tests near engine.rs:~13725 + ~14130). They directly call `flush_cow_batch` / `drain_cow_flush_records` or read `inner.pending_cow_batch` and cannot compile after the Step-5 infra deletion. Their invariants are subsumed by Task 4's two new integration tests in `crates/yserver/tests/v2_acceptance.rs` (`v2_frame_builder_cow_copy_area_collapses_two_in_one_frame` + `v2_frame_builder_cow_copy_area_delivers_present_completion`) plus the surviving FrameLayoutTable + B.1/B.2 lifecycle tests. Also update narrative-comment drift at `v2_acceptance.rs:3440` + `:3502` and verify those tests' submit-count assertions still hold under the new collapse semantics.

- [ ] **Step 1: Write the integration test FIRST (TDD)**

```rust
// crates/yserver/tests/v2_acceptance.rs:
#[test]
#[ignore = "requires Vk fixture — gated to v2_acceptance harness"]
fn v2_frame_builder_cow_copy_area_collapses_two_in_one_frame() {
    use yserver::kms::v2::frame_builder::CloseReason;
    common::with_v2_backend(|be| {
        // Create a COW drawable + a regular src pixmap.
        let cow = common::make_cow_drawable_for_tests(be, 256, 256);
        let src = common::make_pixmap_for_tests(be, 256, 256, 24);

        let pre_flushes = be.telemetry_submit_group_flushes_for_tests();

        let src_rect = ash::vk::Rect2D {
            offset: ash::vk::Offset2D { x: 0, y: 0 },
            extent: ash::vk::Extent2D { width: 64, height: 64 },
        };
        be.engine_cow_copy_area_for_tests(cow, src, src_rect, ash::vk::Offset2D { x: 0, y: 0 }).unwrap();
        be.engine_cow_copy_area_for_tests(cow, src, src_rect, ash::vk::Offset2D { x: 64, y: 0 }).unwrap();

        assert!(be.frame_builder_is_open_for_tests(), "frame should still be open");
        be.close_open_frame_for_timeout_for_tests().unwrap();
        let post_flushes = be.telemetry_submit_group_flushes_for_tests();
        assert_eq!(post_flushes.saturating_sub(pre_flushes), 1);

        // Confirm pending_cow_batch is gone (compile-time signal).
        assert!(!be.has_pending_batches_for_tests());
    });
}

#[test]
#[ignore = "requires Vk fixture — gated to v2_acceptance harness"]
fn v2_frame_builder_cow_copy_area_delivers_present_completion() {
    // N10: PRESENT-completion attach during a frame containing
    // cow_copy_areas correctly delivers a CompletedPresentEvent
    // when the frame retires.
    common::with_v2_backend(|be| {
        let cow = common::make_cow_drawable_for_tests(be, 256, 256);
        let src = common::make_pixmap_for_tests(be, 256, 256, 24);

        be.engine_cow_copy_area_for_tests(cow, src, /* ... */, /* ... */).unwrap();
        // Attach a synthetic PRESENT completion (use the existing
        // synthetic helpers if available; otherwise add them — search
        // `attach_synthetic_present_completion_to_cow_for_tests`).
        let event_id = be.attach_synthetic_present_completion_to_cow_for_tests(cow);

        be.close_open_frame_for_timeout_for_tests().unwrap();
        // Wait for the frame ticket to retire, then drain.
        be.drain_completed_present_events_for_tests();
        let events = be.collected_present_events_for_tests();
        assert!(events.iter().any(|e| e.serial == event_id));
    });
}
```

If `attach_synthetic_present_completion_to_cow_for_tests` doesn't exist yet, add it as a Task 4 step: it wraps `attach_cow_present_completion` with a synthetic `PendingPresentEntry { wake_pin: PinnedWake::None, event: CompletedPresentEvent { serial: <unique>, ... } }`. The pre-existing TODO at engine.rs:3510-3520 calls out exactly this helper as "needs adding before Phase B's frame builder lands".

- [ ] **Step 2: Rewrite `attach_cow_present_completion` (N10)**

```rust
// crates/yserver/src/kms/v2/engine.rs (replace fn attach_cow_present_completion
// at engine.rs:3581):
pub(crate) fn attach_cow_present_completion(
    &mut self,
    cow_id: DrawableId,
    entry: super::present_completion::PendingPresentEntry,
) -> Result<(), super::present_completion::PendingPresentEntry> {
    let Some(inner) = self.inner.as_mut() else { return Err(entry); };
    let Some(open) = inner.frame_builder.open.as_mut() else { return Err(entry); };
    // Phase B.3 (N10): predicate is "WRITES to cow_id", not "touched
    // cow_id" — `touched` includes sampled-only references which would
    // attach completions to frames that never wrote the cow.
    let writes_to_cow = open.ops.iter().any(|op| op.dst_id() == Some(cow_id));
    if !writes_to_cow {
        return Err(entry);
    }
    open.pending_present_completions.push(entry);
    Ok(())
}
```

- [ ] **Step 3: Extend `close_open_frame` with the N10 three-branch PRESENT plumbing**

This replaces the existing engine.rs:1405-1467 region (the `append_result` block + `pending_group_ops.push` block) with the signal-aware variant:

```rust
// crates/yserver/src/kms/v2/engine.rs (replace engine.rs:1405-1467):

// Phase B.3 (N10) — branch (a) PRE-SUBMIT: acquire completion signal
// BEFORE end_and_submit_op_with_signal so the semaphore is queued on
// the submit's signal list. Failure routes through the existing
// append-failure rollback at engine.rs:1410-1446 (the 8 cleanup steps).
let completion_signal: Option<super::platform::PresentCompletionSignal> = {
    let pending_count = open_frame.pending_present_completions.len();
    if pending_count == 0 {
        None
    } else {
        let inner_mut = self.inner.as_mut().expect("inner");
        match platform.acquire_present_completion_signal() {
            Ok(s) => Some(s),
            Err(e) => {
                // Same rollback as the append-failure path (engine.rs:1410-1446).
                let device = &inner_mut.vk.device;
                if let Some(pool) = platform.ops_command_pool_handle() {
                    unsafe { device.free_command_buffers(pool, &[cb]) };
                }
                rollback_pre_submit(store, &mut open_frame);
                platform.renderer_failed = true;
                let inner_post = self.inner.as_mut().expect("inner");
                rollback_atlas(
                    inner_post,
                    open_frame.layouts.atlas,
                    open_frame.atlas_prev_ticket_snapshot.clone(),
                );
                for r in open_frame.pins.retired_resources.drain(..) {
                    r.release(&inner_post.vk);
                }
                if inner_post.pending_frame_close_events.len() < 1024 {
                    inner_post.pending_frame_close_events.push(
                        super::frame_builder::FrameCloseEvent {
                            reason, ops_in_frame: open_frame.ops.len(),
                            glyph_uploads_in_frame: open_frame.glyph_uploads_in_frame,
                            renders_in_frame, pin_count: open_frame.pins.len(),
                            aborted: true,
                        }
                    );
                }
                inner_post.frame_builder.complete_close_failure();
                return Err(RenderError::Vk(e));
            }
        }
    }
};
let completion_semaphore = completion_signal
    .as_ref()
    .map(super::platform::PresentCompletionSignal::semaphore);

// End CB + append to SubmitGroup with the semaphore (replaces the existing
// end_and_submit_op call at engine.rs:1408).
let append_result = {
    let inner = self.inner.as_mut().expect("inner");
    end_and_submit_op_with_signal(inner, platform, cb, &frame_ticket, completion_semaphore)
};
if let Err(e) = append_result {
    // Existing append-failure rollback (engine.rs:1410-1446) PLUS:
    // completion_signal drops with the local — submit never queued
    // the signal-op, so the events drop with the OpenFrame too.
    // ... (same 8-step cleanup as Step 2's signal-acquire failure branch)
    return Err(e);
}

// Push the frame's SubmittedOp.
let frame_scratches: Vec<ScratchImage> = {
    open_frame
        .ops
        .iter_mut()
        .filter_map(|op| match op {
            super::frame_builder::RecordedOp::CopyArea(ca) => ca.self_overlap_scratch.take(),
            _ => None,
        })
        .collect()
};
{
    let inner = self.inner.as_mut().expect("inner");
    let generation = open_frame.frame_generation;
    inner.pending_group_ops.push(SubmittedOp {
        cb,
        ticket: frame_ticket.clone(),
        staging: None,
        scratch: frame_scratches,
        atlas_ticket: None,
        generation,
        retired_resources: Vec::new(),
    });
}

let flush_outcome =
    self.flush_submit_group(platform, super::submit_group::FlushReason::FrameBuilder);

match flush_outcome {
    Ok(_) => {
        // Existing commit-after-Ok path PLUS:
        // Phase B.3 (N10) branch (b): drain pending_present_completions
        // into a PendingPresentBatch alongside the exported sync_file fd.
        let drained_completions: Vec<super::present_completion::PendingPresentEntry> =
            std::mem::take(&mut open_frame.pending_present_completions);
        if !drained_completions.is_empty() {
            let (wait, signal) = match completion_signal {
                Some(signal) => match signal.export_sync_file_fd() {
                    Ok(Some(fd)) => (super::present_completion::PresentBatchWait::Fd(fd), Some(signal)),
                    Ok(None) => (super::present_completion::PresentBatchWait::Ready, Some(signal)),
                    Err(e) => {
                        log::warn!(
                            "B.3 close_open_frame: vkGetSemaphoreFdKHR(SYNC_FD) failed: \
                             {e:?}; falling back to FenceTicket polling"
                        );
                        (super::present_completion::PresentBatchWait::Poll, Some(signal))
                    }
                },
                None => unreachable!("non-empty completions but no signal allocated"),
            };
            let inner = self.inner.as_mut().expect("inner");
            let ticket = inner.submitted.back().map(|op| op.ticket.clone());
            inner.pending_present_batches.push(super::present_completion::PendingPresentBatch {
                wait, ticket, signal, events: drained_completions,
            });
        }
        // ... (existing commit_close_success block — unchanged)
    }
    Err(e) => {
        // Phase B.3 (N10) branch (c): force-enqueue a degraded batch
        // BEFORE returning Err — never silent-drop X PRESENT events.
        // The completion_signal allocated in branch (a) drops with the
        // local — it was queued on a submit that failed.
        let drained_completions: Vec<super::present_completion::PendingPresentEntry> =
            std::mem::take(&mut open_frame.pending_present_completions);
        if !drained_completions.is_empty() {
            let inner = self.inner.as_mut().expect("inner");
            inner.pending_present_batches.push(super::present_completion::PendingPresentBatch {
                wait: super::present_completion::PresentBatchWait::Ready,
                ticket: None,
                signal: None,
                events: drained_completions,
            });
        }
        // ... (existing rollback_pre_submit + rollback_atlas + ... — unchanged)
    }
}
```

Note: the existing 1410-1446 rollback block IS large (37 lines). Don't inline-rewrite it inside the signal-acquire failure path — factor it into a `close_failure_rollback(self, store, platform, open_frame, reason, renders_in_frame, cb) -> RenderError` helper FIRST, then call it from all three failure sites (signal-acquire fail, append fail, record fail). That keeps the diff readable. The helper exists in spirit at engine.rs:1316-1346 + 1374-1402 + 1411-1446 + 1521-1548 — pull it out as part of this task.

- [ ] **Step 4: Rewrite `cow_copy_area`**

```rust
// crates/yserver/src/kms/v2/engine.rs (replace fn cow_copy_area at engine.rs:3079):
pub(crate) fn cow_copy_area(
    &mut self,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    cow_id: DrawableId,
    src: DrawableId,
    src_rect: vk::Rect2D,
    dst_pos: vk::Offset2D,
) -> Result<(), RenderError> {
    // N9: empty-input fast-path.
    if src_rect.extent.width == 0 || src_rect.extent.height == 0 {
        return Ok(());
    }
    if platform.renderer_failed {
        return Err(RenderError::RendererFailed);
    }
    // N9: flush pending_render_batch at entry. NO flush_cow_batch — that
    // helper is deleted in this task.
    self.flush_render_batch(store, platform)?;

    // Same-image overlap not handled on the cow path (legacy invariant
    // at engine.rs:3109-3111). Defend explicitly.
    if src == cow_id {
        return Err(RenderError::UnsupportedDepth(0));
    }

    // Identical body shape to copy_area's disjoint case — the cow IS a
    // regular Drawable per N3, the dst_id field just happens to point at
    // a Drawable that the backend tracks as a COW elsewhere.
    self.copy_area(store, platform, src, cow_id, src_rect, dst_pos)
}
```

The simplest implementation forwards to `copy_area` directly: per N3, `cow_id` is a regular `DrawableId`, and the body shape is identical. If the cow's storage path differs in some subtle way (e.g. damage tracking lands on a separate slot), inline the body instead of forwarding. **Confirm at write-time** by reading the legacy cow_copy_area body (engine.rs:3289-3411) for any per-cow side-effects beyond drawable storage mutation — if there are none, forwarding is the cleanest. If there are (e.g. `record_present` / `commit_bo_present` calls), inline the matching damage + record steps.

- [ ] **Step 5: Delete `pending_cow_batch` infrastructure**

Mechanical deletions, in this order:

```bash
cd /home/jos/Projects/yserver
# Delete PendingCowBatch struct (engine.rs:133, ~25 lines):
# (manual edit — remove the struct + its impl)
# Delete pending_cow_batch field on RenderEngineInner (engine.rs:622):
# Delete cow_flush_records field on RenderEngineInner (engine.rs:630):
# Delete the init at the engine.rs:990 (constructor):
#   cow_flush_records: Vec::new(),
# Delete cow_copy_area_open_first (engine.rs:3289-3411).
# Delete flush_cow_batch (engine.rs:3412-3579).
# Delete drain_cow_flush_records (engine.rs:3614 region — confirm line).
```

Each deletion exposes call sites — fix them as compile errors surface.

- [ ] **Step 6: Delete `flush_cow_batch` call sites in backend.rs**

Confirmed sites (`grep -n flush_cow_batch crates/yserver/src/kms/v2/backend.rs`):

- backend.rs:2185-2193 (`simulate_page_flip_complete_for_tests`)
- backend.rs:2523-2525 (`disable_output`)
- backend.rs:4991-4993 (`on_page_flip_ready`)
- backend.rs:5066-5068 (`maybe_composite`)
- backend.rs:6129-6131 (`release_overlay_window`)
- backend.rs:9918-9920 (`enqueue_present_completion` flush-fallback)

For backend.rs:9918-9920 specifically (the `attach_cow_present_completion` failure-fallback), the behavior changes: when attach returns Err (no open frame writes to `cow_id`), the backend should `enqueue_present_completion` immediately as an X11-protocol-visible event (mirroring the legacy "no batch present → fire immediately" path). Read the surrounding code at backend.rs:9890-9930 to understand the existing fallback shape — the change is "drop the flush_cow_batch line; keep the immediate-fire branch".

- [ ] **Step 7: Update `has_pending_batches_for_tests` (N10 minor)**

```rust
// crates/yserver/src/kms/v2/engine.rs (engine.rs:1718):
pub fn has_pending_batches_for_tests(&self) -> bool {
    self.inner
        .as_ref()
        .is_some_and(|i| i.frame_builder.is_open() || i.pending_render_batch.is_some())
}
```

The behavioral meaning is preserved: "is there any in-flight work the frame builder hasn't committed yet?".

- [ ] **Step 8: Delete the cow-flush telemetry surface**

Three locations:

- `backend.rs:1543-1573` — delete the entire `fn drain_cow_telemetry(&mut self)` helper.
- `backend.rs:5155` (`maybe_composite` tick) + `backend.rs:6140` (`release_overlay_window`) — delete the `self.drain_cow_telemetry();` call sites. The doc-comment at `backend.rs:6953` that mentions "`drain_cow_telemetry`" gets rewritten or dropped depending on the surrounding code; check at write-time.
- `telemetry.rs:571-591` — delete `fn record_cow_batch_flushed`.
- `telemetry.rs:82` + `telemetry.rs:89` — delete the `pub cow_batches_flushed: u64,` and `pub cow_copies_coalesced: u64,` bucket fields. These are part of `TelemetryBucket` (search the struct definition; the bucket is duplicated for per-second + lifetime via the `bucket` / `lifetime` field names visible in `record_cow_batch_flushed`). Both copies go.
- `telemetry.rs:307` (per-second log format string) — delete the `cow_batches_flushed/s={} cow_copies_coalesced/s={}` segment.
- `telemetry.rs:347-348` — delete the `b.cow_batches_flushed,` + `b.cow_copies_coalesced,` argument lines that fed the format string.
- `telemetry.rs:848` — update the doc comment ("Mirrors `record_cow_batch_flushed`") to point at the surviving render-batch telemetry helper instead, or drop the reference.

After this step the only cow-related identifier surviving in the tree is the `cow_id` Drawable field, which is correct — the COW is still a Drawable per N3; only its batch infrastructure is gone.

- [ ] **Step 9: Delete / rewrite the legacy cow-batch unit tests**

These tests directly call `flush_cow_batch` / `drain_cow_flush_records` or read `inner.pending_cow_batch` — they cannot compile after Step 5. Delete each one in full; their behavior is now covered structurally by the frame builder's per-frame collapse + Task 4's two new integration tests:

- `engine.rs:9974` — `cow_copy_area_coalesces_four_srcs_into_one_submit` (asserts `pending_cow_batch.coalesced_count == 4` then calls `flush_cow_batch` + reads `drain_cow_flush_records`). Replaced by Task 4 Step 1's `v2_frame_builder_cow_copy_area_collapses_two_in_one_frame` (same shape, two ops; the four-op variant adds no new signal — the collapse mechanism is frame-builder ops-walk-at-close which doesn't care about op count).
- `engine.rs:10171` — `cow_copy_area_repeated_src_skips_redundant_transition`. The legacy code path skipped a TRANSFER_SRC barrier on repeated srcs within one batch (engine.rs:3203-3239). Under B.3 the frame builder's per-drawable overlay already coalesces consecutive same-src layout transitions via `current_layout_for_drawable` — the new equivalent is "two same-src cow_copy_areas in one frame produce ops whose emit consults the overlay's current src layout and emits zero or one src barrier". Delete the legacy test; the invariant is now subsumed by the existing FrameLayoutTable unit tests (`first_touch_drawable_*` at frame_builder.rs:907-952).
- `engine.rs:10312` — `cow_copy_area_flush_via_non_cow_op`. Asserts a non-cow `fill_rect` mid-sequence flushes the pending cow batch. Under B.3 there's no pending cow batch; the closest analog (closing the open frame on M-trigger) is already covered by B.2's `close_open_frame_for_non_ported_op` path, which still fires for `render_composite_legacy`. No B.3-specific replacement is needed.
- `engine.rs:10948` — `cow_copy_area_open_marks_src_last_render_ticket_immediately`. Asserts src's `last_render_ticket` updates at batch-open (engine.rs:3238). Under B.3 the equivalent is "src's `last_render_ticket` updates on the first cow_copy_area's append" (the frame builder calls `store.touch_render_fence(src, frame_ticket.clone())` in the cow_copy_area body's prelude — see Task 4 Step 4, which forwards to `copy_area`'s prelude). Add a one-liner integration test asserting this; it's a 5-line addition to Task 4's test set.
- `engine.rs:~13725` (`engine_flush_submit_group_with_cow_batch_intermixed_*` or similar — find by grep for `flush_cow_batch B` near the SubmitGroup-cap tests at engine.rs:13725 + engine.rs:14130) — these tests orchestrate explicit `flush_cow_batch` calls to test deferred-graduation. Delete them; the SubmitGroup-cap=1 invariant (B.1's M1) means there's no group-cap interaction left to test on the cow side. The general M1 cap=1 test surface remains.
- `engine.rs:14130` (`cow_copy_area_open_first_*` simulation) — same fate.

For the v2_acceptance narrative-only sites:

- `v2_acceptance.rs:3440` + `v2_acceptance.rs:3502` — these are narrative comments inside the larger `v2_acceptance.rs:3401`'s mixed-sequence test (`telemetry_submit_group_flushes_for_tests` differential test). Update the comments to "Step 2: fill_rectangle on non-cow dst → the cow frame closes (M-trigger), the fill appends to a new frame" and similar. **Verify the test's submit-count assertions still hold** — under B.3 the cow_copy_area + fill_rect + composite mixed sequence collapses differently. If the existing assertion is `assert_eq!(after_flushes - initial_flushes, 3)` (three submits — one per op), update to whatever the new collapse produces (likely 1 if all three end up in one frame, 2 if there's a forced close between them due to M-trigger from `render_composite_legacy` under kill-switch-off). Run the test against the rewritten tree to find the actual count; don't guess.

- [ ] **Step 10: Implement the `CopyArea` emit arm**

Already done in Task 2 Step 3 (`emit_recorded_copy_area_into_cb`). cow_copy_area produces the same `RecordedOp::CopyArea` payload, so no additional dispatch arm is needed. Confirm.

- [ ] **Step 11: Build + run tests**

```bash
cargo build -p yserver
# Resolve any remaining `pending_cow_batch` / `flush_cow_batch` /
# `drain_cow_flush_records` / `drain_cow_telemetry` /
# `record_cow_batch_flushed` / `cow_batches_flushed` /
# `cow_copies_coalesced` references that fail to compile.
cargo test -p yserver --lib
cargo test -p yserver --test v2_acceptance v2_frame_builder_cow_copy_area_
cargo test -p yserver --test v2_acceptance flush_outcomes_  # narrative-comment test
```

Expected: every cow-batch surface identifier is gone from the build; both new integration tests pass; the legacy cow_copy_area unit tests at engine.rs:9974, 10171, 10312, 10948, ~13725, ~14130 are DELETED (not "still passing" — they don't exist any more). The B.3 invariants that subsumed them are covered by Task 4's new tests + the existing FrameLayoutTable + B.1/B.2 frame builder lifecycle tests.

- [ ] **Step 12: Commit (atomic)**

```bash
git add -u
git commit -m "feat(v2/engine): B.3 Task 4 — port cow_copy_area + delete pending_cow_batch + migrate PRESENT (N10)

ATOMIC change — partial deletion of pending_cow_batch leaves dangling
references that don't compile. All in one commit.

cow_copy_area rewrite: forwards to copy_area now that the COW is a
regular DrawableId per N3. The frame builder's per-frame collapse
subsumes the legacy same-dst pending_cow_batch coalescing.

Deletes: PendingCowBatch struct, RenderEngineInner::pending_cow_batch
field, RenderEngineInner::cow_flush_records, flush_cow_batch helper,
cow_copy_area_open_first, drain_cow_flush_records, and all 6
flush_cow_batch call sites in backend.rs.

Deletes the cow-flush telemetry surface: drain_cow_telemetry helper
(backend.rs:1543-1573) + its two call sites (backend.rs:5155 + 6140),
record_cow_batch_flushed helper (telemetry.rs:571-591), the
cow_batches_flushed + cow_copies_coalesced bucket fields and their
per-second log format slots (telemetry.rs:82, 89, 307, 347-348).

Deletes the legacy cow-batch unit tests (engine.rs:9974, 10171,
10312, 10948 + the two SubmitGroup-cap cow tests near engine.rs:13725
+ 14130) whose assertions directly read pending_cow_batch / call
flush_cow_batch / drain_cow_flush_records. Their invariants are
subsumed by Task 4's new frame-builder integration tests + the
existing FrameLayoutTable / B.1 / B.2 lifecycle tests.

N10 PRESENT-completion migration:
- Adds OpenFrame::pending_present_completions slot (in Task 1).
- Rewrites attach_cow_present_completion's predicate to
  `open.ops.iter().any(|op| op.dst_id() == Some(cow_id))` —
  the writes predicate, not touched (codex round-14).
- Extends close_open_frame with three-branch plumbing:
  (a) PRE-SUBMIT: acquire PresentCompletionSignal and thread its
      semaphore into end_and_submit_op_with_signal — mirrors the
      legacy flush_cow_batch order at engine.rs:3467-3483.
  (b) POST-FLUSH SUCCESS: drain completions into PendingPresentBatch
      with the exported sync_file fd (Fd / Ready / Poll per the
      export result).
  (c) POST-FLUSH FAILURE: force-enqueue a degraded
      PendingPresentBatch { wait: Ready, ticket: None, signal: None,
      events } BEFORE returning Err — X PRESENT protocol observes
      these events regardless of submit success (codex round-14
      MEDIUM #2).

Updates has_pending_batches_for_tests to check frame_builder.open
instead of pending_cow_batch (N10 minor).

Adds v2_frame_builder_cow_copy_area_collapses_two_in_one_frame and
v2_frame_builder_cow_copy_area_delivers_present_completion integration
tests."
```

### Task 5: cow_copy_area integration test (already covered by Task 4)

Task 4's commit already shipped the two cow tests. If a separate task feels appropriate (per the spec's "2 tasks per op" cadence), split Task 4 into Task 4a (the atomic infra change) and Task 4b (test wiring). The plan as written treats them as one commit because the atomicity requirement makes splitting meaningless — the tests can't compile until the infra change lands.

**Decision: Task 5 is folded into Task 4.** Continue at Task 6.

### Task 6: put_image body rewrite (N1 + N2)

**Files:**

- Modify: `crates/yserver/src/kms/v2/engine.rs` — replace `put_image` body (engine.rs:4127-4275). Add `emit_recorded_put_image_into_cb` helper.

- [ ] **Step 1: Write the failing integration test**

```rust
// crates/yserver/tests/v2_acceptance.rs:
#[test]
#[ignore = "requires Vk fixture"]
fn v2_frame_builder_put_image_collapses_two_in_one_frame() {
    common::with_v2_backend(|be| {
        let dst = common::make_pixmap_for_tests(be, 64, 64, 24);
        let pre_flushes = be.telemetry_submit_group_flushes_for_tests();
        let bytes: Vec<u8> = vec![0xff; 64 * 64 * 4];
        be.engine_put_image_for_tests(
            dst, ash::vk::Offset2D::default(),
            ash::vk::Extent2D { width: 32, height: 32 },
            &bytes[..32*32*4], 24,
        ).unwrap();
        be.engine_put_image_for_tests(
            dst, ash::vk::Offset2D { x: 32, y: 0 },
            ash::vk::Extent2D { width: 32, height: 32 },
            &bytes[..32*32*4], 24,
        ).unwrap();
        be.close_open_frame_for_timeout_for_tests().unwrap();
        let post = be.telemetry_submit_group_flushes_for_tests();
        assert_eq!(post.saturating_sub(pre_flushes), 1);
    });
}
```

- [ ] **Step 2: Replace the `put_image` body**

```rust
// crates/yserver/src/kms/v2/engine.rs (replace fn put_image):
pub(crate) fn put_image(
    &mut self,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    target: DrawableId,
    dst_pos: vk::Offset2D,
    src_extent: vk::Extent2D,
    src_bytes: &[u8],
    src_depth: u8,
) -> Result<(), RenderError> {
    // N9: empty-input fast-path.
    if src_extent.width == 0 || src_extent.height == 0 {
        return Ok(());
    }
    if platform.renderer_failed {
        return Err(RenderError::RendererFailed);
    }
    self.flush_render_batch(store, platform)?;

    let Some(inner) = self.inner.as_mut() else {
        return Err(RenderError::NoVk);
    };
    let Some(drawable) = store.get(target) else {
        return Err(RenderError::UnknownDrawable(target));
    };

    // Format gate (preserve legacy at engine.rs:4160-4176).
    let dst_bpp: u32 = match src_depth {
        1 | 8 => 1,
        24 | 32 => 4,
        _ => return Err(RenderError::UnsupportedDepth(src_depth)),
    };
    let expected_format = if dst_bpp == 1 {
        vk::Format::R8_UNORM
    } else {
        vk::Format::B8G8R8A8_UNORM
    };
    if drawable.storage.format != expected_format {
        return Err(RenderError::UnsupportedDepth(src_depth));
    }
    let dst_extent = drawable.storage.extent;
    let dst_image = drawable.storage.image;
    let dst_pre_layout = drawable.storage.current_layout;
    let prior_dst_ticket = drawable.last_render_ticket.clone();

    // Clamp the put rect (preserve legacy at engine.rs:4182-4191).
    let clipped = clamp_put_rect(dst_pos, src_extent, dst_extent);
    let Some((dst_rect, src_origin_in_input)) = clipped else {
        return Ok(());
    };
    let copy_w = dst_rect.extent.width;
    let copy_h = dst_rect.extent.height;
    let staging_size = u64::from(copy_w) * u64::from(copy_h) * u64::from(dst_bpp);
    if staging_size == 0 {
        return Ok(());
    }

    // Allocate the staging buffer BEFORE any open-frame mutation (N8-style
    // ordering — allocation failure leaves the frame untouched).
    let staging = Arc::new(StagingBuffer::new(inner.vk.clone(), staging_size.max(1))?);
    let (sx, sy) = src_origin_in_input;
    unpack_to_staging(
        src_bytes, src_extent,
        sx, sy, copy_w, copy_h, src_depth,
        staging.mapped.as_ptr(),
    )?;

    // Open the frame if not already open.
    if !inner.frame_builder.is_open() {
        let _ = inner;
        let ticket = platform.submit_group_ticket_or_open()?;
        let inner = self.inner.as_mut().expect("inner");
        inner.acquire_generation = inner.acquire_generation.saturating_add(1);
        let frame_generation = inner.acquire_generation;
        inner.frame_builder.open_for_paint(ticket, frame_generation);
    }
    let inner = self.inner.as_mut().expect("inner");
    let frame_ticket = inner.frame_builder.open.as_ref().expect("open").ticket.clone();

    // N2: pin the staging Arc + first_touch dst + ticket-touch.
    let staging_pin_idx = {
        let open = inner.frame_builder.open.as_mut().expect("open");
        open.touched.first_touch(target, prior_dst_ticket);
        open.layouts.first_touch_drawable(target, dst_pre_layout);
        open.pins.pin_staging(Arc::clone(&staging))
    };
    store.touch_render_fence(target, frame_ticket.clone());
    store.damage(target, dst_rect);

    let payload = Box::new(super::frame_builder::RecordedPutImage {
        dst_id: target,
        dst_rect,
        dst_image,
        dst_extent,
        dst_old_layout: dst_pre_layout,
        staging_pin_idx,
    });
    {
        let open = inner.frame_builder.open.as_mut().expect("open");
        open.push_op_and_set_layouts(
            super::frame_builder::RecordedOp::PutImage(payload),
            &[(target, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
        );
    }
    Ok(())
}
```

- [ ] **Step 3: Implement `emit_recorded_put_image_into_cb`**

```rust
// crates/yserver/src/kms/v2/engine.rs (near emit_recorded_copy_area_into_cb):
fn emit_recorded_put_image_into_cb(
    inner: &mut RenderEngineInner,
    cb: vk::CommandBuffer,
    pins: &super::frame_builder::FramePinSet,
    pi: &super::frame_builder::RecordedPutImage,
) -> Result<(), RenderError> {
    let device = &inner.vk.device;
    // N1 put_image pre-barrier (mirror engine.rs:4210 — DST only).
    barrier_to_layout(
        device, cb, pi.dst_image,
        pi.dst_old_layout, vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_WRITE,
    );
    let staging_buffer = pins.staging_buffers[pi.staging_pin_idx.0 as usize].buffer;
    let region = [vk::BufferImageCopy::default()
        .buffer_offset(0)
        .buffer_row_length(0)
        .buffer_image_height(0)
        .image_subresource(vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .layer_count(1))
        .image_offset(vk::Offset3D {
            x: pi.dst_rect.offset.x, y: pi.dst_rect.offset.y, z: 0,
        })
        .image_extent(vk::Extent3D {
            width: pi.dst_rect.extent.width,
            height: pi.dst_rect.extent.height,
            depth: 1,
        })];
    unsafe {
        device.cmd_copy_buffer_to_image(
            cb, staging_buffer, pi.dst_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL, &region,
        );
    }
    // N1 put_image post-barrier (dst → SHADER_READ_ONLY_OPTIMAL).
    barrier_to_layout(
        device, cb, pi.dst_image,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COPY,
        vk::AccessFlags2::TRANSFER_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    Ok(())
}
```

Replace the Task 1 stub:

```rust
Op::PutImage(pi) => emit_recorded_put_image_into_cb(inner, cb, pins, pi),
```

- [ ] **Step 4: Run + commit**

```bash
cargo build -p yserver
cargo test -p yserver --lib
cargo test -p yserver --test v2_acceptance v2_frame_builder_put_image_collapses_two_in_one_frame
```

```bash
git add -u
git commit -m "feat(v2/engine): B.3 Task 6 — port put_image to frame builder

Replaces put_image's direct-submit body with frame-builder-resident
append. Body: empty-input fast-path → renderer_failed → flush_render_batch
→ preflight (format gate, clamp) → staging buffer allocation → open
frame if not open → pin staging via open.pins.pin_staging (N2) →
first_touch + ticket-touch + damage → push_op_and_set_layouts with
(target, SHADER_READ_ONLY_OPTIMAL).

Emit replays via emit_recorded_put_image_into_cb: pre-barrier dst →
TRANSFER_DST_OPTIMAL with the exact mask shape from engine.rs:4210
(N1 — ALL_COMMANDS / SHADER_SAMPLED_READ | COLOR_ATTACHMENT_WRITE
producer), cmd_copy_buffer_to_image, post-barrier dst →
SHADER_READ_ONLY_OPTIMAL."
```

### Task 7: put_image collapse integration test

Already implemented in Task 6 Step 1. Marked complete by Task 6's commit.

---

## Phase 3 — FILL family (6 tasks: 2 per op × 3 ops)

### Task 8: fill_rect + fill_rect_batch body rewrite (N4)

`fill_rect` is a one-liner that delegates to `fill_rect_batch` with N=1 rect. Both rewrites land together — `fill_rect` becomes a delegating shell again, `fill_rect_batch` carries the new frame-builder body.

**Files:**

- Modify: `crates/yserver/src/kms/v2/engine.rs` — `fill_rect` (engine.rs:2244-2257) becomes a one-line delegate; `fill_rect_batch` (engine.rs:2274-2430) gets the new body. Add `emit_recorded_fill_rect_into_cb`.

- [ ] **Step 1: Write the failing integration test**

```rust
// crates/yserver/tests/v2_acceptance.rs:
#[test]
#[ignore = "requires Vk fixture"]
fn v2_frame_builder_fill_rect_batch_collapses_two_in_one_frame() {
    common::with_v2_backend(|be| {
        let dst = common::make_pixmap_for_tests(be, 128, 128, 24);
        let pre = be.telemetry_submit_group_flushes_for_tests();
        let rects1 = [ash::vk::Rect2D { offset: ash::vk::Offset2D::default(),
                                         extent: ash::vk::Extent2D { width: 16, height: 16 } }];
        let rects2 = [ash::vk::Rect2D { offset: ash::vk::Offset2D { x: 32, y: 0 },
                                         extent: ash::vk::Extent2D { width: 16, height: 16 } }];
        be.engine_fill_rect_batch_for_tests(dst, [1.0, 0.0, 0.0, 1.0], &rects1).unwrap();
        be.engine_fill_rect_batch_for_tests(dst, [0.0, 1.0, 0.0, 1.0], &rects2).unwrap();
        be.close_open_frame_for_timeout_for_tests().unwrap();
        let post = be.telemetry_submit_group_flushes_for_tests();
        assert_eq!(post.saturating_sub(pre), 1);
    });
}
```

- [ ] **Step 2: Replace `fill_rect_batch` body**

```rust
// crates/yserver/src/kms/v2/engine.rs (replace fn fill_rect_batch):
pub(crate) fn fill_rect_batch(
    &mut self,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    target: DrawableId,
    color: [f32; 4],
    rects: &[vk::Rect2D],
) -> Result<(), RenderError> {
    if rects.is_empty() {
        return Ok(());
    }
    if platform.renderer_failed {
        return Err(RenderError::RendererFailed);
    }
    self.flush_render_batch(store, platform)?;

    let Some(inner) = self.inner.as_mut() else {
        return Err(RenderError::NoVk);
    };
    let Some(drawable) = store.get(target) else {
        return Err(RenderError::UnknownDrawable(target));
    };
    let extent = drawable.storage.extent;
    let image_view = drawable.storage.image_view;
    let format = drawable.storage.format;
    let dst_pre_layout = drawable.storage.current_layout;
    let prior_dst_ticket = drawable.last_render_ticket.clone();

    // Clamp + drop empties (preserve legacy at engine.rs:2321-2328).
    let clamped: Vec<vk::Rect2D> = rects
        .iter()
        .map(|r| clamp_rect(*r, extent))
        .filter(|r| r.extent.width != 0 && r.extent.height != 0)
        .collect();
    if clamped.is_empty() {
        return Ok(());
    }

    if !inner.frame_builder.is_open() {
        let _ = inner;
        let ticket = platform.submit_group_ticket_or_open()?;
        let inner = self.inner.as_mut().expect("inner");
        inner.acquire_generation = inner.acquire_generation.saturating_add(1);
        let frame_generation = inner.acquire_generation;
        inner.frame_builder.open_for_paint(ticket, frame_generation);
    }
    let inner = self.inner.as_mut().expect("inner");
    let frame_ticket = inner.frame_builder.open.as_ref().expect("open").ticket.clone();

    {
        let open = inner.frame_builder.open.as_mut().expect("open");
        open.touched.first_touch(target, prior_dst_ticket);
        open.layouts.first_touch_drawable(target, dst_pre_layout);
    }
    store.touch_render_fence(target, frame_ticket.clone());
    for r in &clamped {
        store.damage(target, *r);
    }

    let payload = Box::new(super::frame_builder::RecordedFillRect {
        dst_id: target,
        dst_image_view: image_view,
        dst_extent: extent,
        dst_format: format,
        dst_old_layout: dst_pre_layout,
        color,
        rects: clamped,
    });
    {
        let open = inner.frame_builder.open.as_mut().expect("open");
        open.push_op_and_set_layouts(
            super::frame_builder::RecordedOp::FillRect(payload),
            &[(target, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
        );
    }
    Ok(())
}
```

`fill_rect` becomes a simple delegate:

```rust
pub(crate) fn fill_rect(
    &mut self,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    target: DrawableId,
    rect: vk::Rect2D,
    color: [f32; 4],
) -> Result<(), RenderError> {
    self.fill_rect_batch(store, platform, target, color, &[rect])
}
```

- [ ] **Step 3: Implement `emit_recorded_fill_rect_into_cb`**

```rust
fn emit_recorded_fill_rect_into_cb(
    inner: &mut RenderEngineInner,
    cb: vk::CommandBuffer,
    fr: &super::frame_builder::RecordedFillRect,
) -> Result<(), RenderError> {
    let device = &inner.vk.device;
    // FILL pre-barrier (mirror engine.rs:2333-2348 — drain prior compose
    // reads / put_image writes / paint writes on the same image).
    let drawable_image = {
        // The recorded payload doesn't carry vk::Image — we resolve it
        // via the dst_id at emit time? Actually NO — the v2 storage
        // image is stable for the drawable's lifetime, so it's safe to
        // resolve via store.get at emit. But emit takes `pins: &FramePinSet`
        // and NOT `&mut store`. Re-check the dispatch signature.
        //
        // Looking at emit_recorded_op_into_cb at engine.rs:8425 — it takes
        // `store: &mut DrawableStore` so we DO have store access. Resolve
        // via store.get(fr.dst_id).
        return Err(RenderError::NoVk);  // placeholder — actual body below
    };
    // ...
    Ok(())
}
```

Realising the emit signature passes `store: &mut DrawableStore` (see engine.rs:8425-8431) — pull `image` from the store at emit time. Adjust the signature:

```rust
fn emit_recorded_fill_rect_into_cb(
    inner: &mut RenderEngineInner,
    store: &DrawableStore,
    cb: vk::CommandBuffer,
    fr: &super::frame_builder::RecordedFillRect,
) -> Result<(), RenderError> {
    let device = &inner.vk.device;
    let dst_image = store.get(fr.dst_id).ok_or(RenderError::UnknownDrawable(fr.dst_id))?.storage.image;
    barrier_to_layout(
        device, cb, dst_image,
        fr.dst_old_layout, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ
            | vk::AccessFlags2::TRANSFER_WRITE
            | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
    );
    let render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: fr.dst_extent,
    };
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(fr.dst_image_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::LOAD)  // LOAD-BEARING — N4 / FILL pseudocode
        .store_op(vk::AttachmentStoreOp::STORE)];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(render_area)
        .layer_count(1)
        .color_attachments(&color_attachment);
    let attachments = [vk::ClearAttachment::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .color_attachment(0)
        .clear_value(vk::ClearValue {
            color: vk::ClearColorValue { float32: fr.color },
        })];
    let clear_rects: Vec<vk::ClearRect> = fr.rects
        .iter()
        .map(|r| vk::ClearRect::default()
            .rect(*r)
            .base_array_layer(0)
            .layer_count(1))
        .collect();
    unsafe {
        device.cmd_begin_rendering(cb, &rendering_info);
        let viewport = [vk::Viewport {
            x: 0.0, y: 0.0,
            #[allow(clippy::cast_precision_loss)]
            width: fr.dst_extent.width as f32,
            #[allow(clippy::cast_precision_loss)]
            height: fr.dst_extent.height as f32,
            min_depth: 0.0, max_depth: 1.0,
        }];
        device.cmd_set_viewport(cb, 0, &viewport);
        let scissor = [render_area];
        device.cmd_set_scissor(cb, 0, &scissor);
        device.cmd_clear_attachments(cb, &attachments, &clear_rects);
        device.cmd_end_rendering(cb);
    }
    barrier_to_layout(
        device, cb, dst_image,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    Ok(())
}
```

Dispatch arm:

```rust
Op::FillRect(fr) => emit_recorded_fill_rect_into_cb(inner, store, cb, fr),
```

Note: `emit_recorded_op_into_cb` already has `store: &mut DrawableStore` in scope; we pass `&*store` (immutable view) or `store` (mutable — fine, the function takes `&DrawableStore`). Confirm at write-time.

- [ ] **Step 4: Run + commit**

```bash
cargo test -p yserver --lib
cargo test -p yserver --test v2_acceptance v2_frame_builder_fill_rect_batch_collapses_two_in_one_frame
```

```bash
git add -u
git commit -m "feat(v2/engine): B.3 Task 8 — port fill_rect + fill_rect_batch to frame builder

fill_rect_batch records ONE RecordedFillRect per call carrying the
entire rect slice (N4 — splitting per-rect would be new behavior, not
preservation). fill_rect is a one-line delegate to fill_rect_batch
with [rect].

Body order per N9: empty-input fast-path → renderer_failed →
flush_render_batch → preflight (clamp+filter) → open frame if not
open → first_touch + ticket-touch + damage → push_op_and_set_layouts
with (target, SHADER_READ_ONLY_OPTIMAL).

Emit replays via emit_recorded_fill_rect_into_cb using
cmd_clear_attachments directly — NO composite pipeline, NO descriptor.
load_op=LOAD is LOAD-BEARING (N4) — outside-rect pixels must be
preserved. Pre-barrier producer mask mirrors engine.rs:2333-2348
verbatim (drain prior compose reads / put_image writes)."
```

### Task 9: fill_rect_batch collapse integration test

Folded into Task 8.

### Task 10: logic_fill body rewrite (N6)

**Files:**

- Modify: `crates/yserver/src/kms/v2/engine.rs` — replace `logic_fill` body (engine.rs:2489-2718). Add `emit_recorded_logic_fill_into_cb`.

- [ ] **Step 1: Write the failing integration test**

```rust
// crates/yserver/tests/v2_acceptance.rs:
#[test]
#[ignore = "requires Vk fixture"]
fn v2_frame_builder_logic_fill_collapses_two_in_one_frame() {
    common::with_v2_backend(|be| {
        let dst = common::make_pixmap_for_tests(be, 64, 64, 24);
        let pre = be.telemetry_submit_group_flushes_for_tests();
        let rects = [yserver::kms::cpu_types::Rectangle16 {
            x: 0, y: 0, width: 16, height: 16,
        }];
        be.engine_logic_fill_for_tests(dst, yserver_core::backend::GcFunction::Xor,
            /* opaque_alpha */ true, 0xFF00FF, &rects).unwrap();
        be.engine_logic_fill_for_tests(dst, yserver_core::backend::GcFunction::And,
            true, 0x00FF00, &rects).unwrap();
        be.close_open_frame_for_timeout_for_tests().unwrap();
        let post = be.telemetry_submit_group_flushes_for_tests();
        assert_eq!(post.saturating_sub(pre), 1);
    });
}
```

- [ ] **Step 2: Replace the `logic_fill` body**

```rust
// crates/yserver/src/kms/v2/engine.rs (replace fn logic_fill):
pub(crate) fn logic_fill(
    &mut self,
    store: &mut DrawableStore,
    platform: &mut PlatformBackend,
    target: DrawableId,
    function: yserver_core::backend::GcFunction,
    opaque_alpha: bool,
    fg: u32,
    rects: &[Rectangle16],
) -> Result<(), RenderError> {
    use yserver_core::backend::GcFunction;
    if rects.is_empty() {
        return Ok(());
    }
    if matches!(function, GcFunction::NoOp) {
        return Ok(());
    }
    if platform.renderer_failed {
        return Err(RenderError::RendererFailed);
    }
    self.flush_render_batch(store, platform)?;

    // Ensure the logic-fill pipeline cache exists for the dst format.
    let format = {
        let d = store.get(target).ok_or(RenderError::UnknownDrawable(target))?;
        d.storage.format
    };
    self.ensure_logic_fill_cache(platform, format)?;

    let Some(inner) = self.inner.as_mut() else {
        return Err(RenderError::NoVk);
    };
    let Some(drawable) = store.get(target) else {
        return Err(RenderError::UnknownDrawable(target));
    };
    let extent = drawable.storage.extent;
    let image_view = drawable.storage.image_view;
    let dst_pre_layout = drawable.storage.current_layout;
    let prior_dst_ticket = drawable.last_render_ticket.clone();

    // Unpack the X11 wire pixel (preserve legacy at engine.rs:2546-2560).
    let color = if format == vk::Format::R8_UNORM {
        [(fg & 0xFF) as f32 / 255.0, 0.0, 0.0, 1.0]
    } else {
        [
            ((fg >> 16) & 0xFF) as f32 / 255.0,
            ((fg >> 8) & 0xFF) as f32 / 255.0,
            (fg & 0xFF) as f32 / 255.0,
            1.0,
        ]
    };

    // Clamp + drop empties (preserve legacy at engine.rs:2562-2588).
    let vk_rects: Vec<vk::Rect2D> = rects
        .iter()
        .filter_map(|r| {
            let x0 = i32::from(r.x).max(0);
            let y0 = i32::from(r.y).max(0);
            let x1 = (i32::from(r.x).saturating_add(i32::from(r.width)))
                .min(i32::try_from(extent.width).unwrap_or(i32::MAX));
            let y1 = (i32::from(r.y).saturating_add(i32::from(r.height)))
                .min(i32::try_from(extent.height).unwrap_or(i32::MAX));
            if x1 <= x0 || y1 <= y0 { return None; }
            Some(vk::Rect2D {
                offset: vk::Offset2D { x: x0, y: y0 },
                extent: vk::Extent2D {
                    width: (x1 - x0) as u32,
                    height: (y1 - y0) as u32,
                },
            })
        })
        .collect();
    if vk_rects.is_empty() {
        return Ok(());
    }

    if !inner.frame_builder.is_open() {
        let _ = inner;
        let ticket = platform.submit_group_ticket_or_open()?;
        let inner = self.inner.as_mut().expect("inner");
        inner.acquire_generation = inner.acquire_generation.saturating_add(1);
        let frame_generation = inner.acquire_generation;
        inner.frame_builder.open_for_paint(ticket, frame_generation);
    }
    let inner = self.inner.as_mut().expect("inner");
    let frame_ticket = inner.frame_builder.open.as_ref().expect("open").ticket.clone();

    {
        let open = inner.frame_builder.open.as_mut().expect("open");
        open.touched.first_touch(target, prior_dst_ticket);
        open.layouts.first_touch_drawable(target, dst_pre_layout);
    }
    store.touch_render_fence(target, frame_ticket.clone());
    for r in &vk_rects {
        store.damage(target, *r);
    }

    let payload = Box::new(super::frame_builder::RecordedLogicFill {
        dst_id: target,
        dst_image_view: image_view,
        dst_extent: extent,
        dst_format: format,
        dst_old_layout: dst_pre_layout,
        logic_mode: function,
        opaque_alpha,
        color,
        rects: vk_rects,
    });
    {
        let open = inner.frame_builder.open.as_mut().expect("open");
        open.push_op_and_set_layouts(
            super::frame_builder::RecordedOp::LogicFill(payload),
            &[(target, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)],
        );
    }
    Ok(())
}
```

- [ ] **Step 3: Implement `emit_recorded_logic_fill_into_cb`**

Mirror engine.rs:2593-2697 (the legacy logic_fill emit body); the only changes are reading from the recorded payload + relooking up the pipeline at emit time via `inner.logic_fill_caches[dst_format].get(logic_mode, opaque_alpha)`. Code shape:

```rust
fn emit_recorded_logic_fill_into_cb(
    inner: &mut RenderEngineInner,
    store: &DrawableStore,
    cb: vk::CommandBuffer,
    lf: &super::frame_builder::RecordedLogicFill,
) -> Result<(), RenderError> {
    use crate::kms::vk::logic_fill_pipeline::LogicFillPushConsts;
    let device = &inner.vk.device;
    let dst_image = store.get(lf.dst_id).ok_or(RenderError::UnknownDrawable(lf.dst_id))?.storage.image;
    let cache = inner.logic_fill_caches
        .get_mut(&lf.dst_format)
        .ok_or(RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED))?;
    let pipeline = cache.get(lf.logic_mode, lf.opaque_alpha).map_err(|_| {
        RenderError::Vk(vk::Result::ERROR_INITIALIZATION_FAILED)
    })?;
    let pipeline_layout = cache.pipeline_layout();

    // N6 pre-barrier (drain prior writes — exact shape from engine.rs:2593-2618).
    barrier_to_layout(
        device, cb, dst_image,
        lf.dst_old_layout, vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
        vk::PipelineStageFlags2::ALL_COMMANDS,
        vk::AccessFlags2::SHADER_SAMPLED_READ
            | vk::AccessFlags2::TRANSFER_WRITE
            | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
    );

    let render_area = vk::Rect2D {
        offset: vk::Offset2D::default(),
        extent: lf.dst_extent,
    };
    let color_attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(lf.dst_image_view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)];
    let rendering_info = vk::RenderingInfo::default()
        .render_area(render_area)
        .layer_count(1)
        .color_attachments(&color_attachment);
    #[allow(clippy::cast_precision_loss)]
    let viewport = [vk::Viewport {
        x: 0.0, y: 0.0,
        width: lf.dst_extent.width as f32,
        height: lf.dst_extent.height as f32,
        min_depth: 0.0, max_depth: 1.0,
    }];
    #[allow(clippy::cast_precision_loss)]
    let dst_vp = [lf.dst_extent.width as f32, lf.dst_extent.height as f32];
    unsafe {
        device.cmd_begin_rendering(cb, &rendering_info);
        device.cmd_set_viewport(cb, 0, &viewport);
        device.cmd_bind_pipeline(cb, vk::PipelineBindPoint::GRAPHICS, pipeline);
        for r in &lf.rects {
            let scissor = [*r];
            device.cmd_set_scissor(cb, 0, &scissor);
            #[allow(clippy::cast_precision_loss)]
            let pc = LogicFillPushConsts {
                dst_origin: [r.offset.x as f32, r.offset.y as f32],
                dst_size: [r.extent.width as f32, r.extent.height as f32],
                viewport: dst_vp,
                _pad: [0.0, 0.0],
                fg_color: lf.color,
            };
            device.cmd_push_constants(
                cb, pipeline_layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0, pc.as_bytes(),
            );
            device.cmd_draw(cb, 4, 1, 0, 0);
        }
        device.cmd_end_rendering(cb);
    }
    barrier_to_layout(
        device, cb, dst_image,
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
        vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_SAMPLED_READ,
    );
    Ok(())
}
```

Dispatch arm:

```rust
Op::LogicFill(lf) => emit_recorded_logic_fill_into_cb(inner, store, cb, lf),
```

- [ ] **Step 4: Run + commit**

```bash
cargo test -p yserver --lib
cargo test -p yserver --test v2_acceptance v2_frame_builder_logic_fill_collapses_two_in_one_frame
```

```bash
git add -u
git commit -m "feat(v2/engine): B.3 Task 10 — port logic_fill to frame builder

logic_fill records RecordedOp::LogicFill carrying GcFunction + opaque_alpha
(caller-provided GC state, NOT derived per N6 / codex round-8) +
dst_format (cache key) + pre-clamped rect slice. Emit re-resolves the
LogicFillPipelineCache pipeline fresh via
inner.logic_fill_caches[dst_format].get(logic_mode, opaque_alpha).

Body order matches the rest of the FILL family per N9. Emit barriers
mirror engine.rs:2593-2697 verbatim — drain prior compose / put_image /
paint writes via the ALL_COMMANDS producer mask."
```

### Task 11: logic_fill collapse integration test

Folded into Task 10.

---

## Phase 4 — MASK family (2 tasks: render_traps_or_tris only)

### Task 12: render_traps_or_tris body rewrite (N5)

The heaviest single body rewrite. Full payload field listing comes from spec § N5.

**Files:**

- Modify: `crates/yserver/src/kms/v2/engine.rs` — replace `render_traps_or_tris` body (engine.rs:7183-7782). Add `emit_recorded_render_traps_or_tris_into_cb`.

- [ ] **Step 1: Body rewrite shape**

Order per N5 + N9 + Phase 9A:

```
0. empty-input fast-path (instance_count == 0, bbox_w/h == 0) → return Ok.
1. renderer_failed check.
2. flush_render_batch.
3. ensure_render_assets + ensure_trap_assets (preserve legacy).
4. preflight: store.get(dst) metadata, std_op from op byte, dst_has_alpha,
   self-alias gate (preserve legacy at engine.rs:7271-7274 — return Ok on
   gap, the trap path doesn't route through alias scratch).
5. Allocate vertex StagingBuffer (HOST_VISIBLE + VERTEX_BUFFER usage) +
   copy instance_data into mapped (preserve engine.rs:7280-7292). Allocation
   FIRST per N8-style ordering.
6. Phase 9A: peek + close-before-grow + grow + adopt for mask_scratch.
   (Mirror the B.2 render_composite logic but for mask_scratch via
   ensure_image_size_returning_old.)
7. Phase 9A: peek + close-before-grow + grow + adopt for dst_readback
   when needs_dst_readback. (Same pattern as B.2 render_composite.)
8. Resolve append-time-stable fields per N5:
   - src_kind: match `src` (ResolvedSource::Drawable/Solid/Gradient/None).
     For Drawable: snapshot pict_format-aware swizzle_class.
     For Solid: snapshot color.
     For Gradient: snapshot intrinsic_axis_projection from
       inner.picture_paint[xid] (CPU-immutable per B.2 R3).
     For None: return Ok (gap, preserve legacy at engine.rs:7397-7400).
   - src_repeat: pre-resolve via repeat_to_shader_const(src_repeat).
   - src_force_opaque: via resolve_force_opaque_pict_format.
   - user_src_xform: via pixman_transform_to_affine.
   - clip_scissors: pre-clamp from clip_rects against dst_extent (return
     Ok if empty after clamp — preserve engine.rs:7484-7525).
9. Open frame if not open. Pin the vertex StagingBuffer via
   open.pins.pin_staging — returns vertex_pool_pin: PinnedStagingIdx.
10. Prelude state mutations FOR ALL TOUCHED DRAWABLES (N5 step 6):
    - dst: first_touch + first_touch_drawable + touch_render_fence.
    - src (only when Drawable): SAME three on src DrawableId.
    - Solid / Gradient / None: no src-side mutation needed.
11. push_op_and_set_layouts(RenderTrapsOrTris(payload), layouts_to_set)
    where layouts_to_set includes (dst, SHADER_READ_ONLY_OPTIMAL) always,
    AND (src, SHADER_READ_ONLY_OPTIMAL) when src is Drawable.
```

Code is too long to inline here. The implementer reads spec § "MASK family" recording shape, spec § N5 payload, and the legacy body at engine.rs:7183-7782 — and translates to the frame-builder shape. The TWO load-bearing extras beyond a mechanical port:

- The Phase 9A close-before-grow check matches B.2 render_composite's at engine.rs:6539-6557.
- The src-side prelude touch is REQUIRED (codex round-9 CRITICAL catch) — the pre-B.3 body only touched the source fence at submit (engine.rs:7752); without the new first_touch + touch_render_fence at APPEND, the source can be freed while the open frame intends to sample it at close.

- [ ] **Step 2: Emit body**

The emit replays per N5 (raster phase + composite phase). Shape:

```rust
fn emit_recorded_render_traps_or_tris_into_cb(
    inner: &mut RenderEngineInner,
    store: &mut DrawableStore,
    cb: vk::CommandBuffer,
    pins: &super::frame_builder::FramePinSet,
    rt: &super::frame_builder::RecordedRenderTrapsOrTris,
) -> Result<(), RenderError> {
    use crate::kms::vk::{
        ops::render as vk_render,
        render_pipeline::{StdPictOp, record_solid_color_clear},
        trap_pipeline::TrapDrawPushConsts,
    };

    // (a) Resolve src view + extent FRESH from engine caches at emit time.
    // (b) Resolve mask_view + mask_attachment_view + mask_extent + current
    //     layout FRESH from inner.mask_scratch.
    // (c) Resolve dst_readback view FRESH when std_op.needs_dst_readback().
    // (d) Record the trap raster phase (mirror engine.rs:7531-7647).
    // (e) Record the composite phase (mirror engine.rs:7665-7735).
    // (f) Post-emit CPU writeback: mask_scratch.set_current_layout(
    //       SHADER_READ_ONLY_OPTIMAL) per N5 LOAD-BEARING (codex round-10).

    // ... (full body — port engine.rs:7531-7749 verbatim with reads from
    //      the recorded payload instead of locals)
    Ok(())
}
```

The vertex buffer comes from `pins.staging_buffers[rt.vertex_pool_pin.0 as usize].buffer`.

Dispatch arm:

```rust
Op::RenderTrapsOrTris(rt) => emit_recorded_render_traps_or_tris_into_cb(inner, store, cb, pins, rt),
```

- [ ] **Step 3: Write the failing tests**

Spec § N5 mandates TWO tests in addition to the basic collapse-test:

1. **Cross-frame mask scratch grow** — 3-op sequence `(small, large, large)`. The grow inherently triggers close-before-grow between op 1 and op 2 → F1 closes, F2 reopens against the new mask_scratch. Asserts `telemetry_submit_group_flushes` delta = 2 and `frame_builder_close_reason_scratch_grow` lifetime counter delta = 1.

2. **Solid-source equivalence** — a Solid-src trap op replays with `record_solid_color_clear` writing the correct color before composite. Catches the "stale 1×1 solid contents" replay bug codex round-7 flagged.

The `frame_builder_close_reason_scratch_grow` accessor exists per Task 1 from B.2 (telemetry.rs:117). Add a `KmsBackendV2`-level pass-through `telemetry_close_reason_scratch_grow_for_tests` if not already present.

- [ ] **Step 4: Run + commit**

```bash
cargo test -p yserver --lib
cargo test -p yserver --test v2_acceptance v2_frame_builder_render_traps_or_tris_
```

```bash
git add -u
git commit -m "feat(v2/engine): B.3 Task 12 — port render_traps_or_tris to frame builder (N5)

render_traps_or_tris records ONE RecordedRenderTrapsOrTris covering
both raster + composite stages. Payload carries the full append-time-
stable input set per spec § N5: dst identity + dst_old_layout,
std_op + raw op_byte (separate per needs_full_dst byte-pattern test),
src_kind discriminant with per-kind snapshot (Drawable swizzle_class /
Solid color / Gradient intrinsic_axis_projection), CompositeAttrs
inputs (src_repeat resolved at append, force_opaque, user_src_xform),
prim_kind + bbox + instance_count, pre-clamped clip_scissors,
vertex_pool_pin: PinnedStagingIdx.

Append-side: vertex buffer allocation FIRST, Phase 9A peek-before-grow
for mask_scratch + dst_readback with adopt_retired_resource_for_gpu_
retirement routing (B.2 pattern), src-side prelude touch
(first_touch + touch_render_fence on the source Drawable per codex
round-9 CRITICAL catch — the pre-B.3 body only touched src at submit).

Emit re-resolves mask_scratch / dst_readback / composite pipeline /
descriptor set FRESH from engine state — none are recorded. Trap
raster phase + composite phase mirror engine.rs:7531-7735 verbatim,
with the post-emit CPU writeback (mask_scratch.set_current_layout(
SHADER_READ_ONLY_OPTIMAL) per N5 codex round-10 — without this the
NEXT trap op's pre-barrier reads stale old_layout)."
```

### Task 13: render_traps_or_tris collapse + cross-frame mask grow + solid equivalence tests

Folded into Task 12.

---

## Phase 5 — GLYPH family (2 tasks: image_text only)

### Task 14: image_text body rewrite (N7)

**Files:**

- Modify: `crates/yserver/src/kms/v2/engine.rs` — replace `image_text` body (engine.rs:4486-4720+). Add `emit_recorded_image_text_into_cb`.

- [ ] **Step 1: Body rewrite shape**

Mirror B.1's `composite_glyphs_via_frame_builder` (engine.rs:5191-5618+) for structure. The differences:

- The target-format gate (N7 LOAD-BEARING) fires BEFORE any atlas first-touch / glyph upload / op append.
- The draw-side emit calls into the text-pipeline single-run draw (mirror engine.rs:4722-4732's `record_text_run`) instead of the composite-glyphs scissored draw.
- The recorded payload is `RecordedImageText` (no `clip_scissors` field — image_text doesn't carry an X RENDER picture clip).
- The per-glyph uploads use the existing B.1 `RecordedOp::GlyphUpload` variant verbatim (N7 — reused, not redefined).

Body order:

```
0. empty-input fast-path (rendered.is_empty()) → return Ok(stats).
1. renderer_failed check.
2. flush_render_batch.
3. store.get(target) preflight + target_format gate (N7 LOAD-BEARING):
   if target_format != B8G8R8A8_UNORM { return Ok(stats); }
4. Lazy-init V2GlyphAtlas + TextPipeline (preserve engine.rs:4531-4553).
5. Open frame if not open + capture frame_generation.
6. first_touch dst + ticket-touch dst + first_touch_atlas snapshot
   (N7 atlas transactional discipline — mirror engine.rs:5301-5333).
7. For each glyph: lookup → on miss, pack + allocate staging buffer
   + pin via open.pins.pin_staging + push RecordedOp::GlyphUpload via
   open.ops.push (NOT push_op_and_set_layouts — GlyphUpload's
   dst_id() is None per Task 1, no layout updates).
8. Push RecordedOp::ImageText via push_op_and_set_layouts with
   (target, SHADER_READ_ONLY_OPTIMAL).
9. Return Ok(stats).
```

The per-glyph code is mechanical (mirror engine.rs:4562-4700). The `record_upload` line at engine.rs:4644-4648 (which directly records an upload CB) is REPLACED with a `RecordedOp::GlyphUpload` append — the upload becomes deferred to emit time.

- [ ] **Step 2: Emit `Op::ImageText` arm**

```rust
fn emit_recorded_image_text_into_cb(
    inner: &mut RenderEngineInner,
    store: &mut DrawableStore,
    cb: vk::CommandBuffer,
    it: &super::frame_builder::RecordedImageText,
) -> Result<(), RenderError> {
    let atlas_extent = inner.glyph_atlas.as_ref().ok_or(RenderError::NoVk)?.extent();
    let vk = inner.vk.clone();
    let drawable = store.get_mut(it.dst_id).ok_or(RenderError::UnknownDrawable(it.dst_id))?;
    let glyphs_view: Vec<crate::kms::vk::ops::text::TextGlyph> = it.glyphs
        .iter()
        .map(|g| crate::kms::vk::ops::text::TextGlyph {
            entry: super::glyph_atlas::AtlasEntry {
                atlas_x: g.atlas_x, atlas_y: g.atlas_y,
                w: g.w, h: g.h,
                pen_left: 0, pen_top: 0,
            },
            dst_x: g.dst_x, dst_y: g.dst_y,
        })
        .collect();
    let mut adapter = StorageTextTarget {
        extent: drawable.storage.extent,
        image: drawable.storage.image,
        image_view: drawable.storage.image_view,
        current_layout: drawable.storage.current_layout,
    };
    let pipeline = inner.text_pipeline.as_ref().ok_or(RenderError::NoVk)?;
    // image_text uses the single-run record_text_run (no clip scissors),
    // distinct from composite_glyphs's record_text_run_scissored.
    crate::kms::vk::ops::text::record_text_run(
        &vk, cb, &mut adapter,
        crate::kms::vk::ops::text::TextAtlas { extent: atlas_extent },
        pipeline,
        &glyphs_view,
        it.foreground_rgba,
    )?;
    drawable.storage.current_layout = adapter.current_layout;
    Ok(())
}
```

Dispatch arm:

```rust
Op::ImageText(it) => emit_recorded_image_text_into_cb(inner, store, cb, it),
```

- [ ] **Step 3: Write the integration tests (N7 acceptance gate)**

Spec § "Implementation gates" Phase 5 requires four assertions:

```rust
#[test]
#[ignore = "requires Vk fixture"]
fn v2_frame_builder_image_text_collapses_two_in_one_frame() { /* two-call collapse */ }

#[test]
#[ignore = "requires Vk fixture"]
fn v2_frame_builder_image_text_close_failure_rolls_back_atlas() {
    // Use platform_force_next_submit_failure_for_tests to force close
    // failure AFTER an image_text frame. Assert atlas's last_render_ticket
    // rolls back to its pre-frame value.
}

#[test]
#[ignore = "requires Vk fixture"]
fn v2_frame_builder_image_text_non_bgra8_target_drops_run() {
    // Create a depth-1 (R8_UNORM) target. Call image_text. Assert:
    //   - stats.glyphs_dropped == 0 (the run is dropped wholesale before
    //     any glyph processing — there are NO dropped glyphs because
    //     there were no processed glyphs).
    //   - Atlas last_render_ticket UNCHANGED.
    //   - No staging buffer pinned in the open frame (assert pin count
    //     pre/post equal).
}

#[test]
#[ignore = "requires Vk fixture"]
fn v2_frame_builder_image_text_delivers_present_completion() {
    // Open frame with image_text op, attach synthetic PRESENT completion
    // to dst (treating dst as a COW for the test), close frame, verify
    // CompletedPresentEvent delivered (mirrors Task 4 PRESENT test but
    // for an image_text frame).
}
```

- [ ] **Step 4: Run + commit**

```bash
cargo test -p yserver --lib
cargo test -p yserver --test v2_acceptance v2_frame_builder_image_text_
```

```bash
git add -u
git commit -m "feat(v2/engine): B.3 Task 14 — port image_text to frame builder (N7)

image_text records RecordedOp::ImageText for the draw side + reuses
B.1's RecordedOp::GlyphUpload variant verbatim for per-glyph uploads
(N7 — composite_glyphs's existing pattern). Body order mirrors
composite_glyphs_via_frame_builder (engine.rs:5191-5618+).

Target-format gate (N7 LOAD-BEARING — codex round-7 catch):
target_format != B8G8R8A8_UNORM → early-return Ok(stats) BEFORE atlas
first-touch / glyph upload / op append. No rollback path needed
because nothing was recorded. Depth-24 and depth-32 ARGB both map to
BGRA8_UNORM and PASS the gate (codex round-10 wording correction).

Atlas transactional discipline (N7): first_touch_atlas snapshot at the
first atlas-touching op-append (mirror engine.rs:5318-5333), the
existing commit_close_success + rollback_atlas paths handle the rest.
Atlas pin-ceiling logic from B.1 applies — no new counter.

Emit calls record_text_run (single-run, no clip scissors — distinct
from composite_glyphs's record_text_run_scissored)."
```

### Task 15: image_text integration tests (already covered by Task 14)

Folded into Task 14.

---

## Phase 6 — Wrap-up (2 tasks)

### Task 16: cargo +nightly fmt + plain clippy

**Files:**

- Modify: any file flagged by clippy.

- [ ] **Step 1: Format**

```bash
cd /home/jos/Projects/yserver
cargo +nightly fmt
git status  # confirm fmt produced clean state
```

- [ ] **Step 2: Plain clippy (NOT pedantic per AGENTS.md)**

```bash
cargo clippy --workspace --all-targets 2>&1 | tee /tmp/clippy-b3.log
```

Fix every warning the B.3 surface introduced (CopyArea / PutImage / FillRect / LogicFill / ImageText / RenderTrapsOrTris payloads + their emit fns + the body rewrites). Common cases:

- `clippy::needless_borrow` on `Arc::clone(&staging)` — fine, that's the intentional shape.
- `clippy::cast_precision_loss` on `extent.width as f32` — add `#[allow(clippy::cast_precision_loss)]` if not already (mirror engine.rs's pattern).
- `clippy::too_many_arguments` on emit helpers — add `#[allow(clippy::too_many_arguments)]`.
- `clippy::redundant_field_names` — fix mechanically.

Don't chase warnings outside the B.3 surface (the ~3000 pre-existing pedantic warnings per project memory `feedback_clippy_pedantic_default`).

- [ ] **Step 3: Commit**

```bash
git add -u
git commit -m "style(v2): B.3 Task 16 — cargo +nightly fmt + plain clippy

Format + fix every plain clippy warning introduced by the B.3 surface
(CopyArea / PutImage / FillRect / LogicFill / ImageText /
RenderTrapsOrTris payloads, their emit helpers, and the rewritten op
bodies). Pedantic warnings are NOT addressed per AGENTS.md + memory
feedback_clippy_pedantic_default."
```

### Task 17: docs/status.md update + bee hardware-gate placeholder

**Files:**

- Modify: `docs/status.md` — add Phase B.3 entry.

- [ ] **Step 1: Append Phase B.3 entry**

Match the B.1 / B.2 entry shape (search `grep -nE 'Phase B sub-phase B' docs/status.md` for the template).

```markdown
### Phase B sub-phase B.3 — IMPLEMENTED 2026-MM-DD

All 8 remaining non-ported paint ops (`copy_area`, `cow_copy_area`,
`put_image`, `fill_rect`, `fill_rect_batch`, `logic_fill`, `image_text`,
`render_traps_or_tris`) are now FrameBuilder-resident. M2's only
remaining call site is `render_composite_legacy` (engine.rs:5877)
behind the `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE` kill-switch — with
the switch ON (default), zero non_ported close events fire in steady
state.

**Mechanism changes:**
- `SubmittedOp::scratch` is `Vec<ScratchImage>` (was `Option<ScratchImage>`).
  Single self-overlap scratch lives on `RecordedCopyArea` from append
  until the close-path walk migrates it (N8).
- `pending_cow_batch` and `flush_cow_batch` infrastructure DELETED.
  X PRESENT completions for COW frames now migrate to
  `OpenFrame::pending_present_completions`, acquired via
  `PresentCompletionSignal` BEFORE the submit (N10 — never silent-drop;
  flush-failure force-enqueues a degraded `PendingPresentBatch`).

**Bee hardware-smoke gate (NOT YET — pending capture):**
- `close_reasons[non_ported]/s` → ≤ 10 (vs ~900–1100 pre-B.3).
- `submit_group_flushes/s` drop by 30–50 % beyond B.2's ~75 % absorption.
  Combined with B.2: target ~200–400 submits/s on bee MATE drag.
- `ops/frame_avg` rises from B.2's ~1.7 to ~4–8.
- `frame_builder_aborts/s = 0`.
- silence (dual-output) regression check — no scene-compose regression,
  no ERROR_DEVICE_LOST, no fault chains.
- yoga / iMac / fuji regression checks.
- Cross-vendor sanity — same MATE drag on non-radv (nvidia, intel,
  lavapipe) — no new validation VUIDs.
```

- [ ] **Step 2: Commit**

```bash
git add -u
git commit -m "docs(status): Phase B.3 implementation entry + bee hardware-gate placeholder

All 8 remaining non-ported paint ops are FrameBuilder-resident.
Pending bee MATE hardware capture to confirm submit_group_flushes/s
drops from B.2's ~75% absorption toward the spec's 200-400/s end-of-B.4
band."
```

---

## Acceptance gates

### Implementation gates (per-task, validated during plan execution)

- `cargo build -p yserver` clean for each task.
- `cargo test -p yserver --lib` green for each task.
- `cargo +nightly fmt --check` clean (Task 16).
- `cargo clippy --workspace --all-targets` (plain, NOT pedantic) clean for the B.3 surface (Task 16).
- Each integration test in Phase 2/3/4 demonstrates two-op collapse-to-one-submit for its op (Tasks 3, 5, 7, 9, 11, 13).
- Phase 5 image_text integration test demonstrates: (a) two-call collapse; (b) atlas first-touch snapshot + close-failure rollback restores atlas `last_render_ticket`; (c) non-BGRA8 target negative-test drops the run without atlas mutation; (d) PRESENT-completion delivery on a frame containing image_text (N10 atomic test).
- Phase 4 MASK integration test demonstrates: cross-frame mask scratch grow (3-op `(small, large, large)` sequence with `telemetry_submit_group_flushes` delta = 2 and `frame_builder_close_reason_scratch_grow` lifetime counter delta = 1) AND solid-source equivalence (Solid-src trap op replays with `record_solid_color_clear` writing the correct color before composite).

### Hardware gates (user-driven, after Task 17)

User drives `*-hw` recipes per project memory `feedback_hw_recipes_user_only`. Targets:

- **bee MATE-load** after all 8 ports land:
  - `close_reasons[non_ported]/s` → ≤ 10.
  - `submit_group_flushes/s` drop by 30–50 % beyond B.2's ~75 %.
  - `ops/frame_avg` rises from B.2's ~1.7 to ~4–8.
  - `frame_builder_aborts/s = 0`.
- **silence (dual-output)** regression check.
- **yoga / iMac / fuji** regression checks.
- **Cross-vendor sanity** (nvidia / intel / lavapipe) — no new validation VUIDs.

---

## Risk register (inherits from spec § "Risk register")

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Validation VUIDs in MASK family emit (mask scratch + composite cross-barrier) | Medium | High (silent corruption) | Bee vkdebug pass after MASK port lands, before merging. |
| cow_copy_area rewrite wires cow_id to wrong DrawableStore entry | Low | High (corruption — wrong drawable's storage written) | Resolved at spec-time per N3 (cow is a regular Drawable in store). Task 4's `v2_frame_builder_cow_copy_area_collapses_two_in_one_frame` test verifies. |
| put_image staging-buffer Arc not pinned to the right fence (UAF) | Low | High (GPU fault) | B.1's pin-set mechanism is proven; mirror exactly. Task 6 test pattern can extend to a concurrent-destroy stress test. |
| Rip-and-replace makes regressions harder to bisect | Medium | Medium | Codex review depth (B.3 went through 17 review rounds); per-op integration tests; git revert remains the bisect tool of last resort. |
| `fill_rect_batch` implementer splits per-rect instead of preserving per-call | Low | Medium | Decision final at spec time per N4. Task 8 records ONE `RecordedFillRect` per call carrying the entire rect slice. |
| v1 backend breaks beyond compile fix | Low | Low (v1 scheduled for removal) | Per project policy (memory `feedback_v2_rip_and_replace_porting`), apply only minimal compile fix; do not delay B.3 to investigate v1 runtime paths. |
| `RecordedRenderTrapsOrTris` exceeds 512B size budget (Open Question 1) | Medium | Low (payload-layout detail) | If hit, Box-wrap `clip_scissors` or `RecordedTrapSrcKind`. Not a structural redesign — the single-op decision is final per N5. |

---

## References

- Phase B.3 spec: `docs/superpowers/specs/2026-05-25-frame-builder-phase-b3-design.md`.
- Phase B.2 plan: `docs/superpowers/plans/2026-05-24-frame-builder-phase-b2.md`.
- Phase A spec: `docs/superpowers/specs/2026-05-23-frame-builder-submit-rate-design.md`.
- AGENTS.md (project conventions).
- Memory: `feedback_v2_rip_and_replace_porting`, `feedback_clippy_pedantic_default`, `feedback_hw_recipes_user_only`, `feedback_write_failing_test_first`.
