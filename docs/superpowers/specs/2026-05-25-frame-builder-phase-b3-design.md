# Phase B.3 — Eliminate M2: Port all remaining non-ported paint ops into the FrameBuilder

**Date:** 2026-05-25
**Status:** Design — awaiting user review then implementation plan
**Predecessors:** Phase B.2 (`2026-05-24-frame-builder-phase-b-design.md`, plan `2026-05-24-frame-builder-phase-b2.md`)

## Goal

After Phase B.2, the frame builder absorbs `render_composite` and `render_fill_rectangles` (the latter delegates to the former). M2 — the invariant "every non-ported paint entry point closes the open frame before recording its own CB" — still fires from 8 paint paths whenever they're called, breaking frame coalescing.

Phase B.3 ports the remaining 8 non-ported paths into the frame builder so they too accumulate into the open frame instead of submitting standalone CBs. After B.3:

- M2 has zero call sites — the invariant is structurally retired, not just narrowed.
- `frame_builder_close_reason_non_ported_paint_op` should fall to zero in the steady state.
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
| `image_text` | 4486 | MASK |
| `render_traps_or_tris` | 7183 | MASK |

All 8 are ported in one phase. No partial deferrals to a B.4.

The single remaining M2 close call site after B.3 is the one inside `render_composite_legacy` (engine.rs:5877) — which only fires when the B.2 sub-gate `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE=off` is set (kill-switch). With sub-gate=on (default after Task 20 of B.2), legacy is unreachable. B.3's wrap-up task removes the legacy M2 close and deletes `close_open_frame_for_non_ported_op` entirely once `render_composite_legacy` itself is removed (which happens alongside B.5's SubmitGroup removal — outside this phase, but the B.3 work makes the M2 helper otherwise unused).

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

The B.3 contribution is: 5–7 new `RecordedOp::*` variants, one `_via_frame_builder` body per op, one `emit_recorded_*_into_cb` per op variant, three new family gates, and the per-source telemetry breakdown.

### The three op families

The 8 ports group naturally by recording shape:

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

`fill_rect` and `fill_rect_batch` may share `RecordedFillRect` if the batched variant is just N>1 rects with the same color (decision deferred to implementation — if the recorded payload diverges in any field, keep them split). `logic_fill` carries a GC logic-mode discriminant in addition.

**MASK family** — `image_text`, `render_traps_or_tris`. Two-stage CBs (mask scratch fill + render composite). Recording shape:

```
emit_recorded_mask_render_into_cb:
  ensure mask scratch is in TRANSFER_DST_OPTIMAL
  (image_text:) cmd_copy_buffer_to_image to load glyph bits into mask scratch
  (traps/tris:) record_trap_or_tri_raster pipeline into mask scratch
  pipeline_barrier2(mask scratch → SHADER_READ_ONLY)
  record_render_composite_open_with_old_layout(dst, ...)
  bind descriptor (src + mask_scratch_view + dst)
  cmd_draw
  record_render_composite_close(dst)
```

Mirrors composite_glyphs (B.1) shape. The mask scratch lives in `EngineInner::mask_scratch` (already present), and its retired backing routes via `adopt_retired_resource_for_gpu_retirement` if growth fires (already wired by B.2 Task 1 plumbing — `MaskScratch` implements `BatchResource`).

## Op-variant catalog

| Variant | Box-wrapped? | Family | Notes |
|---------|--------------|--------|-------|
| `RecordedCopyArea(Box<RecordedCopyArea>)` | yes | TRANSFER | covers copy_area + cow_copy_area; payload carries `dst_target` enum (Drawable vs Cow) |
| `RecordedPutImage(Box<RecordedPutImage>)` | yes | TRANSFER | staging_pin_idx + dst + sub-rect |
| `RecordedFillRect(Box<RecordedFillRect>)` | yes | FILL | op + color + rects + clip + descriptor; may also cover fill_rect_batch |
| `RecordedFillRectBatch(Box<RecordedFillRectBatch>)` | yes (if distinct) | FILL | only if `fill_rect_batch` recording diverges from `fill_rect` |
| `RecordedLogicFill(Box<RecordedLogicFill>)` | yes | FILL | + GC logic mode |
| `RecordedImageText(Box<RecordedImageText>)` | yes | MASK | glyph mask payload + dst |
| `RecordedRenderTrapsOrTris(Box<RecordedRenderTrapsOrTris>)` | yes | MASK | trap/tri vertex pool + mask scratch + dst |

All variants Box-wrapped per B.2 Task 6 (keep enum tag + ptr at 16B; the un-Boxed enum exceeded 256B with just `RenderComposite`). The size-budget test extends to assert each new payload ≤ 512B individually and `RecordedOp` itself stays ≤ 256B.

## Sub-gate structure

Three family gates, all default OFF during implementation:

- `YSERVER_FRAME_BUILDER_B3_TRANSFER` — gates copy_area + cow_copy_area + put_image.
- `YSERVER_FRAME_BUILDER_B3_FILL` — gates fill_rect + fill_rect_batch + logic_fill.
- `YSERVER_FRAME_BUILDER_B3_MASK` — gates image_text + render_traps_or_tris.

Each gate mirrors B.2 Task 5's machinery: `OnceLock<AtomicBool>` per gate, env parser accepting `on/1/true/yes` / `off/0/false/no`, `#[cfg(test)] set_*_for_tests` setter. Default-on flip happens once per family in its own bisect-clean commit at the end of the phase.

Rationale (from brainstorming): three gates trade some env-var surface against the ability to disable one family if it regresses without losing the others. Single-gate was rejected (regression bisect requires commit revert rather than env flip); per-op gates (8 total) were rejected as overkill.

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

B.1's `FramePinSet::staging_buffers: Vec<Arc<StagingBuffer>>` (Mechanism 1) already exists. `put_image_via_frame_builder` clones the `Arc<StagingBuffer>` into `frame_builder.open.pins.staging_buffers` at append time. The recorded op stores the index (`staging_pin_idx: u32`) — same pattern as B.1's `RecordedGlyphUpload`.

At emit time, `inner.frame_builder.open.pins.staging_buffers[idx].buffer` is the vk::Buffer for `cmd_copy_buffer_to_image`. On frame retire, the Arc's refcount drops; on hitting zero, the StagingBuffer's Drop frees the Vk handles.

This is mechanically identical to B.1's composite_glyphs glyph-upload path — reuse, don't redefine.

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

Unlike fill_rect (which reuses render_composite's pipeline), render_traps_or_tris uses a dedicated `trap_pipeline` (engine.rs:2134's `ensure_trap_pipeline`) and the `mask_scratch: Option<MaskScratch>` slot.

`render_traps_or_tris_via_frame_builder` peek-grows the mask scratch (Phase 9A pattern from B.2 Task 9), records the trap raster + composite as ONE recorded op (or two — TBD), and the emit replays the two-stage CB.

If recorded as one op, the variant holds both stages' inputs (trap pipeline params + composite descriptor + draws). If two, the `RecordedTrapRaster` + `RecordedTrapComposite` variants need consistent op-pair ordering (no other ops interleaved between them within the same frame).

Decision: ONE op variant covering both stages. Simpler emit, no inter-op ordering constraints to enforce.

The MASK family implementation MUST include an integration test exercising "two mask ops in the same frame followed by a mask scratch grow" — i.e., op N records with mask_scratch instance M0, op N+1 needs a larger mask_scratch and triggers Phase 9A's close-before-grow, then a third op records against M1. Validates that the per-frame mask_scratch view stays consistent through grow events.

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

The B.3 wrap-up task (Phase 5, Task N-1) removes the `CloseReason::NonPortedPaintOp` variant once `close_open_frame_for_non_ported_op` itself is removed in B.5. For B.3, the variant stays (it still fires under family-gate=OFF for the corresponding ops).

## Per-source telemetry breakdown (Task 1)

Replace single `frame_builder_close_reason_non_ported_paint_op: u64` with per-source buckets:

```rust
pub(crate) frame_builder_close_reason_non_ported_copy_area: u64,
pub(crate) frame_builder_close_reason_non_ported_cow_copy_area: u64,
pub(crate) frame_builder_close_reason_non_ported_put_image: u64,
pub(crate) frame_builder_close_reason_non_ported_fill_rect: u64,
pub(crate) frame_builder_close_reason_non_ported_fill_rect_batch: u64,
pub(crate) frame_builder_close_reason_non_ported_logic_fill: u64,
pub(crate) frame_builder_close_reason_non_ported_image_text: u64,
pub(crate) frame_builder_close_reason_non_ported_render_traps_or_tris: u64,
```

Add a `NonPortedSource` enum:

```rust
pub(crate) enum NonPortedSource {
    CopyArea, CowCopyArea, PutImage,
    FillRect, FillRectBatch, LogicFill,
    ImageText, RenderTrapsOrTris,
}
```

Change `close_open_frame_for_non_ported_op` to take a `source: NonPortedSource` parameter. Each of the 8 call sites passes the corresponding variant. The telemetry recorder fans out to the per-source bucket.

Log line extension:

```
close_reasons[... non_ported_total=N copy_area=N cow_copy=N put_image=N
              fill_rect=N fill_batch=N logic_fill=N image_text=N traps_tris=N ...]
```

The aggregate `non_ported_total` is kept (= sum of the 8) for backward-compat with downstream analyzers.

This task lands BEFORE the ports begin. A bee-side run with the breakdown reveals which sources actually dominate — guides per-op port priority and lets us measure progress after each family flips ON.

## Task structure (39 tasks, 6 phases)

### Phase 0 — Telemetry breakdown (2 tasks)

(Codex audit split: the breakdown touches three surfaces — `CloseReason` API, helper signature + 8 call sites, and the telemetry counter/log/test surface. Splitting into API plumbing first, then telemetry wiring, keeps each commit reviewable.)

**Task 1a: `NonPortedSource` enum + helper signature + call sites.**
- Add `NonPortedSource` enum to `frame_builder.rs`.
- Change `close_open_frame_for_non_ported_op` signature to take `source: NonPortedSource`.
- Update all 8 call sites in `engine.rs` (lines 2255, 2285, 2505, 2747, 3091, 4140, 4498, 7215) with the correct variant; the `render_composite_legacy` site (5877) gets a `NonPortedSource::Legacy` or equivalent.
- Plumb `source` through to `FrameCloseEvent` (no telemetry-counter change yet — just carry the data).

**Task 1b: Telemetry buckets + log line + tests.**
- Replace single `frame_builder_close_reason_non_ported_paint_op` bucket with 8 per-source buckets in `telemetry.rs` (+1 for `Legacy` if we tag the kill-switch path separately).
- Extend `record_frame_builder_close` to fan out by `source`.
- Extend the `v2_telemetry:` log line with the per-source breakdown.
- Unit test: each `NonPortedSource` variant increments the right bucket.
- Keep the aggregate `non_ported_total` field (= sum of all sources) for backward-compat with downstream analyzers.

### Phase 1 — Foundation (2 tasks)

**Task 2: Three family gates.**
- `YSERVER_FRAME_BUILDER_B3_TRANSFER`, `_FILL`, `_MASK` — `OnceLock<AtomicBool>` machinery per B.2 Task 5.
- Default OFF.
- Backend test setters.
- Default-OFF unit tests (matching B.2's pattern with env-var skip guard).

**Task 3: New RecordedOp variants.**
- Add `RecordedCopyArea`, `RecordedPutImage`, `RecordedFillRect`, `RecordedLogicFill`, `RecordedImageText`, `RecordedRenderTrapsOrTris` struct stubs + enum variants. (`RecordedFillRectBatch` deferred — added only if implementation needs it; per N4, the default is one `RecordedFillRect`-style op per `fill_rect_batch` call.)
- Box-wrap each variant.
- Size-budget tests per variant.
- Stub `unimplemented!()` arms in `emit_recorded_op_into_cb` for each new variant.

(No new `OpenFrame` field — cow_copy_area uses the existing drawable overlay machinery per N3.)

### Phase 2 — TRANSFER family (12 tasks: 4 per op × 3 ops)

**For each of `copy_area`, `cow_copy_area`, `put_image`:**

- **Dispatch split.** Extract existing body into `<op>_legacy` (with the existing M2 close at top). Add stub `<op>_via_frame_builder` returning early `Ok(stats)`. Dispatcher routes on `frame_builder_b3_transfer_enabled()`.
- **`<op>_via_frame_builder` body.** Prelude (renderer_failed check, dst metadata, ticket-touch dst+src, src/dst overlay first_touch). Resolve any scratch via `BatchResource` if applicable (put_image staging Arc clone). Append `RecordedOp::<variant>` via `push_op_and_set_layouts`.
- **`emit_recorded_<op>_into_cb`.** Barriers + transfer + barriers-back. Layout normalization to `SHADER_READ_ONLY_OPTIMAL` per N1.
- **Integration test + drop wrapper M2 close.** Two-call collapse test (`v2_frame_builder_<op>_collapses_two_in_one_frame`). Remove the `self.close_open_frame_for_non_ported_op(...)?` call from the op's wrapper.

`cow_copy_area`'s body task uses the existing `current_layout_for_drawable(store, cow_id)` accessor — no special cow handling beyond resolving the cow Drawable from the dispatch (per N3).

`put_image`'s body task additionally clones the staging Arc into `frame_builder.open.pins.staging_buffers` (per N2).

### Phase 3 — FILL family (12 tasks: 4 per op × 3 ops)

**For each of `fill_rect`, `fill_rect_batch`, `logic_fill`:**

Same 4-task shape. The body uses the existing `RenderPipeline` cache + descriptor pool ring (same call sites as B.2 Task 11), with `src=solid_src_image`, `mask=white_mask_image`, `dst=white_mask_image` (no readback, no mask). The emit calls `record_solid_color_clear` for the solid_src and runs the standard composite open/draws/close triplet.

`fill_rect_batch`'s body records ONE `RecordedFillRect` per `fill_rect_batch` call carrying the entire rect slice (per N4). Multiple `fill_rect_batch` calls in one frame each become their own recorded op; the frame builder collapses across calls. The wrapper M2 close drops.

`logic_fill` carries the GC logic mode through to the recorded payload; emit selects the appropriate pipeline variant.

### Phase 4 — MASK family (8 tasks: 4 per op × 2 ops)

**For each of `image_text`, `render_traps_or_tris`:**

Same 4-task shape. The body peek-grows the mask scratch (Phase 9A pattern from B.2 Task 9 — close-before-grow if frame has prior ops, then ensure_returning_old + adopt). Append a single `RecordedOp::<variant>` carrying both stages' inputs.

The emit replays the two-stage CB: (a) mask scratch fill (transfer for image_text glyph bits, or trap raster pipeline for render_traps_or_tris); (b) standard composite open+draws+close sampling the mask scratch view.

### Phase 5 — Wrap-up (3 tasks)

**Task N-2: cargo +nightly fmt + plain clippy.** Mirrors B.2 Task 19. Plain clippy only (NOT pedantic per AGENTS.md). Fix any clippy warnings the B.3 surface introduced.

**Task N-1: Flip all three family gates default ON + drop dead M2 close from render_composite_legacy.**
- Flip `_TRANSFER`, `_FILL`, `_MASK` default branches from `false` to `true`.
- Update the default-OFF assertions in tests.
- Remove the `close_open_frame_for_non_ported_op` call inside `render_composite_legacy` (engine.rs:5877) — under B.2 sub-gate=ON, legacy is unreachable; the call is dead code. Keep the helper function itself for B.5's removal.
- Single bisect-clean commit per family flip (3 commits total) OR one commit covering all three (acceptable since they're independent). Prefer per-family commits for revert granularity.

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

- **bee MATE-load** with all three families flipped ON:
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

3. **`close_open_frame_for_non_ported_op` removal timing.** Phase 5 Task N-1 removes the dead call site in `render_composite_legacy`. The helper function itself stays until B.5 (where `render_composite_legacy` is removed). Optionally: rename the helper to `close_open_frame_legacy_kill_switch_path` to clarify its scope shrinks under B.3.

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
