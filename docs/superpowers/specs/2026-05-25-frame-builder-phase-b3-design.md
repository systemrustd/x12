# Phase B.3 — Eliminate M2: Port all remaining non-ported paint ops into the FrameBuilder

**Date:** 2026-05-25
**Status:** Design — awaiting user review then implementation plan
**Predecessors:** Phase B.2 (`2026-05-24-frame-builder-phase-b-design.md`, plan `2026-05-24-frame-builder-phase-b2.md`)

## Goal

After Phase B.2, the frame builder absorbs `render_composite` and `render_fill_rectangles` (the latter delegates to the former). M2 — the invariant "every non-ported paint entry point closes the open frame before recording its own CB" — still fires from 8 paint paths whenever they're called, breaking frame coalescing.

Phase B.3 ports the remaining 8 non-ported paths into the frame builder so they too accumulate into the open frame instead of submitting standalone CBs. After B.3:

- M2 has only ONE remaining call site: the `render_composite_legacy` kill-switch path (engine.rs:5877), reachable when `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE=off`. With the kill-switch in its default ON position, that site is unreachable; if it ever fires it shows up in the `legacy=` per-source bucket (see §Per-source telemetry breakdown). B.5 deletes `render_composite_legacy` and the M2 close together.
- `frame_builder_close_reason_non_ported_paint_op[legacy]` is the only non-zero source expected; the 8 paint sources should fall to zero in the steady state.
- `submit_group_flushes_per_second` on the bee MATE workload should drop further; combined with B.2's ~75% absorption of submits, B.3 should push the residue toward 0 paint-induced flushes (only structural close triggers remain: scene compose, pin ceiling, scratch grow, present completion, sync wait, timeout, shutdown).

## Scope

The 8 non-ported entry points (audit findings, 2026-05-25):

| Function | engine.rs line | Family |
|----------|---------------|--------|
| `copy_area` | 2735 | TRANSFER |
| `cow_copy_area` | 3079 | TRANSFER |
| `put_image` | 4127 | TRANSFER |
| `fill_rect` | 2244 | FILL |
| `fill_rect_batch` | 2274 | FILL |
| `logic_fill` | 2489 | FILL |
| `image_text` | 4486 | GLYPH |
| `render_traps_or_tris` | 7183 | MASK |

All 8 are ported in one phase. No partial deferrals to a B.4.

The single remaining M2 close call site after B.3 is the one inside `render_composite_legacy` (engine.rs:5877). This call STAYS — it is reachable any time someone flips the B.2 kill-switch `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE=off`, and after B.3 the B.3-family gates may have opened a frame that legacy must close before recording its own CB. Removing the close while the kill-switch path is still alive is NOT kill-switch-safe (codex round 2 finding R2.1). The legacy close + the `close_open_frame_for_non_ported_op` helper both stay until B.5 deletes `render_composite_legacy` itself, at which point both become dead code together.

The legacy site is still counted in the per-source telemetry breakdown — see the `NonPortedSource::Legacy` variant below — so we can see at runtime whether anyone is actually flipping the kill-switch.

## Non-goals (explicitly out of scope)

- **Removing the SubmitGroup (M1 cap=1).** B.5.
- **Removing `render_composite_legacy` body.** B.5.
- **Folding scene compose into the frame builder.** B.4.
- **Multi-output frame builder.** B.4.
- **New paint primitives** (e.g., compute-shader paths, new RENDER ops).
- **New telemetry beyond per-source non_ported breakdown.** The renders_per_frame and existing close-reason histograms cover B.3.

## Architecture

B.3 is a *replication* phase, not a new-mechanisms phase. All B.2 infrastructure is reused as-is:

- `BatchResource` retired-scratch routing via `RenderEngineInner::adopt_retired_resource_for_gpu_retirement` (B.2 Task 1).
- Mechanism 2 descriptor watermark via `OpenFrame::frame_generation` (B.2 Task 3). Captured at open, consumed by every descriptor acquire during the open frame, watermark released on retire.
- Layout overlay source-of-truth via `RenderEngineInner::current_layout_for_drawable` + `commit_close_success`'s overlay→storage writeback (B.2 Task 4). Open-frame paint ops MUST consult the overlay accessor for any drawable's current layout — never read `storage.current_layout` directly.
- Atomic `OpenFrame::push_op_and_set_layouts` for op-append + overlay update (B.2 Task 11/12).
- Per-op `RecordedOp::*` enum variants on `open_frame.ops` (B.2 Task 6).
- Per-op `emit_recorded_*_into_cb` helpers dispatched from `emit_recorded_op_into_cb` (B.2 Task 12).

The B.3 contribution is: 5–7 new `RecordedOp::*` variants, one `_via_frame_builder` body per op, one `emit_recorded_*_into_cb` per op variant, four new family gates, and the per-source telemetry breakdown.

### The four op families

(Codex round 2 split MASK into MASK + GLYPH because `image_text` uses entirely different infrastructure from `render_traps_or_tris`. The four-family structure replaces the original three.)

The 8 ports group by recording shape:

**TRANSFER family** — `copy_area`, `cow_copy_area`, `put_image`. Recording shape:

```
emit_recorded_transfer_into_cb:
  pipeline_barrier2(src image → TRANSFER_SRC_OPTIMAL, dst image → TRANSFER_DST_OPTIMAL)
  cmd_copy_image / cmd_copy_buffer_to_image
  pipeline_barrier2(dst → SHADER_READ_ONLY_OPTIMAL, src → its prior layout)
```

No descriptor, no draw, no render pass. The payload carries dst image+layout, src image+layout (where applicable), and the rect/region descriptors.

`copy_area` and `cow_copy_area` share recording shape — one `RecordedCopyArea` variant covers both. The dispatcher decides which dst to resolve (drawable storage vs cow image).

`put_image` differs: the src is a `StagingBuffer` Arc (cloned into the open frame's pin set), and the emit uses `cmd_copy_buffer_to_image`. Distinct variant `RecordedPutImage`.

**FILL family** — `fill_rect`, `fill_rect_batch`, `logic_fill`. Recording shape mirrors B.2's render_composite — solid-color shader via the existing `RenderPipeline` cache, descriptor per op, open + draws + close:

```
emit_recorded_fill_into_cb:
  record_solid_color_clear(solid_src_image, color)   // per-op clear at emit time
  pipeline_get(StdPictOp::Src or Over, dst_format, ...)
  record_render_composite_open_with_old_layout(dst, dst_old_layout, pipeline)
  bind descriptor (src=solid_src_view, mask=white_mask_view, dst=white_mask_view)
  cmd_draw per (clip_scissor × rect)
  record_render_composite_close(dst)
```

`fill_rect` and `fill_rect_batch` share the composite-pipeline shape above. `logic_fill` is structurally different: it uses its own `LogicFillPipelineCache` (engine.rs:2453) with a logic-op render pass and push constants, NOT the composite pipeline cache, and writes no descriptor sets (engine.rs:2530, 2593-2697). The recorded payload for `logic_fill` carries the GC logic-mode + pipeline-cache key + push-constant payload; the emit replays a logic-op render pass open + draw + close. Same family (all are "fills") but two distinct recording sub-shapes inside it.

**MASK family** — `render_traps_or_tris` only. Two-stage CB (trap raster into mask scratch + render composite sampling that scratch). Recording shape:

```
emit_recorded_render_traps_or_tris_into_cb:
  ensure mask scratch is in COLOR_ATTACHMENT_OPTIMAL for trap raster
  record_trap_or_tri_raster pipeline into mask scratch (CLEAR + edge raster)
  pipeline_barrier2(mask scratch COLOR_ATTACHMENT → SHADER_READ_ONLY)
  record_render_composite_open_with_old_layout(dst, ...)
  bind descriptor (src + mask_scratch_view + dst)
  cmd_draw
  record_render_composite_close(dst)
```

The mask scratch lives in `EngineInner::mask_scratch` (already present), and its retired backing routes via `adopt_retired_resource_for_gpu_retirement` if growth fires (already wired by B.2 Task 1 plumbing — `MaskScratch` implements `BatchResource`).

**GLYPH family** — `image_text` only. NOT a mask-scratch op; structurally identical to B.1's `composite_glyphs_via_frame_builder`. Uses `V2GlyphAtlas` + `TextPipeline`, uploads glyph misses into the atlas via per-glyph staging buffers, then records a text-run draw against the atlas (engine.rs:4529, 4611, 4641, 4711). Recording shape mirrors B.1's `RecordedCompositeGlyphs` + `RecordedGlyphUpload` pair:

```
on append:
  for each glyph miss: pack into atlas, pin staging buffer via open.pins.pin_staging,
                       append RecordedOp::GlyphUpload (B.1 variant — reused verbatim)
  append RecordedOp::ImageText (new variant: dst + glyphs normalized to
                                {atlas_x, atlas_y, w, h, dst_x, dst_y} +
                                foreground color)

on emit (close-time replay):
  GlyphUpload arm in emit_recorded_op_into_cb (engine.rs:8425-8439, B.1 code,
                     match-arm not a separate fn) → cmd_copy_buffer_to_image
                     into atlas slots
  new ImageText arm in emit_recorded_op_into_cb → text-pipeline draw using
                     atlas + per-glyph positions (mirrors B.1 CompositeGlyphs
                     arm at engine.rs:8440-8478)
```

Reuses B.1's `RecordedGlyphUpload` variant verbatim for the upload side; only the draw-side `RecordedImageText` is new. Per-glyph atlas-cache lookup happens at append time inside `image_text_via_frame_builder` — the cache-insert key never reaches the recorded op, and the draw-side payload mirrors B.1's `RecordedCompositeGlyphs` shape (glyphs in atlas-coords already resolved at append, foreground color, dst metadata). NO atlas-key field, NO clip-state field on `RecordedImageText`.

## Op-variant catalog

| Variant | Box-wrapped? | Family | Notes |
|---------|--------------|--------|-------|
| `RecordedCopyArea(Box<RecordedCopyArea>)` | yes | TRANSFER | covers copy_area + cow_copy_area; payload carries `dst_id: DrawableId` (cow is a regular DrawableId per N3) + src_id + sub-rect + dst_old_layout |
| `RecordedPutImage(Box<RecordedPutImage>)` | yes | TRANSFER | staging_pin_idx (B.1 `pin_staging` style) + dst_id + sub-rect + dst_old_layout |
| `RecordedFillRect(Box<RecordedFillRect>)` | yes | FILL | op + color + rect-slice + clip + descriptor + dst_old_layout; covers fill_rect_batch as N≥1 rect-slice per N4 |
| `RecordedLogicFill(Box<RecordedLogicFill>)` | yes | FILL | GC logic mode + dst_format (cache key) + color + pre-clamped rect-slice + dst metadata + dst_old_layout; NO X RENDER picture-clip input, NO descriptor handle. Uses `LogicFillPipelineCache` directly per N6. |
| `RecordedImageText(Box<RecordedImageText>)` | yes | GLYPH | dst metadata + dst_old_layout + foreground color + glyphs Vec<{atlas_x, atlas_y, w, h, dst_x, dst_y}> (same shape as B.1's RecordedCompositeGlyphs); NO atlas-key, NO clip. Companion `RecordedOp::GlyphUpload` (B.1 variant — reused) carries per-glyph staging uploads. |
| `RecordedRenderTrapsOrTris(Box<RecordedRenderTrapsOrTris>)` | yes | MASK | trap/tri vertex pool + mask scratch view + dst (single variant covers both raster + composite stages per N5) |

No `RecordedFillRectBatch` variant — `fill_rect_batch` produces one `RecordedFillRect` per call carrying the entire rect slice (N4 decision).

All variants Box-wrapped per B.2 Task 6 (keep enum tag + ptr at 16B; the un-Boxed enum exceeded 256B with just `RenderComposite`). The size-budget test extends to assert each new payload ≤ 512B individually and `RecordedOp` itself stays ≤ 256B.

## Sub-gate structure

Four family gates, all default OFF during implementation:

- `YSERVER_FRAME_BUILDER_B3_TRANSFER` — gates copy_area + cow_copy_area + put_image.
- `YSERVER_FRAME_BUILDER_B3_FILL` — gates fill_rect + fill_rect_batch + logic_fill.
- `YSERVER_FRAME_BUILDER_B3_MASK` — gates render_traps_or_tris.
- `YSERVER_FRAME_BUILDER_B3_GLYPH` — gates image_text.

Each gate mirrors B.2 Task 5's machinery: `OnceLock<AtomicBool>` per gate, env parser accepting `on/1/true/yes` / `off/0/false/no`, `#[cfg(test)] set_*_for_tests` setter. Default-on flip happens once per family in its own bisect-clean commit at the end of the phase.

Rationale (from brainstorming + round-2 audit): four gates trade some env-var surface against the ability to disable one family if it regresses without losing the others. The MASK + GLYPH split adds a 4th gate but reflects the structural difference between mask-scratch raster (render_traps_or_tris) and glyph-atlas upload (image_text). Single-gate was rejected (regression bisect requires commit revert rather than env flip); per-op gates (8 total) were rejected as overkill.

Cross-family bisect works as: leave one or two gates ON, flip the others OFF, observe rendering. Family-internal bisect requires commit revert within that family.

## New invariants / pitfalls

These are net-new to B.3. All B.2 invariants (M1, M3, ticket-touch, renderer_failed fatal-after-failure, descriptor watermark, layout overlay, atomic push) apply verbatim.

### N1 — TRANSFER ops normalize to SHADER_READ_ONLY_OPTIMAL at end

B.2's render_composite always leaves dst in `SHADER_READ_ONLY_OPTIMAL` post-op (via `record_render_composite_close`). The overlay tracking assumes one terminal layout per drawable.

TRANSFER ops naturally transit through `TRANSFER_DST_OPTIMAL` during the copy. To preserve the single-terminal-layout assumption:

> Every TRANSFER op's emit MUST emit a final barrier `TRANSFER_DST_OPTIMAL → SHADER_READ_ONLY_OPTIMAL` for dst before returning, and the corresponding append's overlay update MUST set dst to `SHADER_READ_ONLY_OPTIMAL`.

Same for src: if the TRANSFER op transitions src to `TRANSFER_SRC_OPTIMAL` for the copy, the emit's final barrier MUST return src to `SHADER_READ_ONLY_OPTIMAL` (assuming src came from there; the overlay-resolved `src_old_layout` carries the prior value). For freshly-allocated src in `UNDEFINED`, this becomes a `UNDEFINED → SHADER_READ_ONLY_OPTIMAL` transition which is legal.

The append-time overlay update for src uses the same `push_op_and_set_layouts(op, &[(dst_id, SHADER_READ_ONLY_OPTIMAL), (src_id, SHADER_READ_ONLY_OPTIMAL)])` shape — two drawable updates per op.

### N2 — put_image staging buffer pin uses the existing B.1 slot

B.1's `FramePinSet::staging_buffers: Vec<Arc<StagingBuffer>>` (Mechanism 1) already exists. `put_image_via_frame_builder` pins the staging buffer via `open.pins.pin_staging(Arc::clone(&staging))` at append time — same call site shape as B.1's composite_glyphs work (engine.rs:5605, 5607). The helper returns a `staging_pin_idx` that the recorded op stores.

At emit time, the close-walk has already detached the pin set; the per-op emit function receives `pins: &FramePinSet` and reads the buffer via `pins.staging_buffers[staging_pin_idx.0 as usize].buffer` (mirroring B.1's `RecordedGlyphUpload` emit at engine.rs:8428). The emit does NOT reach back through `inner.frame_builder.open` — the open frame's pin set is no longer addressable at that point.

On frame retire, the Arc's refcount drops; on hitting zero, the StagingBuffer's Drop frees the Vk handles.

This is mechanically identical to B.1's composite_glyphs glyph-upload path — reuse, don't redefine. The pin-staging helper signature and the emit-time access pattern are both already in use by B.1.

### N3 — cow_copy_area's dst is a Drawable; no new overlay machinery needed

(Earlier draft of this spec proposed a separate `OpenFrame::cow_layout_in_frame` slot; codex audit found that to be the wrong model and the section was rewritten.)

The COW is allocated as a normal Drawable in the v2 store (see `backend.rs` around `allocate_cow_backing` and the v2 store's cow registration). `cow_copy_area` reads and writes it through `store.get(cow_id)` / `store.get_mut(cow_id)` like any other drawable (engine.rs:3118, 3302, 3452); scene compose samples it via the standard `storage.sample_view` (scene.rs:1741); destroy paths gate on `last_render_ticket` like any drawable. `PlatformBackend` has present-generation state for the cow (`record_present`, `commit_bo_present`) but no separate cow-layout API — none is needed.

This means cow_copy_area's frame-builder port needs **no new overlay machinery**: the existing `current_layout_for_drawable(store, cow_id)` accessor and the existing `commit_close_success`'s drawables walk handle the cow's in-frame layout the same way they handle any other drawable. The recorded op carries the cow's `DrawableId` like any other; `push_op_and_set_layouts` updates the cow's overlay entry in the same map.

What this **does** mean for the cow_copy_area port: the dispatcher needs to know whether the destination is "a regular drawable" (copy_area path) or "the cow" (cow_copy_area path). The existing legacy split between `copy_area` and `cow_copy_area` is preserved at the dispatch level (separate wrapper functions, separate `_via_frame_builder` bodies), but both record the same `RecordedCopyArea` variant with `dst_id: DrawableId` referring to either kind of drawable. Two `cow_copy_area` calls in one frame collapse correctly because both touch the same DrawableId in the overlay.

### N4 — fill_rect_batch is already the coalesced unit (one CB per call)

(Earlier draft claimed `fill_rect_batch` "already batches multiple fill_rect calls with matching attrs" — codex audit found that to be inaccurate. Decision moved from "deferred to implementation" to spec-time-final.)

The existing `fill_rect_batch` (engine.rs:2274) is already the coalesced unit: ONE call records one CB that clears a slice of rects with one color (engine.rs:2321, 2364, 2413). There is no cross-call coalesce key for fills; `pending_render_batch` and `RenderBatchKey` belong exclusively to `render_composite` batching (engine.rs:3623, 3697).

**Decision (final):** `fill_rect_batch_via_frame_builder` records ONE `RecordedFillRect`-style op per `fill_rect_batch` call, carrying the entire rect slice. Multiple `fill_rect_batch` calls within one frame collapse via the frame builder (each call appends its own op), but each call remains a single recorded op — preserving the existing "one CB per fill_rect_batch call" granularity at the per-op level while enabling frame-level collapse.

Splitting a `fill_rect_batch` call into one recorded op per individual rect would be new behavior, NOT preservation of existing architecture. Rejected.

The `pending_render_batch` (engine.rs:3982's `flush_render_batch`) is *render_composite-only* infrastructure and is unaffected by the FILL family port. It stays as the B.2 sub-gate=OFF fallback for render_composite; B.5 removes it when render_composite_legacy goes away.

### N5 — render_traps_or_tris has its own pipeline + scratch

Unlike fill_rect (which reuses render_composite's pipeline), render_traps_or_tris uses a dedicated `trap_pipeline` lazily initialized by `ensure_trap_assets` (engine.rs:2186) along with the `mask_scratch: Option<MaskScratch>` slot.

`render_traps_or_tris_via_frame_builder` peek-grows the mask scratch (Phase 9A pattern from B.2 Task 9), records the trap raster + composite as ONE recorded op (or two — TBD), and the emit replays the two-stage CB.

If recorded as one op, the variant holds both stages' inputs (trap pipeline params + composite descriptor + draws). If two, the `RecordedTrapRaster` + `RecordedTrapComposite` variants need consistent op-pair ordering (no other ops interleaved between them within the same frame).

Decision: ONE op variant covering both stages. Simpler emit, no inter-op ordering constraints to enforce.

The MASK family implementation MUST include an integration test exercising a mask scratch grow across frame boundaries (codex R3.4 — the test must explicitly cross frames; Phase 9A's close-before-grow rule means "two mask ops then a grow" inherently spans 2 frames, not 1).

Scenario:
1. Op A: `render_traps_or_tris` with small mask extent → records into open frame F1 against mask_scratch instance M0.
2. Op B: `render_traps_or_tris` with larger mask extent → Phase 9A peek detects the grow, calls `close_open_frame(... CloseReason::ScratchGrow)`, F1 closes + submits. Then `MaskScratch::ensure_image_size_returning_old` grows M0 → M1; the retired M0 routes through `adopt_retired_resource_for_gpu_retirement` to `submitted.back`'s pin set (case b — F1's just-pushed SubmittedOp).
3. Op C: third `render_traps_or_tris` with the same large extent → records into a NEW frame F2 against M1.
4. Force-close F2 via the test's timeout helper.

Observables (asserted by the test):
- `telemetry_submit_group_flushes_for_tests` delta = **2** (F1's close + F2's close).
- `frame_builder_close_reason_scratch_grow` lifetime counter incremented by **exactly 1** between pre- and post-test snapshot. This needs a new `#[cfg(test)]` telemetry accessor mirroring the existing per-backend submit-flush accessor pattern at `tests/v2_acceptance.rs:3955-4014` and `telemetry.rs:606-718`.
- Both F1 and F2 retire correctly (no leaked unsignaled FenceTicket); on F1's fence signal, M0 (the retired BatchResource) is released via `BatchResource::release`.

The test reuses the existing scratch-grow infrastructure (BatchResource adoption, Phase 9A) — no new test helper beyond the `frame_builder_close_reason_scratch_grow` accessor.

### N6 — logic_fill uses its own pipeline cache (separate sub-shape inside FILL family)

(Surfaced by codex round 2.) `logic_fill` doesn't share `fill_rect`'s recording shape. It uses `LogicFillPipelineCache` (engine.rs:2453, 2530) and records a logic-op render pass with push constants directly — NO descriptor set traffic (engine.rs:2593-2697).

This means `logic_fill_via_frame_builder` and `emit_recorded_logic_fill_into_cb` are structurally distinct from the fill_rect path even though they're in the same FILL family. The recording shape (matching the live legacy path at engine.rs:2562-2697):

```
on append (logic_fill_via_frame_builder):
  ensure trap-style assets (or logic-fill assets)
  clamp each rect to dst extent
  pipeline = LogicFillPipelineCache::get(logic_mode, dst_format)
  resolve dst_old_layout via current_layout_for_drawable
  append RecordedOp::LogicFill { pipeline, color, rects-clamped, dst_id,
                                 dst_image, dst_view, dst_extent, dst_old_layout }

emit_recorded_logic_fill_into_cb:
  // load-bearing barrier shape per engine.rs:2593-2618 — DO NOT collapse:
  pipeline_barrier2(
      src_stage    = ALL_COMMANDS,
      src_access   = SHADER_SAMPLED_READ | TRANSFER_WRITE | COLOR_ATTACHMENT_WRITE,
      dst_stage    = COLOR_ATTACHMENT_OUTPUT,
      dst_access   = COLOR_ATTACHMENT_WRITE,
      old_layout   = dst_old_layout,
      new_layout   = COLOR_ATTACHMENT_OPTIMAL,
  )
  cmd_begin_rendering(dst, load_op=LOAD)
  cmd_bind_pipeline(pipeline)
  cmd_set_viewport(once, dst extent)         // single viewport, NOT per rect
  cmd_push_constants(color)
  for each clamped rect:
      cmd_set_scissor(rect)
      cmd_draw(...)
  cmd_end_rendering
  pipeline_barrier2(
      src_stage    = COLOR_ATTACHMENT_OUTPUT,
      src_access   = COLOR_ATTACHMENT_WRITE,
      dst_stage    = FRAGMENT_SHADER,
      dst_access   = SHADER_SAMPLED_READ,
      old_layout   = COLOR_ATTACHMENT_OPTIMAL,
      new_layout   = SHADER_READ_ONLY_OPTIMAL,
  )
```

Important: `logic_fill` has NO X RENDER picture-clip input — the clip-rect handling that fill_rect / fill_rect_batch use does not apply. The recording payload carries dst metadata + color + the pre-clamped rect slice + the pipeline cache key (logic_mode + dst_format). NO descriptor handle.

Same family for telemetry / gate purposes; distinct recorded variant + emit fn.

### N7 — image_text is in the GLYPH family, NOT MASK

(Surfaced by codex round 2.) `image_text` does not use `mask_scratch` at all. It builds a `V2GlyphAtlas` + `TextPipeline` lazily, packs glyph misses into the atlas, uploads via per-glyph staging buffers, and records a text-run draw against the atlas (engine.rs:4529, 4611, 4641, 4711). This is structurally identical to B.1's `composite_glyphs_via_frame_builder` path.

Decision: `image_text` is its own GLYPH family with its own gate (`YSERVER_FRAME_BUILDER_B3_GLYPH`). Recording:

```
on append (image_text_via_frame_builder):
  ensure atlas / text pipeline (lazy)
  for each glyph miss:
    pack into atlas
    open.pins.pin_staging(Arc::clone(&staging))
    open.ops.push(RecordedOp::GlyphUpload(...))  ← B.1 variant, REUSED verbatim
  open.ops.push(RecordedOp::ImageText(...))

on emit (close-time replay):
  GlyphUpload ops → cmd_copy_buffer_to_image into the atlas's mapped sub-region
                    (existing B.1 `Op::GlyphUpload` match arm at engine.rs:8425-8439,
                    a match arm in `emit_recorded_op_into_cb`, NOT a separate fn)
  ImageText op → text-pipeline draw against the atlas + glyph positions
                 (new `Op::ImageText` match arm in the same dispatch, mirroring
                 B.1's `Op::CompositeGlyphs` arm at engine.rs:8440-8478)
```

Atlas-pin-ceiling logic from B.1 applies: the per-frame ceiling protects against unbounded staging-buffer accumulation. image_text's port reuses B.1's pin_ceiling counter; no new counter needed.

The audit comparison: B.1's `composite_glyphs_via_frame_builder` (engine.rs:5605-5618) already does this exact dance for the X RENDER composite_glyphs path. image_text's body is mechanically the same with a different draw call at emit-time (text run vs composite-glyphs).

## Frame close triggers after B.3

Pre-B.3 close-reason histogram has 9 triggers (per B.2 Task 1's `CloseReason::ScratchGrow` addition):

```
scene_compose, non_ported, legacy_sc, present_completion, sync_wait,
timeout, shutdown, pin_ceiling, scratch_grow
```

After B.3:
- `non_ported` should drop to ~zero (only the dead `_legacy` path's M2 close, plus any new non-ported ops that get added — none in B.3 scope).
- All other triggers unchanged.
- Per-source non_ported breakdown (Task 1) provides empirical signal during the rollout.

`CloseReason::NonPortedPaintOp` and the `close_open_frame_for_non_ported_op` helper both stay through all of B.3. They are still reachable under family-gate=OFF for any of the 8 ports, and under the B.2 kill-switch (off) for `render_composite_legacy`. B.5 deletes both when `render_composite_legacy` itself is removed.

## Per-source telemetry breakdown (Task 1)

Replace single `frame_builder_close_reason_non_ported_paint_op: u64` with 9 per-source buckets (the 8 ported ops + Legacy for the kill-switch path):

```rust
pub(crate) frame_builder_close_reason_non_ported_copy_area: u64,
pub(crate) frame_builder_close_reason_non_ported_cow_copy_area: u64,
pub(crate) frame_builder_close_reason_non_ported_put_image: u64,
pub(crate) frame_builder_close_reason_non_ported_fill_rect: u64,
pub(crate) frame_builder_close_reason_non_ported_fill_rect_batch: u64,
pub(crate) frame_builder_close_reason_non_ported_logic_fill: u64,
pub(crate) frame_builder_close_reason_non_ported_image_text: u64,
pub(crate) frame_builder_close_reason_non_ported_render_traps_or_tris: u64,
pub(crate) frame_builder_close_reason_non_ported_legacy: u64,
```

Add a `NonPortedSource` enum (9 variants, all required — no optional / split-helper alternative per codex round 2 finding R2.5):

```rust
pub(crate) enum NonPortedSource {
    CopyArea, CowCopyArea, PutImage,
    FillRect, FillRectBatch, LogicFill,
    ImageText, RenderTrapsOrTris,
    /// Kill-switch path: `render_composite_legacy` (engine.rs:5877)
    /// when `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE=off`. Counted
    /// separately so post-B.3 hardware runs can confirm the
    /// kill-switch is not being flipped accidentally; steady-state
    /// value should be 0 unless explicitly opted in.
    Legacy,
}
```

Change `close_open_frame_for_non_ported_op` to take a `source: NonPortedSource` parameter. All 9 call sites pass the corresponding variant. The telemetry recorder fans out to the per-source bucket.

Log line extension. To keep the existing `v2_telemetry:` line under ~120 columns (codex R3.6 — the existing line at `telemetry.rs:375-400` is already long; adding 9 inline fields pushes past readability), split into TWO log lines. The existing `v2_telemetry:` line keeps ONLY the aggregate `non_ported_total` in its `close_reasons[...]` block:

```
v2_telemetry: ... close_reasons[scene_compose=N non_ported_total=N legacy_sc=N
              present_completion=N sync_wait=N timeout=N shutdown=N
              pin_ceiling=N scratch_grow=N]
```

A NEW sibling line emits the per-source breakdown, with shortened field names in the `non_ported_sources` namespace:

```
v2_telemetry: non_ported_sources[copy=N cow_copy=N put_image=N
              fill=N fill_batch=N logic=N image_text=N traps_tris=N legacy=N]
```

Field-name shortenings (the `non_ported_sources` namespace makes them unambiguous): `copy_area` → `copy`, `fill_rect` → `fill`, `logic_fill` → `logic`, `render_traps_or_tris` → `traps_tris`. The aggregate `non_ported_total` (= sum of the 9 sources) stays on the primary line for backward-compat with downstream analyzers.

This task lands BEFORE the ports begin. A bee-side run with the breakdown reveals which sources actually dominate — guides per-op port priority and lets us measure progress after each family flips ON.

## Task structure (39 tasks, 7 phases)

### Phase 0 — Telemetry breakdown (2 tasks)

(Codex audit split: the breakdown touches three surfaces — `CloseReason` API, helper signature + 8 call sites, and the telemetry counter/log/test surface. Splitting into API plumbing first, then telemetry wiring, keeps each commit reviewable.)

**Task 1a: `NonPortedSource` enum + helper signature + call sites.**
- Add `NonPortedSource` enum to `frame_builder.rs` with 9 variants (all required, including `Legacy`).
- Change `close_open_frame_for_non_ported_op` signature to take `source: NonPortedSource`.
- Update all 9 call sites in `engine.rs` (the 8 ported paint ops at lines 2255, 2285, 2505, 2747, 3091, 4140, 4498, 7215, plus the `render_composite_legacy` kill-switch site at 5877 — `NonPortedSource::Legacy`).
- Plumb `source` through to `FrameCloseEvent` (no telemetry-counter change yet — just carry the data).

**Task 1b: Telemetry buckets + log line + tests.**
- Replace single `frame_builder_close_reason_non_ported_paint_op` bucket with 9 per-source buckets in `telemetry.rs` (the 8 ports + `legacy`).
- Extend `record_frame_builder_close` to fan out by `source`.
- Extend the `v2_telemetry:` log line with the per-source breakdown.
- Unit test: each `NonPortedSource` variant increments the right bucket.
- Keep the aggregate `non_ported_total` field (= sum of all 9 sources) for backward-compat with downstream analyzers.

### Phase 1 — Foundation (2 tasks)

**Task 2: Four family gates.**
- `YSERVER_FRAME_BUILDER_B3_TRANSFER`, `_FILL`, `_MASK`, `_GLYPH` — `OnceLock<AtomicBool>` machinery per B.2 Task 5.
- Default OFF.
- Backend test setters.
- Default-OFF unit tests (matching B.2's pattern with env-var skip guard).

**Task 3: New RecordedOp variants.**
- Add `RecordedCopyArea`, `RecordedPutImage`, `RecordedFillRect`, `RecordedLogicFill`, `RecordedImageText`, `RecordedRenderTrapsOrTris` struct stubs + enum variants. (No `RecordedFillRectBatch` — per N4, `fill_rect_batch` produces one `RecordedFillRect` per call with N≥1 rects.)
- Box-wrap each variant.
- Size-budget tests per variant.
- Stub `unimplemented!()` arms in `emit_recorded_op_into_cb` for each new variant.

(No new `OpenFrame` field — cow_copy_area uses the existing drawable overlay machinery per N3.)
(image_text reuses B.1's `RecordedOp::GlyphUpload` variant verbatim per N7 — no new GLYPH-upload variant needed.)

### Phase 2 — TRANSFER family (12 tasks: 4 per op × 3 ops)

**For each of `copy_area`, `cow_copy_area`, `put_image`:**

- **Dispatch split.** Extract existing body into `<op>_legacy` (with the existing M2 close at top). Add stub `<op>_via_frame_builder` returning early `Ok(stats)`. Dispatcher routes on `frame_builder_b3_transfer_enabled()`.
- **`<op>_via_frame_builder` body.** Prelude (renderer_failed check, dst metadata, ticket-touch dst+src, src/dst overlay first_touch). Resolve any scratch via `BatchResource` if applicable (put_image staging Arc clone). Append `RecordedOp::<variant>` via `push_op_and_set_layouts`.
- **`emit_recorded_<op>_into_cb`.** Barriers + transfer + barriers-back. Layout normalization to `SHADER_READ_ONLY_OPTIMAL` per N1.
- **Integration test + drop wrapper M2 close.** Two-call collapse test (`v2_frame_builder_<op>_collapses_two_in_one_frame`). Remove the `self.close_open_frame_for_non_ported_op(...)?` call from the op's wrapper.

`cow_copy_area`'s body task uses the existing `current_layout_for_drawable(store, cow_id)` accessor — no special cow handling beyond resolving the cow Drawable from the dispatch (per N3).

`put_image`'s body task additionally clones the staging Arc into `frame_builder.open.pins.staging_buffers` (per N2).

### Phase 3 — FILL family (12 tasks: 4 per op × 3 ops)

**For each of `fill_rect`, `fill_rect_batch`, `logic_fill`:** the same 4-task shape (dispatch split, `_via_frame_builder` body, emit, integration test + drop wrapper M2 close).

The recording shape diverges within the family (per N6):

- `fill_rect` + `fill_rect_batch`: body uses the existing `RenderPipeline` cache + descriptor pool ring (same call sites as B.2 Task 11), with `src=solid_src_image`, `mask=white_mask_image`, `dst=white_mask_image` (no readback, no mask). Emit calls `record_solid_color_clear` for the solid_src and runs the standard composite open/draws/close triplet.
- `logic_fill`: body uses `LogicFillPipelineCache` (engine.rs:2453) — distinct from the composite pipeline. The recorded payload carries GC logic mode + dst_format + color + pre-clamped rect slice + dst metadata + dst_old_layout. Emit records a logic-op render pass with push constants directly; NO descriptor traffic, NO picture-clip input. See N6 for the full barrier shape.

`fill_rect_batch`'s body records ONE `RecordedFillRect` per `fill_rect_batch` call carrying the entire rect slice (per N4). Multiple `fill_rect_batch` calls in one frame each become their own recorded op; the frame builder collapses across calls. The wrapper M2 close drops.

### Phase 4 — MASK family (4 tasks: render_traps_or_tris only)

**For `render_traps_or_tris`:** the same 4-task shape. Body peek-grows the mask scratch (Phase 9A pattern from B.2 Task 9 — close-before-grow if frame has prior ops, then ensure_returning_old + adopt). Append a single `RecordedOp::RenderTrapsOrTris` carrying both stages' inputs.

Emit replays the two-stage CB: (a) trap edge raster into mask scratch via the trap pipeline; (b) standard composite open+draws+close sampling the mask scratch view (per N5).

Includes the cross-frame mask-scratch-grow integration test per N5 (3-op sequence `(small, large, large)` spanning frames F1 + F2 — the grow inherently triggers `close-before-grow` between op 1 and op 2, so F1 closes before the grow and F2 reopens against M1).

### Phase 5 — GLYPH family (4 tasks: image_text only)

**For `image_text`:** the same 4-task shape. Body lazily ensures the `V2GlyphAtlas` + `TextPipeline` (mirroring B.1's composite_glyphs body), packs glyph misses into the atlas with per-glyph staging-buffer pins via `open.pins.pin_staging`, appends `RecordedOp::GlyphUpload` variants per miss (B.1 variant — REUSED), then appends `RecordedOp::ImageText`.

Emit replays per N7: B.1's existing `Op::GlyphUpload` match arm in `emit_recorded_op_into_cb` (engine.rs:8425-8439, a match arm, NOT a separate function) handles the upload side; the new `Op::ImageText` arm in the same dispatch records the text-pipeline draw against the atlas, mirroring B.1's `Op::CompositeGlyphs` arm (engine.rs:8440-8478).

Reuses B.1's atlas pin-ceiling logic — no new counter or invariant introduced beyond what B.1 already enforces for composite_glyphs.

### Phase 6 — Wrap-up (3 tasks)

**Task N-2: cargo +nightly fmt + plain clippy.** Mirrors B.2 Task 19. Plain clippy only (NOT pedantic per AGENTS.md). Fix any clippy warnings the B.3 surface introduced.

**Task N-1: Flip all four family gates default ON.**
- Flip `_TRANSFER`, `_FILL`, `_MASK`, `_GLYPH` default branches from `false` to `true`.
- Update the default-OFF assertions in tests.
- The `render_composite_legacy` M2 close at engine.rs:5877 STAYS (per round-2 finding R2.1 — the legacy kill-switch path still needs to close B.3-opened frames if anyone flips `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE=off`). The close + the `close_open_frame_for_non_ported_op` helper both wait for B.5's deletion of `render_composite_legacy`.
- One bisect-clean commit per family flip (4 commits total) OR one combined commit (acceptable since the flips are independent). Prefer per-family commits for revert granularity.

**Task N: Status doc + bee hardware-gate placeholder.**
- Append Phase B.3 entry to `docs/status.md` matching the B.1/B.2 entry shape.
- Document the per-source telemetry breakdown.
- Bee hardware-smoke gate placeholder: pre-flip and post-flip MATE-drag telemetry comparison (`non_ported/s` per source, `submit_group_flushes/s`, frame builder `ops/frame_avg` increase).

## Acceptance gates

### Implementation gates (per-task, validated during plan execution)

- `cargo build` clean for each task.
- `cargo test -p yserver --lib` green for each task.
- `cargo +nightly fmt --check` clean.
- `cargo clippy --workspace --all-targets` (plain, NOT pedantic) clean for the B.3 surface.
- Each integration test in Phase 2/3/4 demonstrates two-op collapse-to-one-submit for its op.

### Hardware gates (user-driven, after Task N)

- **bee MATE-load** with all four families flipped ON:
  - `close_reasons[non_ported_total]/s` → ≤ 10 (vs ~900–1100 pre-B.3). Steady-state should approach zero; non-zero only from rare unported edge paths.
  - `submit_group_flushes/s` drop by 30–50% beyond B.2's ~75% absorption. Combined with B.2: target ~200–400 submits/s on bee MATE drag (the original Phase B spec target).
  - `ops/frame_avg` rises from B.2's ~1.7 to ~4–8 (more ops per frame as M2 stops fragmenting them).
  - `frame_builder_aborts/s = 0` (no new failure modes under load).
- **silence (dual-output)** regression check — no scene-compose regression, no ERROR_DEVICE_LOST, no fault chains.
- **yoga / iMac / fuji** regression checks — no new errors on those platforms.
- **Cross-vendor sanity** — same MATE drag on a non-radv host (nvidia, intel, lavapipe) — no new validation VUIDs introduced by B.3.

## Open questions

These remain unresolved at design time and ride along to implementation:

1. **`render_traps_or_tris` one-vs-two recorded ops.** Currently planned as one variant covering both stages (per N5). If the two-stage CB is too dense to fit in one payload (>512B), split into `RecordedTrapRaster` + `RecordedTrapComposite` with consistent pairing enforcement at append+emit. Re-evaluate after the body lands.

2. **`logic_fill` pipeline variants.** GC logic modes (Copy, And, Or, Xor, …) map to render pipeline variants. The existing `_legacy` path handles this; ensure the via_frame_builder path's pipeline cache lookup keys on the logic mode correctly.

3. **`close_open_frame_for_non_ported_op` removal timing.** Both the helper and its `render_composite_legacy` call site (engine.rs:5877) stay through B.3 — the legacy path needs the close any time the B.2 kill-switch is OFF, even after B.3 family gates are ON (codex round-2 R2.1). B.5 removes the legacy body, the helper, and `CloseReason::NonPortedPaintOp` together.

## Risk register

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Validation VUIDs in MASK family emit (mask scratch + composite cross-barrier) | Medium | High (silent corruption) | Bee vkdebug pass after MASK family ports land, before flipping `_MASK` default ON. |
| cow_copy_area's dispatcher wires `cow_id` to wrong DrawableStore entry | Low | High (corruption — wrong drawable's storage written) | Resolved at spec-time per N3 (cow is a regular Drawable in store). Integration test: two `cow_copy_area` calls in one frame collapse correctly and the cow's `storage.current_layout` updates via the standard `commit_close_success` walk. |
| put_image staging-buffer Arc not pinned to the right fence (UAF if staging buffer drops before frame retires) | Low | High (GPU fault) | B.1's pin-set mechanism is proven; mirror exactly. Test with concurrent destroy of the source Pixmap mid-frame. |
| Per-source telemetry breakdown introduces a hot-path branch | Very low | Very low (perf) | The branch is once per non_ported close (~100/s); negligible. |
| `fill_rect_batch` decision goes wrong (over- or under-batches) | Medium | Medium (lower coalesce or extra closes) | Re-measurable: the per-source telemetry will show fill_rect vs fill_rect_batch counts; pick the option that maximizes frame-builder absorption. |

## References

- Phase B spec: `docs/superpowers/specs/2026-05-24-frame-builder-phase-b-design.md`.
- Phase B.2 plan: `docs/superpowers/plans/2026-05-24-frame-builder-phase-b2.md`.
- Phase A spec: `docs/superpowers/specs/2026-05-23-frame-builder-submit-rate-design.md`.
- Audit findings (2026-05-25): `non_ported` aggregate counter confirmed in `telemetry.rs:679-682`; eight close-call-sites enumerated at `engine.rs:2255, 2285, 2505, 2747, 3091, 4140, 4498, 5877, 7215`.
