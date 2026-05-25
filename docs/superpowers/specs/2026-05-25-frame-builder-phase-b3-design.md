# Phase B.3 — Eliminate M2: Port all remaining non-ported paint ops into the FrameBuilder

**Date:** 2026-05-25
**Status:** Design — awaiting user review then implementation plan
**Predecessors:** Phase B.2 (`2026-05-24-frame-builder-phase-b-design.md`, plan `2026-05-24-frame-builder-phase-b2.md`)

## Goal

After Phase B.2, the frame builder absorbs `render_composite` and `render_fill_rectangles` (the latter delegates to the former). M2 — the invariant "every non-ported paint entry point closes the open frame before recording its own CB" — still fires from 8 paint paths whenever they're called, breaking frame coalescing.

Phase B.3 ports the remaining 8 non-ported paths into the frame builder so they too accumulate into the open frame instead of submitting standalone CBs. After B.3:

- M2 has only ONE remaining call site: the `render_composite_legacy` kill-switch path (engine.rs:5877), reachable when `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE=off`. With the kill-switch in its default ON position, that site is unreachable. B.5 deletes `render_composite_legacy` and the M2 close together.
- The 8 paint sources contribute 0 non_ported closes in steady state (they no longer call the helper at all).
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

All 8 are ported in one phase. No partial deferrals to a B.4. Each port is a rip-and-replace of the existing v2 body — no `_legacy` fallback split, no per-family env gate, no default-OFF unit tests, no flip commit (see "Rollout style" below).

The single remaining M2 close call site after B.3 is the one inside `render_composite_legacy` (engine.rs:5877). This call STAYS — it is reachable any time someone flips the B.2 kill-switch `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE=off`, and after B.3 the rewritten ops may have opened a frame that legacy must close before recording its own CB. Removing the close while the kill-switch path is still alive is NOT kill-switch-safe (codex round 2 finding R2.1). The legacy close + the `close_open_frame_for_non_ported_op` helper both stay until B.5 deletes `render_composite_legacy` itself, at which point both become dead code together.

## Non-goals (explicitly out of scope)

- **Removing the SubmitGroup (M1 cap=1).** B.5.
- **Removing `render_composite_legacy` body.** B.5.
- **Folding scene compose into the frame builder.** B.4.
- **Multi-output frame builder.** B.4.
- **New paint primitives** (e.g., compute-shader paths, new RENDER ops).
- **New telemetry / per-source breakdowns.** Skipped per the rip-and-replace style.
- **v1 backend preservation.** If the v2 changes break v1 compile, apply only the minimal compile fix. v1 is scheduled for removal in a later phase.

## Architecture

B.3 is *almost entirely* a replication phase. All B.2 infrastructure is reused as-is:

- `BatchResource` retired-scratch routing via `RenderEngineInner::adopt_retired_resource_for_gpu_retirement` (B.2 Task 1).
- Mechanism 2 descriptor watermark via `OpenFrame::frame_generation` (B.2 Task 3). Captured at open, consumed by every descriptor acquire during the open frame, watermark released on retire.
- Layout overlay source-of-truth via `RenderEngineInner::current_layout_for_drawable` + `commit_close_success`'s overlay→storage writeback (B.2 Task 4). Open-frame paint ops MUST consult the overlay accessor for any drawable's current layout — never read `storage.current_layout` directly.
- Atomic `OpenFrame::push_op_and_set_layouts` for op-append + overlay update (B.2 Task 11/12).
- Per-op `RecordedOp::*` enum variants on `open_frame.ops` (B.2 Task 6).
- Per-op `emit_recorded_*_into_cb` helpers dispatched from `emit_recorded_op_into_cb` (B.2 Task 12).

The single net-new mechanism (codex rounds 7+8) is the LIVE concrete-scratch slot for `copy_area`'s self-overlap path, per N8. The scratch is owned by `RecordedCopyArea::self_overlap_scratch: Option<ScratchImage>` from append until close, then transferred (via `std::mem::take` during the close-path ops walk) into `SubmittedOp::scratch: Vec<ScratchImage>` (slot renamed from the legacy `Option<ScratchImage>` to `Vec<ScratchImage>` — one-field structural extension of an existing legacy slot; legacy callers pass `Vec::new()`, B.3 self-overlap pushes 1 per op). There is NO `OpenFrame::live_scratches` sibling collector; the on-variant slot is the sole source of truth. This is a one-field structural extension, NOT a new mechanism class.

The B.3 contribution is: 6 new `RecordedOp::*` variants, one rewritten body per op (in-place, no `_via_frame_builder` suffix or `_legacy` split), one `emit_recorded_*_into_cb` per op variant, and the N8 live-scratch slot extension. NO new family gates, NO per-source telemetry breakdown (rip-and-replace style).

### The four op families

(Codex round 2 split MASK into MASK + GLYPH because `image_text` uses entirely different infrastructure from `render_traps_or_tris`. The four-family structure replaces the original three.)

The 8 ports group by recording shape:

**TRANSFER family** — `copy_area`, `cow_copy_area`, `put_image`. Recording shape (high-level; see N1 for exact load-bearing barrier access masks):

```
emit_recorded_transfer_into_cb:
  pipeline_barrier2(<see N1 for exact stage/access mask shape>:
                    src image  src_old_layout → TRANSFER_SRC_OPTIMAL
                    dst image  dst_old_layout → TRANSFER_DST_OPTIMAL)
  cmd_copy_image / cmd_copy_buffer_to_image
  pipeline_barrier2(<see N1 for exact stage/access mask shape>:
                    dst image  TRANSFER_DST_OPTIMAL → SHADER_READ_ONLY_OPTIMAL
                    src image  TRANSFER_SRC_OPTIMAL → SHADER_READ_ONLY_OPTIMAL)
```

No descriptor, no draw, no render pass. The payload carries dst image+old-layout, src image+old-layout (where applicable), and the rect/region descriptors. **Terminal layout is SHADER_READ_ONLY_OPTIMAL for BOTH src and dst per N1** (an earlier draft said "src → its prior layout" but N1's single-terminal rule wins — the spec is corrected here for consistency).

The exact stage/access masks for these barriers MUST mirror the legacy paths' shapes precisely (engine.rs:2951 for copy_area's pre/post barrier, engine.rs:3300 for cow_copy_area, engine.rs:4210 for put_image). See N1 below for the mandate.

`copy_area` and `cow_copy_area` share recording shape — one `RecordedCopyArea` variant covers both. The dispatcher decides which dst to resolve (drawable storage vs cow image — both are DrawableIds per N3).

`put_image` differs: the src is a `StagingBuffer` Arc (cloned into the open frame's pin set via `open.pins.pin_staging`), and the emit uses `cmd_copy_buffer_to_image`. Distinct variant `RecordedPutImage`. No src image to barrier — staging buffers have no layout transition; only the dst gets the pre/post pair.

**FILL family** — `fill_rect`, `fill_rect_batch`, `logic_fill`. NOT a composite-pipeline path. Legacy `fill_rect_batch` (engine.rs:2360-2401) uses `cmd_clear_attachments` directly: pure REPLACE semantics, no blending, no shader, no descriptor. (Earlier drafts of this spec routed FILL through the composite pipeline with `StdPictOp::Src or Over` — that was wrong; `Over` would preserve dst for translucent fills and the composite shader's `src × mask × blend` adds nothing over a straight clear. Codex round-7 catch.)

`fill_rect` and `fill_rect_batch` recording shape:

```
emit_recorded_fill_into_cb:
  pipeline_barrier2(dst dst_old_layout → COLOR_ATTACHMENT_OPTIMAL):
    src_stage  = ALL_COMMANDS
    src_access = SHADER_SAMPLED_READ | TRANSFER_WRITE | COLOR_ATTACHMENT_WRITE
    dst_stage  = COLOR_ATTACHMENT_OUTPUT
    dst_access = COLOR_ATTACHMENT_WRITE
  cmd_begin_rendering(dst color attachment, load=LOAD, store=STORE)
  cmd_set_viewport(full extent)
  cmd_set_scissor(full render_area)
  cmd_clear_attachments(color=solid color, clear_rect slice = all rects)
  cmd_end_rendering
  pipeline_barrier2(dst COLOR_ATTACHMENT_OPTIMAL → SHADER_READ_ONLY_OPTIMAL):
    src_stage = COLOR_ATTACHMENT_OUTPUT, src_access = COLOR_ATTACHMENT_WRITE
    dst_stage = FRAGMENT_SHADER,           dst_access = SHADER_SAMPLED_READ
```

**LOAD-BEARING — `load=LOAD` (codex round-8 catch).** `cmd_clear_attachments` writes ONLY the listed `clear_rect`s — pixels outside those rects within the render area MUST be preserved from the prior dst contents. Legacy uses `load_op = LOAD` (engine.rs:2354-2358); `DONT_CARE` would invalidate the entire render area, trashing untouched pixels and producing direct visual corruption. The pre-barrier's `src_access` mask drains prior reads/writes (mirroring legacy at engine.rs:2333-2348) so the implicit COLOR_ATTACHMENT_OUTPUT load doesn't race a still-in-flight earlier op on the same dst.

No pipeline bind, no descriptor, no shader draw. The payload carries `dst_id` + dst extent/format + color + the pre-clamped rect-slice + `dst_old_layout`. `fill_rect_batch` carries N≥1 rects; `fill_rect` carries N=1.

`logic_fill` is structurally different again: it uses `LogicFillPipelineCache` (engine.rs:2453) with its own logic-op render pass and push constants, NOT the composite pipeline cache AND NOT `cmd_clear_attachments` (engine.rs:2530, 2593-2697). The recorded payload for `logic_fill` carries the GC logic-mode + pipeline-cache key + push-constant payload; the emit replays a logic-op render pass open + draw + close. Same family (all are "fills") but THREE distinct recording sub-shapes inside it (cmd_clear_attachments for fill_rect/fill_rect_batch vs logic_op pipeline for logic_fill).

**MASK family** — `render_traps_or_tris` only. Two-stage CB (trap raster into mask scratch + render composite sampling that scratch). Recording shape:

```
emit_recorded_render_traps_or_tris_into_cb:
  // Derive emit-time flags from the recorded std_op.
  needs_dst_readback = recorded.std_op.needs_dst_readback()
  needs_full_dst     = needs_full_dst_for_op(recorded.std_op)  // matches engine.rs:7472

  // ── Source resolve (engine.rs:7356-7400) ──
  resolve src_view / src_extent at emit time from engine caches:
    Drawable { id, swizzle_class } → ensure_drawable_view(id, sampler_for_repeat(src_repeat), swizzle_class)
                                       (swizzle_class from append-time snapshot — see N5 payload)
    Solid(color) → solid_src_view + (1×1 extent); record_solid_color_clear(solid_src, color)
    Gradient(xid) → engine.picture_paint[xid].image_view() + extent
                    (intrinsic xform already snapped at append into RecordedTrapSrcKind::Gradient)
  resolve mask_view / mask_extent FRESH from engine.mask_scratch  // never pinned (N5)
  resolve dst_image / dst_view from store.get(dst_id).storage     // overlay-resolved layout per B.2

  // ── Trap raster phase (engine.rs:7531-7647) ──
  pipeline_barrier2(mask scratch  current_layout → COLOR_ATTACHMENT_OPTIMAL)
  cmd_begin_rendering(mask scratch attachment_view, load=CLEAR, render_area=bbox)
  cmd_bind_pipeline(trap or tri pipeline per prim_kind)
  cmd_bind_vertex_buffers(instance_buf — recorded.vertex_pool_pin)
  cmd_push_constants(TrapDrawPushConsts { mask_extent (fresh), bbox_origin, bbox_size })
  cmd_set_viewport(mask_extent) / cmd_set_scissor(bbox)
  cmd_draw(4 verts, recorded.instance_count)
  cmd_end_rendering

  // ── Composite phase (engine.rs:7665-7735) ──
  pipeline_barrier2(mask scratch COLOR_ATTACHMENT_OPTIMAL → SHADER_READ_ONLY_OPTIMAL)

  // dst_readback (only when std_op.needs_dst_readback() — append-side
  // already called ensure_dst_readback_returning_old + adopt, so the
  // engine's dst_readback now holds the right backing).
  dst_readback_view = if needs_dst_readback {
      record_dst_readback_copy(dst → readback)
      engine.dst_readback.view(dst_format, dst_has_alpha)   // engine.rs:7425
  } else {
      white_mask_view
  }

  // Pipeline + descriptor — fresh lookup, NOT recorded (engine.rs:7437-7464).
  pipeline = render_pipelines.get(recorded.std_op, recorded.dst_format,
                                  recorded.dst_has_alpha,
                                  /*component_alpha=*/false)
  descriptor_set = allocate_descriptor_for_views_into_ring(
                       frame_generation,           // B.2 Mechanism 2
                       src_view, mask_view, dst_readback_view)

  // Render-dst-rect / mask-offset choice (engine.rs:7473-7480).
  let (render_dst_x, render_dst_y, render_w, render_h, mask_off_x, mask_off_y) =
      if needs_full_dst { (0, 0, dst_extent.w, dst_extent.h, -bbox_x, -bbox_y) }
      else              { (bbox_x, bbox_y, bbox_w, bbox_h, 0, 0) };

  composite_attrs = CompositeAttrs {
      src_extent,
      mask_extent (fresh from engine.mask_scratch),
      src_repeat: if recorded.src_is_synthetic_1x1 { REPEAT_PAD } else { recorded.src_repeat },
      mask_repeat: REPEAT_NONE,
      src_force_opaque: recorded.src_force_opaque,
      mask_force_opaque: false,
      src_xform: compose_affines(recorded_gradient_intrinsic.unwrap_or(IDENTITY), recorded.user_src_xform),
      mask_xform: IDENTITY,
  }
  rects = recorded.clip_scissors.iter().map(|s| CompositeRect {
      src_x:0, src_y:0,
      mask_x: mask_off_x, mask_y: mask_off_y,
      dst_x: render_dst_x, dst_y: render_dst_y,
      width: render_w, height: render_h,
  })
  // (Actually: per-scissor draw — clip_scissors already pre-clamped at append.)
  vk_render::record_render_composite(vk, cb, dst_adapter, pipeline, layout, descriptor_set,
                                     &composite_attrs, &rects, &recorded.clip_scissors)
```

**The full RecordedRenderTrapsOrTris field listing is the single source of truth at N5 RecordedRenderTrapsOrTris payload below.** That listing is authoritative for spec/code parity. Skipping any field there would replay against stale or wrong inputs (codex catches across rounds 7 + 8: solid sources replay with stale solid_src contents; gradient sources skip the intrinsic xform; pipeline lookup picks wrong blend/alpha; clip_scissors drop X RENDER picture-clip; dst_readback view selection wrong).

**Mask scratch ownership (MEDIUM ownership fix, codex round-7).** The current/live `EngineInner::mask_scratch` is engine-owned mutable state used directly at emit time. The recorded variant does NOT pin mask_scratch — it MUST resolve `engine.mask_scratch` fresh inside `emit_recorded_render_traps_or_tris_into_cb`, taking the current `image()`, `attachment_view()`, `image_view()`, `extent()`, and `current_layout()` as inputs to the two stages. (An earlier draft of the spec carried a `mask_scratch_pin_idx` — that conflates "live scratch used by replay" with "old grown-away backing awaiting fence release". The two are different concepts.)

**Post-emit CPU-layout writeback (LOAD-BEARING — codex round-10 catch).** After the composite phase's exit barrier transitions mask_scratch back to `SHADER_READ_ONLY_OPTIMAL`, the emit MUST call `engine.mask_scratch.as_mut().expect("ensured").set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)` to advance the engine's CPU-tracked layout. The legacy path does this at engine.rs:7740-7749. Skipping the writeback leaves `mask_scratch.current_layout()` stale at `COLOR_ATTACHMENT_OPTIMAL` (or whatever pre-emit layout was), so the NEXT `render_traps_or_tris` op in the next frame's emit reads the wrong old_layout for its pre-barrier and builds a transition from the wrong source state — a VUID-class bug.

Only the GROWN-AWAY old backing routes through `BatchResource`. When the mask-scratch peek-grow (Phase 9A) in `render_traps_or_tris` fires, `ensure_returning_old` returns a `Box<dyn BatchResource>` for the prior backing; that Box routes via `adopt_retired_resource_for_gpu_retirement` to one of three tiers (engine.rs:756-786) — same flow B.2 Task 1 wired:
- **(a) Open frame present:** `open.pins.adopt_retired(boxed)` — the open frame's pin set carries it through close.
- **(b) No open frame, in-flight SubmittedOp present:** `submitted.back_mut().append_retired_scratch(boxed)` — the newest in-flight fence owner adopts it.
- **(c) Nothing in flight:** `boxed.release(&self.vk)` — immediate release.

The current `mask_scratch` after grow stays as engine-owned state, used by all subsequent ops in this frame and beyond.

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

Reuses B.1's `RecordedGlyphUpload` variant verbatim for the upload side; only the draw-side `RecordedImageText` is new. Per-glyph atlas-cache lookup happens at append time inside `image_text` — the cache-insert key never reaches the recorded op, and the draw-side payload mirrors B.1's `RecordedCompositeGlyphs` shape (glyphs in atlas-coords already resolved at append, foreground color, dst metadata). NO atlas-key field, NO clip-state field on `RecordedImageText`.

## Op-variant catalog

| Variant | Box-wrapped? | Family | Notes |
|---------|--------------|--------|-------|
| `RecordedCopyArea(Box<RecordedCopyArea>)` | yes | TRANSFER | covers copy_area + cow_copy_area; payload carries `dst_id: DrawableId` (cow is a regular DrawableId per N3) + `src_id: DrawableId` + sub-rect + `dst_old_layout` + `src_old_layout` + `self_overlap_scratch: Option<LiveScratchImage>` (Some when src_id == dst_id; this is a LIVE concrete `ScratchImage` owned by the recorded variant, NOT a `FramePinSet::retired_resources` slot — see N1's self-overlap subcase and N8 for the ownership model) |
| `RecordedPutImage(Box<RecordedPutImage>)` | yes | TRANSFER | `staging_pin_idx` (B.1 `pin_staging` style) + `dst_id: DrawableId` + sub-rect + `dst_old_layout` (no src image layout — staging buffers have no layout per N1) |
| `RecordedFillRect(Box<RecordedFillRect>)` | yes | FILL | **`dst_id: DrawableId`** + dst extent + dst format + color + pre-clamped rect-slice (covers fill_rect as N=1 and fill_rect_batch as N≥1 per N4) + `dst_old_layout`. NO pipeline, NO descriptor, NO clip — uses `cmd_clear_attachments` directly (codex round-7 catch — earlier drafts erroneously routed through the composite pipeline). |
| `RecordedLogicFill(Box<RecordedLogicFill>)` | yes | FILL | **`dst_id: DrawableId`** + GC logic mode + `dst_format` (cache map key) + `opaque_alpha: bool` (cache key — codex round-8 catch: caller-provided GC parameter, NOT derived; changes color-write-mask) + color + pre-clamped rect-slice + dst extent + `dst_old_layout`; NO X RENDER picture-clip input, NO descriptor handle. Uses `inner.logic_fill_caches[dst_format].get(function, opaque_alpha)` per N6 (engine.rs:2530-2538). |
| `RecordedImageText(Box<RecordedImageText>)` | yes | GLYPH | **`dst_id: DrawableId`** + dst extent + `dst_old_layout` + foreground color + glyphs `Vec<{atlas_x, atlas_y, w, h, dst_x, dst_y}>` (same shape as B.1's RecordedCompositeGlyphs); NO atlas-key, NO clip. Companion `RecordedOp::GlyphUpload` (B.1 variant — reused) carries per-glyph staging uploads. dst format MUST be `B8G8R8A8_UNORM` — runs on other formats are dropped append-side per legacy gate (engine.rs:4515-4526) and N7. |
| `RecordedRenderTrapsOrTris(Box<RecordedRenderTrapsOrTris>)` | yes | MASK | See the full field listing in **N5 RecordedRenderTrapsOrTris payload** below — too many fields for a table row. Single variant covers both raster + composite stages. NO `mask_scratch_pin_idx` — emit re-resolves `engine.mask_scratch` fresh per N5. |

No `RecordedFillRectBatch` variant — `fill_rect_batch` produces one `RecordedFillRect` per call carrying the entire rect slice (N4 decision).

All variants Box-wrapped per B.2 Task 6 (keep enum tag + ptr at 16B; the un-Boxed enum exceeded 256B with just `RenderComposite`). The size-budget test extends to assert each new payload ≤ 512B individually and `RecordedOp` itself stays ≤ 256B.

## Rollout style

**Rip-and-replace, not gated rollout.** Per project preference (user feedback 2026-05-25), v2 paint-op porting drops the dispatch-split / family-gate / default-OFF / flip-commit pattern that B.1 + B.2 used. Each B.3 op gets its REWRITTEN body — no `<op>_legacy` / `<op>_via_frame_builder` split, no `YSERVER_FRAME_BUILDER_B3_*` env vars, no `OnceLock<AtomicBool>` gate machinery, no default-OFF unit tests, no per-family flip commit.

The B.1 and B.2 ports (`composite_glyphs`, `render_composite`, `render_fill_rectangles`) keep their existing gate structure as-shipped — this change applies to B.3 onward. `render_composite_legacy` (B.2's kept fallback) is unaffected; it stays behind `YSERVER_FRAME_BUILDER_RENDER_COMPOSITE` until B.5 deletes it.

Implication for the v2 → v1 ABI surface: v1 may break due to v2-internal type changes (e.g., `SubmittedOp::scratch` rename per N8). Per the same project preference, if v1 breaks, apply only the bare-minimum compile fix; v1 is scheduled for removal in a later phase.

## New invariants / pitfalls

These are net-new to B.3. All B.2 invariants (M1, M3, ticket-touch, renderer_failed fatal-after-failure, descriptor watermark, layout overlay, atomic push) apply verbatim.

### N1 — TRANSFER ops normalize to SHADER_READ_ONLY_OPTIMAL at end + mirror legacy barrier shapes exactly

B.2's render_composite always leaves dst in `SHADER_READ_ONLY_OPTIMAL` post-op (via `record_render_composite_close`). The overlay tracking assumes one terminal layout per drawable.

**Terminal layout rule.** TRANSFER ops naturally transit through `TRANSFER_DST_OPTIMAL` (and `TRANSFER_SRC_OPTIMAL` for the src side of copy_area / cow_copy_area). To preserve the single-terminal-layout assumption:

> Every TRANSFER op's emit MUST emit a final barrier transitioning dst (and src when it was transitioned) to `SHADER_READ_ONLY_OPTIMAL` before returning. The corresponding append's overlay update MUST set BOTH dst and (if applicable) src to `SHADER_READ_ONLY_OPTIMAL` via `push_op_and_set_layouts(op, &[(dst_id, SHADER_READ_ONLY_OPTIMAL), (src_id, SHADER_READ_ONLY_OPTIMAL)])`.

Earlier drafts said "src → its prior layout" — that's wrong. The single-terminal-layout invariant supersedes; if a later op needs src in a different layout it issues its own barrier from SHADER_READ_ONLY.

For freshly-allocated dst in `UNDEFINED`, the pre-barrier becomes `UNDEFINED → TRANSFER_DST_OPTIMAL` which is legal.

**LOAD-BEARING — exact stage/access masks (codex round-6 catch).** The high-level pseudocode in §Architecture is intentionally abstract; the implementation MUST mirror the legacy paths' barrier shapes precisely. These are not interchangeable with the simpler "dst_access_mask = TRANSFER_WRITE" shape — the legacy paths chose these masks specifically to drain prior compose / fill / put-image writes on the same image, the same class of producer-mask requirement that caused B.2's RAW hazard at vkCmdBeginRendering.

For `copy_area` / `cow_copy_area` pre-barrier (mirror engine.rs:2951-3032 — both src AND dst):

```
pipeline_barrier2(
    src_stage    = ALL_COMMANDS,
    src_access   = SHADER_SAMPLED_READ | TRANSFER_WRITE | COLOR_ATTACHMENT_WRITE,
    dst_stage    = COPY,
    dst_access   = TRANSFER_READ  (for src image)  /  TRANSFER_WRITE  (for dst image),
    old_layout   = src_old_layout  /  dst_old_layout  (from overlay),
    new_layout   = TRANSFER_SRC_OPTIMAL  /  TRANSFER_DST_OPTIMAL,
)
```

For `copy_area` / `cow_copy_area` post-barrier (mirror legacy's exit shape — both src AND dst back to SHADER_READ_ONLY):

```
pipeline_barrier2(
    src_stage    = COPY,
    src_access   = TRANSFER_READ  (for src)  /  TRANSFER_WRITE  (for dst),
    dst_stage    = FRAGMENT_SHADER,
    dst_access   = SHADER_SAMPLED_READ,
    old_layout   = TRANSFER_SRC_OPTIMAL  /  TRANSFER_DST_OPTIMAL,
    new_layout   = SHADER_READ_ONLY_OPTIMAL,
)
```

For `put_image` pre-barrier (mirror engine.rs:4210 — DST only; staging buffers have no layout):

```
pipeline_barrier2(
    src_stage    = ALL_COMMANDS,
    src_access   = SHADER_SAMPLED_READ | COLOR_ATTACHMENT_WRITE,
    dst_stage    = COPY,
    dst_access   = TRANSFER_WRITE,
    old_layout   = dst_old_layout,
    new_layout   = TRANSFER_DST_OPTIMAL,
)
```

For `put_image` post-barrier: identical shape to copy_area's dst post-barrier.

The implementer MUST cross-reference engine.rs:2951 (copy_area), engine.rs:3300 (cow_copy_area), engine.rs:4210 (put_image) at implementation time and replicate the producer src_access bits verbatim. Generic "TRANSFER_WRITE only" producer masks would recreate the B.2-class RAW hazard against prior frame-builder ops that wrote to the same image (a `render_composite` writing the dst followed by a `copy_area` reading it in the SAME frame would race without `COLOR_ATTACHMENT_WRITE` in the src_access).

**Same-image scratch subcase** (copy_area self-overlap): when src_id == dst_id, the legacy path uses a temporary scratch image (engine.rs:2814-2918). The B.3 port MUST preserve this path. See N8 for the LIVE concrete-scratch ownership model (the scratch image is NOT a `FramePinSet::retired_resources` Box; an earlier draft of the spec said so and codex round-7 caught it).

The emit's barrier sequence has THREE pairs instead of two:

```
emit_copy_area_self_overlap_into_cb (engine.rs:2814-2918 mirror):
  barrier src                 SHADER_READ_ONLY → TRANSFER_SRC_OPTIMAL
  barrier scratch.image       UNDEFINED        → TRANSFER_DST_OPTIMAL  (TOP_OF_PIPE / NONE → COPY / TRANSFER_WRITE)
  cmd_copy_image src@TRANSFER_SRC  →  scratch@TRANSFER_DST  (src_rect → scratch[0,0])
  barrier scratch             TRANSFER_DST     → TRANSFER_SRC_OPTIMAL
  barrier src                 TRANSFER_SRC     → TRANSFER_DST_OPTIMAL
  cmd_copy_image scratch@TRANSFER_SRC → src@TRANSFER_DST (scratch → src@dst_rect)
  barrier src                 TRANSFER_DST     → SHADER_READ_ONLY_OPTIMAL
  // scratch lives as long as the SubmittedOp; drops in retirement.
```

This is the most complex TRANSFER emit path; the spec calls it out so the implementer does not assume the simple two-barrier shape covers it.

### N2 — put_image staging buffer pin uses the existing B.1 slot

B.1's `FramePinSet::staging_buffers: Vec<Arc<StagingBuffer>>` (Mechanism 1) already exists. `put_image` pins the staging buffer via `open.pins.pin_staging(Arc::clone(&staging))` at append time — same call site shape as B.1's composite_glyphs work (engine.rs:5605, 5607). The helper returns a `staging_pin_idx` that the recorded op stores.

At emit time, the close-walk has already detached the pin set; the per-op emit function receives `pins: &FramePinSet` and reads the buffer via `pins.staging_buffers[staging_pin_idx.0 as usize].buffer` (mirroring B.1's `RecordedGlyphUpload` emit at engine.rs:8428). The emit does NOT reach back through `inner.frame_builder.open` — the open frame's pin set is no longer addressable at that point.

On frame retire, the Arc's refcount drops; on hitting zero, the StagingBuffer's Drop frees the Vk handles.

This is mechanically identical to B.1's composite_glyphs glyph-upload path — reuse, don't redefine. The pin-staging helper signature and the emit-time access pattern are both already in use by B.1.

### N3 — cow_copy_area's dst is a Drawable; no new overlay machinery needed

(Earlier draft of this spec proposed a separate `OpenFrame::cow_layout_in_frame` slot; codex audit found that to be the wrong model and the section was rewritten.)

The COW is allocated as a normal Drawable in the v2 store (see `backend.rs` around `allocate_cow_backing` and the v2 store's cow registration). `cow_copy_area` reads and writes it through `store.get(cow_id)` / `store.get_mut(cow_id)` like any other drawable (engine.rs:3118, 3302, 3452); scene compose samples it via the standard `storage.sample_view` (scene.rs:1741); destroy paths gate on `last_render_ticket` like any drawable. `PlatformBackend` has present-generation state for the cow (`record_present`, `commit_bo_present`) but no separate cow-layout API — none is needed.

This means cow_copy_area's frame-builder port needs **no new overlay machinery**: the existing `current_layout_for_drawable(store, cow_id)` accessor and the existing `commit_close_success`'s drawables walk handle the cow's in-frame layout the same way they handle any other drawable. The recorded op carries the cow's `DrawableId` like any other; `push_op_and_set_layouts` updates the cow's overlay entry in the same map.

What this **does** mean for the cow_copy_area port: `copy_area` and `cow_copy_area` remain two separate entry-point functions (mirroring the pre-B.3 split — `cow_copy_area` is its own dispatcher target distinct from `copy_area`), but both rewritten bodies record the same `RecordedCopyArea` variant with `dst_id: DrawableId` referring to either kind of drawable. Two `cow_copy_area` calls in one frame collapse correctly because both touch the same DrawableId in the overlay.

### N4 — fill_rect_batch is already the coalesced unit (one CB per call)

(Earlier draft claimed `fill_rect_batch` "already batches multiple fill_rect calls with matching attrs" — codex audit found that to be inaccurate. Decision moved from "deferred to implementation" to spec-time-final.)

The existing `fill_rect_batch` (engine.rs:2274) is already the coalesced unit: ONE call records one CB that clears a slice of rects with one color (engine.rs:2321, 2364, 2413). There is no cross-call coalesce key for fills; `pending_render_batch` and `RenderBatchKey` belong exclusively to `render_composite` batching (engine.rs:3623, 3697).

**Decision (final):** `fill_rect_batch` records ONE `RecordedFillRect`-style op per `fill_rect_batch` call, carrying the entire rect slice. Multiple `fill_rect_batch` calls within one frame collapse via the frame builder (each call appends its own op), but each call remains a single recorded op — preserving the existing "one CB per fill_rect_batch call" granularity at the per-op level while enabling frame-level collapse.

Splitting a `fill_rect_batch` call into one recorded op per individual rect would be new behavior, NOT preservation of existing architecture. Rejected.

The `pending_render_batch` (engine.rs:3982's `flush_render_batch`) is *render_composite-only* infrastructure and is unaffected by the FILL family port. It stays as the B.2 sub-gate=OFF fallback for render_composite; B.5 removes it when render_composite_legacy goes away.

### N5 — render_traps_or_tris has its own pipeline + scratch

Unlike fill_rect/fill_rect_batch (which use `cmd_clear_attachments` directly, not a shader pipeline at all — see the FILL family section), render_traps_or_tris uses a dedicated `trap_pipeline` for the raster stage and the composite pipeline for the composite stage (engine.rs:2186 `ensure_trap_assets`), along with the `mask_scratch: Option<MaskScratch>` slot.

**Decision (final):** the rewritten `render_traps_or_tris` peek-grows the mask scratch (Phase 9A pattern from B.2 Task 9) and records the trap raster + composite as ONE recorded op. The variant `RecordedRenderTrapsOrTris` holds the append-time-stable inputs (trap pipeline keys, source identity, composite operator + clip + xforms + bbox, vertex pool pin) — full listing in **N5 RecordedRenderTrapsOrTris payload** below. Mask-scratch identity, dst_readback view, pipeline handle, and descriptor sets are NOT recorded — emit re-resolves all four fresh from engine state. The emit replays the two-stage CB.

A two-op split (`RecordedTrapRaster` + `RecordedTrapComposite`) was rejected: it would impose pairing-ordering constraints at append+emit (no other ops interleaved between them within the same frame) that the single-op shape avoids. Only payload size remains an implementation-time consideration; see Open Questions.

The MASK family implementation MUST include an integration test exercising a mask scratch grow across frame boundaries (codex R3.4 — the test must explicitly cross frames; Phase 9A's close-before-grow rule means "two mask ops then a grow" inherently spans 2 frames, not 1).

Scenario:
1. Op A: `render_traps_or_tris` with small mask extent → records into open frame F1 against mask_scratch instance M0.
2. Op B: `render_traps_or_tris` with larger mask extent → Phase 9A peek detects the grow, calls `close_open_frame(... CloseReason::ScratchGrow)`, F1 closes + submits. Then `MaskScratch::ensure_image_size_returning_old` grows M0 → M1; the retired M0 routes through `adopt_retired_resource_for_gpu_retirement` to case (b) — `submitted.back_mut().append_retired_scratch(boxed)` (F1's just-pushed SubmittedOp's retired_resources).
3. Op C: third `render_traps_or_tris` with the same large extent → records into a NEW frame F2 against M1.
4. Force-close F2 via the test's timeout helper.

Observables (asserted by the test):
- `telemetry_submit_group_flushes_for_tests` delta = **2** (F1's close + F2's close).
- `frame_builder_close_reason_scratch_grow` lifetime counter incremented by **exactly 1** between pre- and post-test snapshot. This needs a new `#[cfg(test)]` telemetry accessor mirroring the existing per-backend submit-flush accessor pattern at `tests/v2_acceptance.rs:3955-4014` and `telemetry.rs:606-718`.
- Both F1 and F2 retire correctly (no leaked unsignaled FenceTicket); on F1's fence signal, M0 (the retired BatchResource) is released via `BatchResource::release`.

The test reuses the existing scratch-grow infrastructure (BatchResource adoption, Phase 9A) — no new test helper beyond the `frame_builder_close_reason_scratch_grow` accessor.

#### N5 RecordedRenderTrapsOrTris payload (full field listing — codex round-8 expansion)

The earlier draft underspecified this variant. Replaying the legacy composite stage requires every input the legacy code resolves at append time. The struct (named fields, all `pub(crate)`):

```rust
pub(crate) struct RecordedRenderTrapsOrTris {
    // Dst identity and layout (per N1 / B.2).
    pub dst_id: DrawableId,
    pub dst_old_layout: vk::ImageLayout,
    pub dst_extent: vk::Extent2D,
    pub dst_format: vk::Format,       // pipeline cache key
    pub dst_has_alpha: bool,          // pipeline cache key + readback view selection
                                       // (from `dst_has_alpha_for_pict_format(dst_format, dst_depth, dst_pict_format)`
                                       // at engine.rs:7261)

    // Composite operator + derived gates (engine.rs:7262-7472).
    pub std_op: StdPictOp,            // from `StdPictOp::from_u8(op_byte)`; pipeline cache key +
                                       //   drives `needs_dst_readback = std_op.needs_dst_readback()`.
    pub op_byte: u8,                  // raw X RENDER PictOp byte. Recorded SEPARATELY from std_op
                                       //   because `needs_full_dst = matches!(op_byte, 0|1|5|6|7|10|13|
                                       //   16..=27|32..=43)` (engine.rs:7472) is a byte-pattern test,
                                       //   not a StdPictOp method. Codex round-9 catch — earlier
                                       //   drafts said "derive from std_op" but no such method exists.
                                       //   Alternative: add a `StdPictOp::needs_full_dst() -> bool` helper
                                       //   in vk/render_pipeline.rs and only record std_op. Either is
                                       //   acceptable; the load-bearing requirement is that emit has
                                       //   a single deterministic source of truth for the bit set.

    // Source kind + per-kind data.
    pub src_kind: RecordedTrapSrcKind, // Drawable { id, swizzle_class } / Solid(color) / Gradient(xid)
                                       //   - Drawable: swizzle_class snapshot from
                                       //     `swizzle_class_for_pict_format(info.format, info.depth, src_pict_format)`
                                       //     (engine.rs:7360) — pinned at append since dst_pict_format /
                                       //     src_pict_format can change between append and emit if a
                                       //     RenderPictFormat resize lands.
                                       //   - Solid: clear color snapshot → emit calls `record_solid_color_clear`.
                                       //   - Gradient: xid → emit looks up `picture_paint.get(xid)` fresh
                                       //     (gradients are CPU-immutable per B.2 R3 finding 9), AND
                                       //     `intrinsic_axis_projection: AffineXform` snapped here at append.

    // CompositeAttrs inputs (engine.rs:7680-7703).
    pub src_is_synthetic_1x1: bool,   // drives REPEAT_PAD override (xeyes pupil fix)
    pub src_repeat: u32,              // from `repeat_to_shader_const(src_repeat)` (legacy enum)
    pub mask_repeat: u32,             // == REPEAT_NONE for the trap path
    pub src_force_opaque: bool,       // from `resolve_force_opaque_pict_format(store, &src, src_pict_format)`
    pub mask_force_opaque: bool,      // == false for the trap path
    pub user_src_xform: AffineXform,  // from `pixman_transform_to_affine(src_transform, src_extent)`

    // Trap raster phase inputs (engine.rs:7531-7647).
    pub prim_kind: TrapPrimKind,      // Trapezoid | Triangle
    pub bbox_x: i32, pub bbox_y: i32, // mask render bbox (origin)
    pub bbox_w: u32, pub bbox_h: u32, // mask render bbox (size)
    pub instance_count: u32,          // edge count for cmd_draw

    // Composite phase rect layout (engine.rs:7473-7480, 7705-7714).
    // The render-dst-rect / mask-offset selection that depends on
    // `needs_full_dst` is computed at emit (derived from `std_op`).
    // The clip_scissors are pre-clamped at append, mirroring legacy.
    pub clip_scissors: Vec<vk::Rect2D>, // X RENDER picture clip; if Some(rects), each
                                          // is clamped to dst_extent at append (engine.rs:7484-7525).
                                          // If empty after clamp, append returns early without recording.

    // Pinned resources.
    pub vertex_pool_pin: Arc<StagingBuffer>, // trap/tri vertex buffer — engine-allocated at append,
                                              // pinned through frame fence via open.pins.pin_staging.
}
```

Additionally:
- **dst_readback view identity** is NOT a field — `engine.dst_readback` is engine-owned mutable state (parallel to `mask_scratch`); emit re-resolves `dst_readback.view(dst_format, dst_has_alpha)` fresh from current engine state (mirror engine.rs:7425). If the readback view's underlying image grew between append and emit, the GROWN-AWAY backing routes through BatchResource (engine.rs:7420-7424 `ensure...returning_old`), same pattern as mask_scratch. Append-side MUST call `ensure_dst_readback_returning_old` and `adopt_retired_resource_for_gpu_retirement` when `std_op.needs_dst_readback()` is true, BEFORE op-append.

- **Pipeline + descriptor** are also NOT recorded — emit looks up the composite pipeline fresh: `render_pipelines.get(std_op, dst_format, dst_has_alpha, /*component_alpha=*/false)` (engine.rs:7437-7443). Descriptor is allocated at emit-time from the open frame's `frame_generation`-tagged descriptor pool ring (B.2 Mechanism 2), bound to the fresh src/mask/dst_readback views.

- **Mask scratch identity** is NOT a field per N5's existing rule — emit re-resolves `engine.mask_scratch` fresh.

Skipping any of the listed fields would cause silent wrong rendering: wrong blend pipeline (missing `std_op`), wrong alpha policy (missing `dst_has_alpha`), wrong drawable view swizzle (missing `swizzle_class`), skipped picture clipping (missing `clip_scissors`), or wrong readback when the dst format changed between append and emit (missing `dst_has_alpha` snapshot).

### N6 — logic_fill uses its own pipeline cache (separate sub-shape inside FILL family)

(Surfaced by codex round 2.) `logic_fill` doesn't share `fill_rect`'s recording shape. It uses `LogicFillPipelineCache` (engine.rs:2453, 2530) and records a logic-op render pass with push constants directly — NO descriptor set traffic (engine.rs:2593-2697).

This means `logic_fill` and `emit_recorded_logic_fill_into_cb` are structurally distinct from the fill_rect path even though they're in the same FILL family. The recording shape (matching the live legacy path at engine.rs:2562-2697):

```
on append (logic_fill):
  // empty-input fast-path returns + renderer_failed check happen BEFORE this point
  flush_render_batch    // N9 entry rule (was flush_cow_batch + flush_render_batch
                        // in legacy at engine.rs:2509-2511; flush_cow_batch removed
                        // when pending_cow_batch is deleted in the cow_copy_area task)
  ensure logic-fill assets
  clamp each rect to dst extent
  // opaque_alpha is a CALLER-provided parameter to logic_fill (GC state) —
  // passed through verbatim, not derived. Part of the cache key (codex round-8).
  pipeline = LogicFillPipelineCache::get(logic_mode, opaque_alpha)
  resolve dst_old_layout via current_layout_for_drawable
  append RecordedOp::LogicFill { logic_mode, opaque_alpha, dst_format, color,
                                 rects-clamped, dst_id, dst_extent, dst_old_layout }
  (emit re-resolves pipeline fresh via LogicFillPipelineCache::get(logic_mode, opaque_alpha))

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

Decision: `image_text` is its own GLYPH family (conceptual grouping; no separate gate per the rip-and-replace style). Recording:

```
on append (image_text):
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

**Atlas transactional discipline (LOAD-BEARING — codex round-6 catch).** The B.1 `composite_glyphs_via_frame_builder` path is not just "pin staging + append" — it carries a transactional state contract that `image_text` MUST also honor:

1. **First-touch snapshot.** On the first atlas-touching op-append within an open frame, snapshot the atlas's pre-frame state: `atlas_prev_ticket_snapshot = atlas.last_render_ticket().cloned()` (engine.rs:5314-style) AND `open.layouts.first_touch_atlas(atlas.current_layout())` to seed the overlay's atlas entry.

2. **Close-success commit.** `commit_close_success` already stamps the atlas's `last_render_ticket` to the closed frame's ticket and commits the overlay's atlas layout to `V2GlyphAtlas::current_layout` (engine.rs:8780-style). image_text's port must NOT bypass this — the existing close-success path handles atlas commits for any frame that touched the atlas, regardless of whether composite_glyphs, image_text, or both touched it.

3. **Close-failure rollback.** On close failure, the atlas's `last_render_ticket` rolls back to `atlas_prev_ticket_snapshot` and the atlas's layout rolls back to its pre-frame value (engine.rs:8838-style — `rollback_atlas`). image_text's port relies on the existing rollback path — no new rollback code, but its append MUST register first-touch correctly so the rollback knows what to restore.

Without (1)+(3), a failed close after an image_text frame would leak a stale `last_render_ticket` on the atlas (pointing at a ticket that never signals) — a B.2-class transaction bug. The remediation is purely append-side: ensure `image_text` calls the same `first_touch_atlas` snapshot helper that `composite_glyphs_via_frame_builder` does, on its first atlas-touching op. No new helper needed; the existing one is the contract.

The audit comparison: B.1's `composite_glyphs_via_frame_builder` (engine.rs:5605-5618) already does this exact dance for the X RENDER composite_glyphs path. image_text's body is mechanically the same with a different draw call at emit-time (text run vs composite-glyphs).

**Target-format gate (LOAD-BEARING — codex round-7 catch).** The pre-B.3 `image_text` drops the run unless the dst format is `vk::Format::B8G8R8A8_UNORM` (engine.rs:4515-4526). The text pipeline is built only for that format; depth-1/8 storages (and any non-BGRA8) cannot be text-run targets. The B.3 rewrite MUST preserve this gate APPEND-SIDE inside the new `image_text` body:

```rust
let target_format = store.get(target).expect("checked").storage.format;
if target_format != vk::Format::B8G8R8A8_UNORM {
    log::warn!("v2 image_text (frame_builder): target format {target_format:?} unsupported — dropping run");
    return Ok(stats);  // identical short-circuit as legacy
}
```

The gate fires BEFORE any atlas first-touch snapshot, any glyph upload, any op append — there is no rollback path needed because nothing was recorded. The recorded `RecordedImageText` variant implicitly carries the invariant `dst_format == B8G8R8A8_UNORM` (validated append-side). Phase 5's integration test SHOULD include a non-BGRA8 target (depth-1 or R8) negative-test verifying the run is dropped without atlas mutation. (Depth-24 and depth-32 ARGB targets both map to `B8G8R8A8_UNORM` — those PASS the gate; codex round-10 caught a wording error in earlier drafts that said "depth-32 should drop".)

### N8 — Live concrete-scratch ownership model (single source of truth on RecordedCopyArea)

(Surfaced by codex round-7, **unified by codex round-8 catch** of split-brain ownership.) The frame builder has two distinct pin lifetimes for non-drawable images:

1. **B.2 BatchResource three-tier routing** — `Box<dyn BatchResource>` slots for *grown-away old backings* (e.g., mask_scratch's previous extent after a peek-grow). The new backing replaces engine state; the old backing routes via `adopt_retired_resource_for_gpu_retirement` (engine.rs:756-786) to one of three tiers: (a) open frame's `open.pins.adopt_retired`, (b) newest in-flight `submitted.back_mut().append_retired_scratch`, or (c) immediate `boxed.release(&vk)` if nothing in flight. Used for grow events.

2. **LIVE concrete-scratch slot** (this invariant) — a per-op one-shot scratch image that the recorded op uses DURING its emit and that must remain addressable until the owning `SubmittedOp` retires. This is `copy_area`'s self-overlap scratch (engine.rs:225-229, 291-310, 2814-2918). Concrete RAII type (`ScratchImage` with `Drop` that calls `destroy_image` + `free_memory`), NOT a `Box<dyn BatchResource>`. Used for per-op temporary resources.

The two coexist. The legacy `SubmittedOp` struct already has both slots — `scratch: Option<ScratchImage>` (case 2) AND `retired_resources: Vec<Box<dyn BatchResource>>` (case 1). The B.3 frame-builder port preserves both slots without conflation.

**Single ownership model (LOAD-BEARING).** The scratch lives in EXACTLY ONE place at any point in time. There is NO sibling `OpenFrame::live_scratches` collector slot. The states form a chain:

1. **Pre-append** — owned by `allocate_scratch_image`'s return value (a local).
2. **From append until close** — owned by `RecordedCopyArea::self_overlap_scratch: Option<ScratchImage>` inside the variant payload, inside `OpenFrame::ops`. (Sole owner during emit too — emit pattern-matches against `RecordedOp::CopyArea(payload)` and reads `payload.self_overlap_scratch.as_ref().expect("self-overlap path").image`.)
3. **At close success, BEFORE `flush_submit_group`** — close path walks `open_frame.ops` and **takes** every `self_overlap_scratch` (`std::mem::take`), collecting them into a local `Vec<ScratchImage>`. After the walk, the SubmittedOp is constructed with `scratch: scratches_taken_from_ops` (not `None`). engine.rs:1459-1466's `SubmittedOp { ..., scratch: None, ... }` literal MUST be edited in Task 1 to take a `scratch: Vec<ScratchImage>` parameter; the existing pre-B.3 callers pass `Vec::new()`.
4. **From close success until fence retire** — owned by `SubmittedOp::scratch: Vec<ScratchImage>` (slot renamed from `Option<ScratchImage>` to `Vec<ScratchImage>` — one-field structural extension; legacy callers updated to push 0–1 elements; B.3 self-overlap pushes 1 per op).
5. **At fence retire** — `SubmittedOp::scratch` drains, each `ScratchImage::Drop` calls `destroy_image` + `free_memory`. No other consumer holds a reference; the GPU is done.

The earlier draft of this spec described an `OpenFrame::live_scratches` sibling collector — **that was wrong** (codex round-8 catch). It would have been a second copy of the ownership state, contradicting the on-`RecordedCopyArea` slot. The on-`RecordedCopyArea` slot is the sole source of truth.

**Close-failure rollback.** If close fails BEFORE step (3) (e.g., a panic during prelude work), the scratch sits in `OpenFrame::ops` and drops cleanly when the OpenFrame drops (no `SubmittedOp` was pushed, no fence ticket exists, `ScratchImage::Drop` is immediately safe). If close fails AT step (3) BUT BEFORE `flush_submit_group` succeeds (e.g., CB end fails), the scratches sit in `Vec<ScratchImage>` local on the stack; same Drop semantics. If close fails AFTER `flush_submit_group` (which constructs the SubmittedOp and pushes it onto `pending_group_ops`), then the SubmittedOp exists with its scratches AND a real fence ticket; either the fence eventually signals (normal retire path) or `drain_all` runs on shutdown (which calls `BatchResource::release` for grown-away resources and `Drop` for scratches — both safe).

**Allocation timing — ordering rule (codex round-8 catch).** The rewritten `copy_area` body MUST allocate the scratch BEFORE any open-frame state mutation. Specifically the body order is:

```
1. The rewritten body does NOT call close_open_frame_for_non_ported_op; it extends the open frame.
2. preflight checks (renderer_failed, store.get(src), store.get(dst), self-overlap detection).
3. if src_id == dst_id { allocate_scratch_image(...)? }  ← may return Err; no mutation yet, safe early return.
4. now do prelude state mutations (touched.first_touch dst+src, layouts.first_touch_drawable dst+src,
   store.touch_render_fence dst+src).
5. push_op_and_set_layouts(RecordedCopyArea { ..., self_overlap_scratch: Some(scratch), ... }).
```

If `allocate_scratch_image` fails at step (3), nothing in the open frame has been touched — the body returns `Err(RenderError::Vk(...))` and the caller's existing renderer_failed handling closes the open frame on its terms (a non-recoverable error promotes to fatal-after-failure per B.2 M1). No partial-mutation rollback is needed because there was no partial mutation.

**Why allocation-first.** Reversing the order (mutate first, then allocate) means a transient memory-pressure failure at allocation time leaves the frame mid-mutated: the dst and src have updated touched/layout snapshots, but no op was appended to match. The next op in the same frame would observe stale snapshots that disagree with the actual recorded ops; close-success commit-writeback would commit a layout that no recorded op transitioned to. This is exactly the bug class codex round-8 flagged.

### N9 — Every B.3 body MUST flush pending_render_batch at entry; pending_cow_batch is deleted

(Codex round-10 catch; cow_copy_area resolution in round-12.) Every pre-B.3 paint body starts with `flush_cow_batch` + `flush_render_batch` calls to preserve chronological X11 order. After B.3, `pending_cow_batch` is DELETED entirely (its sole enqueuer, `cow_copy_area`, is ported to the frame builder), so `flush_cow_batch` and its call sites also vanish. `pending_render_batch` (`flush_render_batch`, engine.rs:3982) still exists post-B.3 — it carries deferred ops from `render_composite_legacy` (B.2's kept kill-switch fallback at engine.rs:5877), and lives until B.5 deletes that fallback.

The N9 invariant after B.3 reads:

> Every B.3 rewritten body MUST call `flush_render_batch` at entry, immediately after empty/no-op fast-path returns (per the round-12 placement clarification below). `pending_cow_batch` no longer exists — there is no flush_cow_batch to call.

Confirmed legacy call sites that informed this rule (mandatory pattern source — codex audit):
- `fill_rect_batch` — engine.rs:2286-2290 (drops flush_cow_batch line, keeps flush_render_batch)
- `logic_fill` — engine.rs:2509-2511 (same)
- `copy_area` — engine.rs:2751-2753 (same)
- `put_image` — engine.rs:4144-4146 (same)
- `image_text` — engine.rs:4503-4505 (same)
- `render_traps_or_tris` — engine.rs:7224-7226 (same)
- B.1's `composite_glyphs_via_frame_builder` — engine.rs:5205-5214 (proves the frame-builder path also needs the flush, NOT just the legacy paths)

**Why pending_cow_batch can be deleted.** Pre-B.3, `cow_copy_area` enqueued into `pending_cow_batch` to collapse same-dst cow_copy_areas into one CB. The pre-B.3 frame builder couldn't see cow_copy_areas because they bypassed it entirely. In B.3, `cow_copy_area` becomes a normal `RecordedCopyArea` append — the frame builder's own coalescing (multiple ops per frame collapse into one CB at close) replaces the pending_cow_batch's same-dst collapse. Multiple cow_copy_areas in one frame still produce one CB; cross-frame cow_copy_areas naturally split per close trigger.

The pre-B.3 stale-dst flush behavior (engine.rs:3168, 3196, 3412 — flush when the next cow_copy_area targets a different dst) is also subsumed: the frame builder's overlay-resolved layout tracking handles per-drawable state correctly across different `dst_id`s within a single frame, with one terminal SHADER_READ_ONLY_OPTIMAL per drawable per frame (per N1's single-terminal rule).

**Empty/no-op fast-path placement (LOAD-BEARING — codex round-12 catch).** The flush call goes AFTER the empty/zero-rects fast-path returns but BEFORE any open-frame mutation. Legacy precedent at multiple sites:
- `copy_area` returns early on empty rect BEFORE flush (engine.rs:2748).
- `logic_fill` returns early on empty rects BEFORE flush (engine.rs:2506).
- `image_text` returns early on empty glyph list BEFORE flush (engine.rs:4500).
- `cow_copy_area` returns early on empty rect BEFORE flush (engine.rs:3092).

Skipping the fast-path placement (i.e., flushing first regardless of input) would force pending_render_batch to drain on every empty call, changing batching cadence and adding latency to no-op paint ops. The body order is therefore: empty-input fast-path return → renderer_failed check → flush_render_batch → preflight / allocation / mutation per N8 → push_op_and_set_layouts.

The flush call can itself close an open frame (since `flush_render_batch` may submit a CB), so it precedes the open-frame mutation just like `allocate_scratch_image` does in N8.

### N10 — COW PRESENT-completion attachment migrates to OpenFrame

(Codex round-13 catch.) `PendingCowBatch` carries TWO load-bearing slots:
1. The cow_copy_area enqueue queue (cb, ticket, dst, srcs, damage, count) — replaced by frame-builder per-frame collapse per N9.
2. `present_completions: Vec<PendingPresentEntry>` — X11 PRESENT extension completion attachments, set by `attach_cow_present_completion` (engine.rs:3581) and drained at flush time into `pending_present_batches` (engine.rs:3497-3567).

Deleting `pending_cow_batch` without migrating (2) would break PRESENT-completion attachment for COW presents. The frame builder MUST replace this attachment surface.

**Migration design (load-bearing).** All three sub-fixes here address codex round-14 findings.

1. **New slot on OpenFrame.** Add `OpenFrame::pending_present_completions: Vec<PendingPresentEntry>` (defaults empty, no per-cow_id keying — all entries share the open frame's ticket at close. Codex round-14 confirmed multi-cow grouping on one frame ticket is fine).

2. **Rewrite `attach_cow_present_completion(cow_id, entry)` — predicate fix (codex round-14 MEDIUM #1):**
   ```
   pub(crate) fn attach_cow_present_completion(
       &mut self,
       cow_id: DrawableId,
       entry: PendingPresentEntry,
   ) -> Result<(), PendingPresentEntry> {
       let Some(inner) = self.inner.as_mut() else { return Err(entry); };
       let Some(open) = inner.frame_builder.open.as_mut() else { return Err(entry); };
       // Predicate: this frame WRITES to cow_id (not just reads/samples).
       // Legacy used exact `batch.dst == cow_id`; that semantic is preserved by
       // walking open.ops and matching any variant whose dst_id == cow_id.
       // (`open.touched` is the WRONG predicate — it's a first-touch snapshot
       // that includes drawables sampled-only, codex round-14 catch.)
       let writes_to_cow = open.ops.iter().any(|op| op.dst_id() == Some(cow_id));
       if !writes_to_cow {
           return Err(entry);
       }
       open.pending_present_completions.push(entry);
       Ok(())
   }
   ```
   The body relies on a new helper `RecordedOp::dst_id(&self) -> Option<DrawableId>` returning the dst for write-variants and `None` for utility variants without a writable dst. The match arms cover EVERY variant in the enum (frame_builder.rs:538 + the B.3 additions — codex round-15 catch on completeness):

   - `RecordedOp::CompositeGlyphs(g)` → `Some(g.dst_id)` — B.1 variant; writes to the dst drawable.
   - `RecordedOp::RenderComposite(rc)` → `Some(rc.dst_id)` — B.2 variant; writes to the dst drawable.
   - `RecordedOp::GlyphUpload(_)` → `None` — B.1 utility variant; writes to the atlas, not a drawable.
   - `RecordedOp::LayoutTransition(_)` → `None` — B.2+ utility variant; not a paint op.
   - `RecordedOp::CopyArea(ca)` → `Some(ca.dst_id)` — B.3 variant covering both copy_area and cow_copy_area.
   - `RecordedOp::PutImage(pi)` → `Some(pi.dst_id)`.
   - `RecordedOp::FillRect(fr)` → `Some(fr.dst_id)`.
   - `RecordedOp::LogicFill(lf)` → `Some(lf.dst_id)`.
   - `RecordedOp::ImageText(it)` → `Some(it.dst_id)`.
   - `RecordedOp::RenderTrapsOrTris(rt)` → `Some(rt.dst_id)`.

   The backend (backend.rs:9904) call site is unchanged — the helper's signature is preserved; only the body changes.

3. **Close-success transfer — mirror legacy precisely INCLUDING signal-on-submit ordering (codex rounds 14+15).** The completion semaphore MUST be acquired BEFORE the frame submit and its `vk::Semaphore` MUST be threaded into the submit's signal-on-completion list — otherwise the exported sync_file fd will never fire (codex round-15 catch: the round-14 draft only handled post-flush_submit_group export work, which would create a semaphore that's never queued for signaling).

   The actual API (per `crates/yserver/src/kms/v2/present_completion.rs:29-52`) is:
   - `PresentBatchWait::{Fd(OwnedFd), Ready, Poll}`.
   - `PendingPresentBatch { wait, ticket: Option<FenceTicket>, signal: Option<PresentCompletionSignal>, events }`.

   Legacy `flush_cow_batch` order (engine.rs:3467-3554):
   - 3467: `let completion_signal = if batch.present_completions.is_empty() { None } else { Some(platform.acquire_present_completion_signal()?) };`
   - 3472: `let completion_semaphore = completion_signal.as_ref().map(|s| s.semaphore());`
   - 3477: `end_and_submit_op_with_signal(inner, platform, cb, &batch.ticket, completion_semaphore)?;` — semaphore is attached to the SubmitGroup append at this point.
   - 3522-3555: After `flush_submit_group` succeeds, export the sync_file fd from `completion_signal`, then build PendingPresentBatch with `wait/ticket/signal` and push.

   B.3 close_open_frame MUST do the same order. The current close_open_frame at engine.rs:1408 calls `end_and_submit_op(inner, platform, cb, &frame_ticket)` (the no-signal wrapper at engine.rs:8404 which delegates to `end_and_submit_op_with_signal(..., None)`). The B.3 rewrite changes that to:

   ```
   // BEFORE end_and_submit_op (engine.rs:~1405-1408):
   let completion_signal: Option<PresentCompletionSignal> = if open_frame.pending_present_completions.is_empty() {
       None
   } else {
       Some(platform.acquire_present_completion_signal()?)
   };
   let completion_semaphore = completion_signal.as_ref().map(|s| s.semaphore());

   // end_and_submit_op_with_signal threads the semaphore into the SubmitGroup
   // append; the GPU submit will signal it on completion.
   end_and_submit_op_with_signal(inner, platform, cb, &frame_ticket, completion_semaphore)?;

   // ... pending_group_ops.push(SubmittedOp { ... }) as before ...

   // Drive flush_submit_group as before.
   let flush_outcome = self.flush_submit_group(platform, FlushReason::FrameBuilder);

   match flush_outcome {
       Ok(_) => {
           // ... commit_close_success path ...
           // NEW: drain pending_present_completions into PendingPresentBatch.
           if !open_frame.pending_present_completions.is_empty() {
               let (wait, signal) = match completion_signal {
                   Some(signal) => match signal.export_sync_file_fd() {
                       Ok(Some(fd)) => (PresentBatchWait::Fd(fd), Some(signal)),
                       Ok(None)     => (PresentBatchWait::Ready, Some(signal)),
                       Err(_e)      => (PresentBatchWait::Poll, Some(signal)),
                   },
                   None => unreachable!("non-empty completions but no signal allocated"),
               };
               let ticket = inner.submitted.back().map(|op| op.ticket.clone());
               inner.pending_present_batches.push(PendingPresentBatch {
                   wait,
                   ticket,
                   signal,
                   events: std::mem::take(&mut open_frame.pending_present_completions),
               });
           }
       }
       Err(e) => { /* see step 4 — force-enqueue degraded batch */ }
   }
   ```

   The `signal: Option<PresentCompletionSignal>` field MUST be stored on the PendingPresentBatch even when its sync_file fd has been exported — the backend explicitly keeps the semaphore alive until both the sync primitive is ready AND the batch ticket signals (backend.rs:2334). Destroying the semaphore earlier is a use-after-free hazard.

   If `acquire_present_completion_signal()?` fails BEFORE `end_and_submit_op_with_signal`, the cb has ALREADY been allocated and recorded by that point — so the failure path is the existing **append-failure rollback** at engine.rs:1410-1446 (NOT the CB-allocation-failure path). That rollback's exact sequence must run, in order:

   1. Free the recorded command buffer from `platform.ops_command_pool_handle()` (engine.rs:1413-1416 — `device.free_command_buffers(pool, &[cb])`).
   2. `rollback_pre_submit(store, &mut open_frame)` to revert any drawable touched/ticket-touch state.
   3. `platform.renderer_failed = true` (fatal-after-failure per B.2 M1).
   4. `rollback_atlas(inner_post, open_frame.layouts.atlas, open_frame.atlas_prev_ticket_snapshot)`.
   5. Drain & release `open_frame.pins.retired_resources` via `r.release(&inner_post.vk)`.
   6. Push a `FrameCloseEvent` with `aborted: true`.
   7. `inner_post.frame_builder.complete_close_failure()`.
   8. Return `Err(e)`.

   The events drop with the OpenFrame (no `PendingPresentBatch` is force-enqueued because no submit was attempted — the X PRESENT protocol observes them as never delivered, same outcome as a partial pre-submit failure pre-B.3). The `completion_signal` (if it was partially allocated before the failing inner step) is dropped too. The B.3 implementer MUST add the signal-acquisition step BETWEEN the existing CB-recording success and `end_and_submit_op_with_signal`, route any Err through this same append-failure rollback block, and NOT skip any of the 8 cleanup steps above.

   For an `end_and_submit_op_with_signal` failure (same body the existing `end_and_submit_op` failure used pre-N10 — line 1410's check): same 8-step rollback applies. The signal allocated in the preceding step drops with the SubmittedOp not being created.

4. **Close-failure semantics — force-enqueue, NOT silent drop (codex round-14 MEDIUM #2).** If `flush_submit_group` FAILS (post-`end_and_submit_op_with_signal`), the legacy `flush_cow_batch` at engine.rs:3556-3567 does NOT silently drop the completions. It force-enqueues a degraded `PendingPresentBatch` with `wait: PresentBatchWait::Ready, ticket: None, signal: None, events: present_completions` and STILL returns Err. The X PRESENT protocol observes the events as delivered (with "Ready" wait), and the caller observes the Err for separate error handling. The B.3 close_open_frame's flush_submit_group-Err branch MUST mirror this: take the open frame's pending_present_completions, build a `PendingPresentBatch { wait: Ready, ticket: None, signal: None, events: completions }` (the `completion_signal` allocated in step 3 is dropped — it was queued on a submit that failed), push onto `pending_present_batches`, THEN return Err. Silent drop would be a protocol-visible regression. NOTE: this branch is reached only when `flush_submit_group` itself fails AFTER `end_and_submit_op_with_signal` succeeded; failures earlier in close_open_frame (CB allocation, end_and_submit_op_with_signal failure, acquire_present_completion_signal failure) route through the existing rollback path and DO drop the events — matching pre-B.3 behavior for the same failure modes (engine.rs:1410-1446's rollback path).

5. **Test helper.** `has_pending_batches_for_tests` (engine.rs:1717-1722) currently checks `pending_cow_batch.is_some() || pending_render_batch.is_some()`. Replace with `frame_builder.open.is_some() || pending_render_batch.is_some()`. Behavioral meaning is preserved: "is there any in-flight work the frame builder hasn't committed yet?".

The cow_copy_area task in Phase 2 covers the full deletion + migration as one atomic commit (per N9's atomicity requirement — partial deletion leaves dangling references that don't compile).

## Frame close triggers after B.3

Pre-B.3 close-reason histogram has 9 triggers (per B.2 Task 1's `CloseReason::ScratchGrow` addition):

```
scene_compose, non_ported, legacy_sc, present_completion, sync_wait,
timeout, shutdown, pin_ceiling, scratch_grow
```

After B.3 rip-and-replace:
- `non_ported` retains a single residual caller: `render_composite_legacy` (engine.rs:5877) under the B.2 kill-switch. All 8 B.3 ops no longer call `close_open_frame_for_non_ported_op` — they extend the open frame instead.
- All other triggers unchanged.
- No per-source breakdown needed: the only remaining `non_ported` source is `render_composite_legacy`, which is itself behind an explicit kill-switch (so non-zero counts are a deliberate opt-in signal, not a port-coverage gap).

`CloseReason::NonPortedPaintOp` and the `close_open_frame_for_non_ported_op` helper both STAY through all of B.3 — they're still reachable from `render_composite_legacy`. B.5 deletes both when `render_composite_legacy` itself is removed.

## Task structure (~19 tasks, 6 phases)

(Earlier drafts had ~39 tasks across 7 phases under the gated-rollout pattern. The rip-and-replace style elides Phase 0 telemetry-breakdown work, the per-op dispatch-split / drop-M2-close subtasks, and the per-family flip-gates wrap-up — roughly half the original tasks.)

### Phase 1 — Foundation (1 task)

**Task 1: New RecordedOp variants + SubmittedOp::scratch slot extension + close-path scratch walk.**
- Add `RecordedCopyArea`, `RecordedPutImage`, `RecordedFillRect`, `RecordedLogicFill`, `RecordedImageText`, `RecordedRenderTrapsOrTris` struct stubs + enum variants. (No `RecordedFillRectBatch` — per N4, `fill_rect_batch` produces one `RecordedFillRect` per call with N≥1 rects.) Per N5 full payload listing, `RecordedRenderTrapsOrTris` includes a `RecordedTrapSrcKind` enum (Drawable { id, swizzle_class } / Solid(color) / Gradient { xid, intrinsic_axis_projection }).
- Box-wrap each variant.
- Size-budget tests per variant.
- Stub `unimplemented!()` arms in `emit_recorded_op_into_cb` for each new variant (filled out by the per-op rewrites in Phases 2-5).
- **Per N8: rename `SubmittedOp::scratch: Option<ScratchImage>` to `SubmittedOp::scratch: Vec<ScratchImage>`** (engine.rs:225-229). Update the existing single in-tree caller — the immediate-op self-overlap copy_area site at engine.rs:2920 — to push a single-element Vec. All other current `SubmittedOp { ..., scratch: None, ... }` literals become `scratch: Vec::new()`.
- **Per N8: close-path scratch walk.** Extend the close-frame helper around engine.rs:1459 to walk `open_frame.ops` BEFORE constructing the `SubmittedOp`, `std::mem::take`ing each `RecordedCopyArea::self_overlap_scratch: Option<ScratchImage>` into a local `Vec<ScratchImage>`. Pass that vec to the `SubmittedOp` literal. In Task 1 the walk yields an empty vec (no rewritten op produces scratch yet — that arrives with the Phase 2 copy_area rewrite). The walk happens BEFORE `flush_submit_group` so close-failure drops the local vec on the stack — scratches die cleanly via `ScratchImage::Drop`.
- **v1 compatibility fallout.** If renaming `SubmittedOp::scratch` breaks the v1 backend compile, apply the bare-minimum patch (e.g., `Vec::new()` literal at the v1 call site). Do NOT preserve v1 behavior beyond compilation; v1 is scheduled for removal in a later phase.

(No new `OpenFrame` field — cow_copy_area uses the existing drawable overlay machinery per N3, and copy_area self-overlap scratch lives on its `RecordedCopyArea` payload per N8.)
(image_text reuses B.1's `RecordedOp::GlyphUpload` variant verbatim per N7 — no new GLYPH-upload variant needed.)

### Phase 2 — TRANSFER family (6 tasks: 2 per op × 3 ops)

**For each of `copy_area`, `cow_copy_area`, `put_image`:**

- **Body rewrite.** Replace the existing direct-submit body of `<op>` in-place. The new body, in order: (1) Empty/no-op fast-path return (e.g., zero rects, empty staging buffer). (2) `renderer_failed` check. (3) **`flush_render_batch` per N9** (no `flush_cow_batch` — `pending_cow_batch` is being deleted in B.3; see N9 + the cow_copy_area task below). (4) Preflight (dst metadata resolve, src/dst overlay first_touch for any Drawable inputs, ticket-touch src+dst). (5) For `put_image`, clone the staging Arc into `frame_builder.open.pins.staging_buffers` (per N2). (6) For `copy_area`'s self-overlap subcase, allocate the scratch FIRST per N8's allocation-ordering rule. (7) Append `RecordedOp::<variant>` via `push_op_and_set_layouts`. (8) Implement `emit_recorded_<op>_into_cb`: barriers + transfer + barriers-back per N1's mandated stage/access masks. Layout normalization to `SHADER_READ_ONLY_OPTIMAL` per N1.
- **Integration test.** Two-call collapse test (`v2_frame_builder_<op>_collapses_two_in_one_frame`) — assert two consecutive `<op>` calls in the same frame produce one SubmittedOp, not two.

`cow_copy_area`'s body task is ATOMIC — it rewrites cow_copy_area, deletes the `pending_cow_batch` infrastructure, AND migrates the PRESENT-completion attachment surface per N10. All in one commit because partial deletion leaves dangling references that don't compile.

Steps:
1. Rewrite the `cow_copy_area` body to append a `RecordedCopyArea` to the frame builder (same shape as `copy_area`'s body, resolving the cow Drawable via the existing `current_layout_for_drawable(store, cow_id)` accessor per N3 — no special cow handling otherwise).
2. **Add `OpenFrame::pending_present_completions: Vec<PendingPresentEntry>` slot (per N10).**
3. **Rewrite `attach_cow_present_completion(cow_id, entry)` body to push into `frame_builder.open.pending_present_completions` when the open frame has any op touching `cow_id` (per N10). Helper signature is unchanged — backend call site at backend.rs:9904 stays as-is.**
4. **Extend `close_open_frame` with FULL three-branch PRESENT plumbing (per N10):**
   - **(a) Pre-submit**: BEFORE `end_and_submit_op` (engine.rs:1408), acquire `PresentCompletionSignal` if `open_frame.pending_present_completions` is non-empty, extract its `vk::Semaphore`, and call `end_and_submit_op_with_signal` instead of `end_and_submit_op`. If signal-acquire fails or the with_signal call fails, route through the existing append-failure rollback at engine.rs:1410-1446 (8 cleanup steps — events drop with OpenFrame, mirroring pre-B.3 pre-submit failure behavior).
   - **(b) Post-`flush_submit_group` SUCCESS**: drain `pending_present_completions` into a new `PendingPresentBatch { wait, ticket, signal, events }` on `pending_present_batches`. `wait` derives from `signal.export_sync_file_fd()` per legacy at engine.rs:3534-3555 (Fd/Ready/Poll).
   - **(c) Post-`flush_submit_group` FAILURE (load-bearing — codex round-14 catch)**: even though flush_submit_group failed, the events are X PRESENT protocol-visible — they MUST be force-enqueued as a degraded `PendingPresentBatch { wait: PresentBatchWait::Ready, ticket: None, signal: None, events: drained_completions }` BEFORE returning Err. The `completion_signal` (allocated in step a, queued on a submit that failed) drops with the local variable. Mirrors legacy at engine.rs:3556-3567.
5. Delete the `pending_cow_batch` field from `EngineInner` (and any helper structs).
6. Delete the `flush_cow_batch` helper function.
7. Delete every `flush_cow_batch(store, platform)?` call site in `engine.rs` (the 6 other ops being rewritten will already have these lines dropped as part of their N9 entry-flush replacement; the backend's fallback site at backend.rs:9920 — outside the attach helper — also needs the line removed).
8. Delete any cow-batch-specific telemetry, state machines, and stale-dst flush hooks (engine.rs:3168, 3196, 3412 region — all become unreachable).
9. **Update `has_pending_batches_for_tests` (engine.rs:1717-1722) to check `frame_builder.open.is_some() || pending_render_batch.is_some()` (per N10 minor catch).**

The frame builder's per-frame collapse subsumes the pre-B.3 same-dst cow batching: multiple `cow_copy_area` calls with the same dst in one frame all append to the open frame and emit into one CB at close. Different-dst calls within a frame are also handled by the frame builder (each dst tracks its own layout via the overlay per B.2 Task 4).

The cow_copy_area integration test `v2_frame_builder_cow_copy_area_collapses_two_in_one_frame` should assert: (a) two same-dst cow_copy_areas in one frame produce one SubmittedOp; (b) the cow's `storage.current_layout` updates correctly via `commit_close_success`; (c) no `pending_cow_batch`-related symbol survives in the compiled binary (the helper / field / call sites are all gone); (d) **PRESENT-completion attach during a frame containing cow_copy_areas correctly delivers a CompletedPresentEvent when the frame retires (per N10).**

`copy_area`'s body rewrite handles the self-overlap subcase per N8:

1. **Preflight first** — detect `src_id == dst_id`. If self-overlap, call `allocate_scratch_image(...)` BEFORE any open-frame mutation. Propagate any `Err` to the caller; nothing mutated, no rollback needed.
2. **Then prelude state** — `touched.first_touch` dst (and src when distinct), `layouts.first_touch_drawable` dst (and src), `store.touch_render_fence` dst (and src).
3. **Then append** — `push_op_and_set_layouts(RecordedCopyArea { ..., self_overlap_scratch: Some(scratch_or_none), ... }, [(dst_id, SHADER_READ_ONLY_OPTIMAL), (src_id, SHADER_READ_ONLY_OPTIMAL)])`.

The close path's scratch walk (added in Task 1 per N8) `std::mem::take`s each `self_overlap_scratch` into a `Vec<ScratchImage>` and passes it to the `SubmittedOp` literal at engine.rs:1459-1466. By the time Phase 2's copy_area rewrite lands, the walk is already in place.

`put_image`'s body rewrite additionally clones the staging Arc into `frame_builder.open.pins.staging_buffers` (per N2).

The existing `close_open_frame_for_non_ported_op` call at the top of each legacy body is DELETED as part of the rewrite (since the rewritten body no longer needs to close someone else's frame — it extends the open frame instead). The helper itself stays in tree only because `render_composite_legacy` still calls it.

### Phase 3 — FILL family (6 tasks: 2 per op × 3 ops)

**For each of `fill_rect`, `fill_rect_batch`, `logic_fill`:** same 2-task shape — body rewrite + integration test. Each body rewrite MUST call `flush_render_batch` at entry per N9 (after empty-input fast-path returns and renderer_failed check; no `flush_cow_batch` — that infrastructure is deleted in the cow_copy_area task).

The recording shape diverges within the family (codex round-7 correction — earlier drafts routed fill_rect through the composite pipeline; that was wrong):

- `fill_rect` + `fill_rect_batch`: rewrite uses **NEITHER the composite pipeline NOR the LogicFill pipeline** — it uses `cmd_clear_attachments` directly (engine.rs:2330-2410). NO descriptor pool, NO pipeline bind, NO shader, NO blend. REPLACE semantics for the listed rects ONLY (pixels outside the rects within the render area are LOADED unchanged from prior dst contents). Recorded payload: `dst_id` + dst extent/format + color + pre-clamped rect slice + `dst_old_layout`. Emit: pre-barrier dst (`src_access = SHADER_SAMPLED_READ | TRANSFER_WRITE | COLOR_ATTACHMENT_WRITE` mirroring legacy at engine.rs:2333-2348) → COLOR_ATTACHMENT_OPTIMAL, `cmd_begin_rendering(load=LOAD, store=STORE)` (NOT DONT_CARE — see FILL family pseudocode), set viewport+scissor, `cmd_clear_attachments(color, rect_slice)`, end_rendering, post-barrier dst → SHADER_READ_ONLY_OPTIMAL.
- `logic_fill`: rewrite uses `LogicFillPipelineCache` (engine.rs:2453, 2530-2538) — distinct from BOTH the composite pipeline AND `cmd_clear_attachments`. The recorded payload carries `dst_id` + GC logic mode + `dst_format` + `opaque_alpha: bool` (caller-provided GC state, NOT derived — codex round-8 catch) + color + pre-clamped rect slice + dst extent + `dst_old_layout`. Emit records a logic-op render pass with push constants directly via `inner.logic_fill_caches[dst_format].get(logic_mode, opaque_alpha)`; NO descriptor traffic, NO picture-clip input. See N6 for the full barrier shape.

Three distinct emit sub-shapes inside FILL: cmd_clear_attachments (fill_rect/fill_rect_batch), logic_op render pass (logic_fill). The "FILL family" name groups them by op-source-type (all are solid-color fills), not by emit mechanics.

`fill_rect_batch`'s rewrite records ONE `RecordedFillRect` per call carrying the entire rect slice (per N4). Each rewrite deletes its existing `close_open_frame_for_non_ported_op` call.

### Phase 4 — MASK family (2 tasks: render_traps_or_tris only)

**For `render_traps_or_tris`:** same 2-task shape — body rewrite + integration test. The body's responsibilities (per the full N5 RecordedRenderTrapsOrTris payload listing) are:

0. **Flush pending_render_batch** (per N9 — load-bearing X11 chronological-order invariant). After empty-input fast-path returns and renderer_failed check, call `self.flush_render_batch(store, platform)?` BEFORE any open-frame work. No `flush_cow_batch` — pending_cow_batch is deleted in the cow_copy_area task. Mirrors legacy at engine.rs:7224-7226 and B.1's composite_glyphs body at engine.rs:5205-5214 (minus the flush_cow_batch line).
1. Preflight checks (renderer_failed, store.get(dst), `std_op = StdPictOp::from_u8(op)`, dst_format/dst_depth/dst_has_alpha, self-alias gate per engine.rs:7270-7273).
2. Allocate the per-instance vertex buffer (`StagingBuffer::new_with_usage` for VERTEX_BUFFER usage). This is an allocation that may fail — done BEFORE any open-frame mutation, mirroring N8's allocation-first rule.
3. **Peek-grow mask scratch** (Phase 9A from B.2 Task 9 — close-before-grow if frame has prior ops, then `ensure_image_size_returning_old` + `adopt_retired_resource_for_gpu_retirement` adopting the OLD backing as `BatchResource` per N8 case 1).
4. **Peek-grow dst_readback** when `std_op.needs_dst_readback()` (same returning_old + adopt pattern).
5. Resolve append-time-stable fields per N5: src_kind discriminant + Drawable swizzle_class / Gradient intrinsic xform / Solid color, src/mask repeat enums, `src_force_opaque`, `user_src_xform`, prim_kind, bbox, instance_count, dst_format, dst_has_alpha, std_op, raw `op_byte` (for emit's `needs_full_dst` derivation), pre-clamped `clip_scissors` (if `clip_rects.is_some()` and all rects clamp to empty, early-return without recording).
6. **Prelude state mutations FOR ALL TOUCHED DRAWABLES** (codex round-9 CRITICAL catch):
   - **dst**: `open.touched.first_touch(dst_id, dst_prior)`, `open.layouts.first_touch_drawable(dst_id, dst_pre_layout)`, `store.touch_render_fence(dst_id, frame_ticket.clone())`.
   - **src (only when `RecordedTrapSrcKind::Drawable { id, .. }`)**: SAME three mutations on the src DrawableId. Skipping these is a lifetime bug class — the source can be freed/reused while the open frame intends to sample it at close. The B.2 render_composite frame-builder body does this for each Drawable source (engine.rs:6756, 6801, 6804); the legacy v2 `render_traps_or_tris` touches the source fence on submit (engine.rs:7752). The B.3 port MUST replicate the B.2 behavior — touch BOTH at append.
   - **Solid / Gradient / None src kinds**: no src-side mutation needed (solid has no Drawable id; gradient is engine-owned CPU-immutable per B.2 R3; None already exited early in step 1).
7. `push_op_and_set_layouts(RecordedOp::RenderTrapsOrTris(payload), &layouts_to_set)` — where `layouts_to_set` includes `(dst_id, SHADER_READ_ONLY_OPTIMAL)` always, AND `(src_id, SHADER_READ_ONLY_OPTIMAL)` when src is `Drawable`.

Append-side MUST NOT carry a `mask_scratch_pin_idx` or a `dst_readback` view — emit re-resolves both fresh per N5.

Emit replays the two-stage CB:
- **Resolve src/dst views fresh** from engine caches at emit time (Drawable via `ensure_drawable_view`, Gradient via `picture_paint[xid].image_view()`, Solid via `solid_src_view` + `record_solid_color_clear`).
- **Resolve mask_view + mask_extent + current_layout fresh** from `engine.mask_scratch` (per N5 mask scratch ownership).
- **(a) Trap raster** — barrier mask → COLOR_ATTACHMENT, begin_rendering(bbox, load=CLEAR), bind trap/tri pipeline per `prim_kind`, bind vertex buffer, push `TrapDrawPushConsts`, set viewport+scissor (bbox), `cmd_draw(4, instance_count)`, end_rendering. Barrier mask → SHADER_READ_ONLY.
- **(b) Composite phase** — derive `needs_dst_readback = std_op.needs_dst_readback()` and `needs_full_dst = matches!(recorded.op_byte, 0|1|5|6|7|10|13|16..=27|32..=43)` (engine.rs:7472 — uses the recorded raw `op_byte` field per N5 listing) at emit time. Optional `record_dst_readback_copy(dst)` + `dst_readback.view(dst_format, dst_has_alpha)` resolve when `needs_dst_readback`. Fresh pipeline lookup `render_pipelines.get(std_op, dst_format, dst_has_alpha, false)`. Fresh descriptor allocation via the frame_generation-tagged ring. Compute `(render_dst_x/y, render_w/h, mask_off_x/y)` from `needs_full_dst`. Compose `combined_src_xform = compose(gradient.intrinsic, user_src_xform)` when src is Gradient, else `user_src_xform`. Effective src_repeat = `REPEAT_PAD` when `src_is_synthetic_1x1`, else the recorded `src_repeat`. Build `CompositeAttrs { src_extent, mask_extent (fresh), src_repeat (effective), mask_repeat = REPEAT_NONE, src_force_opaque, mask_force_opaque = false, src_xform = combined, mask_xform = IDENTITY }`. Per-`clip_scissors` draw using the recorded `clip_scissors` slice. Standard composite close (dst → SHADER_READ_ONLY_OPTIMAL).
- **(c) Post-emit CPU writeback** (codex round-10 catch — see N5 "Post-emit CPU-layout writeback") — after the composite-close barrier transitions mask_scratch back to SHADER_READ_ONLY_OPTIMAL, call `engine.mask_scratch.as_mut().expect("ensured").set_current_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)` to advance the CPU-tracked layout (mirror engine.rs:7740-7749). Skipping this leaves the NEXT trap op's pre-barrier reading a stale old_layout.

Includes the cross-frame mask-scratch-grow integration test per N5 (3-op sequence `(small, large, large)` spanning frames F1 + F2 — the grow inherently triggers `close-before-grow` between op 1 and op 2, so F1 closes before the grow and F2 reopens against M1). Also includes a **solid-source-equivalence test** asserting that a Solid-src trap op replays with `record_solid_color_clear` writing the correct color before composite (catches the "stale 1×1 solid contents" replay bug codex round-7 flagged).

### Phase 5 — GLYPH family (2 tasks: image_text only)

**For `image_text`:** same 2-task shape — body rewrite + integration test. Body: empty-input fast-path return → renderer_failed check → `flush_render_batch` per N9 (no `flush_cow_batch` — deleted in cow_copy_area task) → lazily ensures the `V2GlyphAtlas` + `TextPipeline` (mirroring B.1's composite_glyphs body) → packs glyph misses into the atlas with per-glyph staging-buffer pins via `open.pins.pin_staging` → appends `RecordedOp::GlyphUpload` variants per miss (B.1 variant — REUSED) → appends `RecordedOp::ImageText`.

Emit replays per N7: B.1's existing `Op::GlyphUpload` match arm in `emit_recorded_op_into_cb` (engine.rs:8425-8439, a match arm, NOT a separate function) handles the upload side; the new `Op::ImageText` arm in the same dispatch records the text-pipeline draw against the atlas, mirroring B.1's `Op::CompositeGlyphs` arm (engine.rs:8440-8478).

Reuses B.1's atlas pin-ceiling logic — no new counter or invariant introduced beyond what B.1 already enforces for composite_glyphs.

**Required target-format gate** (per N7's LOAD-BEARING addition, codex round-7 catch): `image_text`'s rewrite MUST early-return `Ok(stats)` when the dst format is not `vk::Format::B8G8R8A8_UNORM` (mirror engine.rs:4515-4526). The gate fires BEFORE any atlas first-touch snapshot, glyph upload, or op append — so no rollback is needed. Phase 5's body rewrite MUST include this gate; Phase 5's integration test MUST include a negative-test asserting a NON-BGRA8 target (depth-1 or R8) drops the run without atlas mutation (no `last_render_ticket` change, no staging pin added). Depth-24 and depth-32 ARGB both alias to BGRA8 and PASS the gate — they are NOT negative-test targets (codex round-10 wording correction).

**Required atlas transactional discipline** (per N7's LOAD-BEARING addition): `image_text`'s rewrite MUST call the same first-touch-atlas snapshot helper that `composite_glyphs_via_frame_builder` does (engine.rs:5314 style — snapshot `atlas_prev_ticket_snapshot` + `layouts.first_touch_atlas`). The existing `commit_close_success` (engine.rs:8780) and `rollback_atlas` (engine.rs:8838) paths handle the rest. Skipping the snapshot would leak a stale `last_render_ticket` on close failure — recreates a B.2-class transaction bug. Phase 5's integration test MUST exercise a close-failure path (e.g., via `platform_force_next_submit_failure_for_tests`) AND verify that the atlas's `last_render_ticket` rolls back to its pre-frame value.

### Phase 6 — Wrap-up (2 tasks)

**Task 18: cargo +nightly fmt + plain clippy.** Mirrors B.2 Task 19. Plain clippy only (NOT pedantic per AGENTS.md). Fix any clippy warnings the B.3 surface introduced.

**Task 19: Status doc + bee hardware-gate placeholder.**
- Append Phase B.3 entry to `docs/status.md` matching the B.1/B.2 entry shape.
- Bee hardware-smoke gate placeholder: post-B.3 MATE-drag telemetry vs B.2 baseline (`non_ported/s`, `submit_group_flushes/s`, frame builder `ops/frame_avg` increase).

## Acceptance gates

### Implementation gates (per-task, validated during plan execution)

- `cargo build` clean for each task.
- `cargo test -p yserver --lib` green for each task.
- `cargo +nightly fmt --check` clean.
- `cargo clippy --workspace --all-targets` (plain, NOT pedantic) clean for the B.3 surface.
- Each integration test in Phase 2/3/4 demonstrates two-op collapse-to-one-submit for its op.

### Hardware gates (user-driven, after Task 19)

- **bee MATE-load** after all 8 ports land:
  - `close_reasons[non_ported]/s` → ≤ 10 (vs ~900–1100 pre-B.3). Only residual non_ported source is `render_composite_legacy` under explicit kill-switch.
  - `submit_group_flushes/s` drop by 30–50% beyond B.2's ~75% absorption. Combined with B.2: target ~200–400 submits/s on bee MATE drag (the original Phase B spec target).
  - `ops/frame_avg` rises from B.2's ~1.7 to ~4–8 (more ops per frame as M2 stops fragmenting them).
  - `frame_builder_aborts/s = 0` (no new failure modes under load).
- **silence (dual-output)** regression check — no scene-compose regression, no ERROR_DEVICE_LOST, no fault chains.
- **yoga / iMac / fuji** regression checks — no new errors on those platforms.
- **Cross-vendor sanity** — same MATE drag on a non-radv host (nvidia, intel, lavapipe) — no new validation VUIDs introduced by B.3.

## Open questions

These remain unresolved at design time and ride along to implementation:

1. **`render_traps_or_tris` payload size threshold.** N5 finalizes ONE recorded op covering both stages. The only outstanding question is the size budget: if `RecordedRenderTrapsOrTris` exceeds 512B (the size-budget assertion limit), the variant will need additional Box-wrapping of inner fields (e.g., the trap vertex pool, the clip list) to stay under the limit. NOT a structural redesign — the single-op decision is final per N5; this is a payload layout detail to settle at implementation when the actual field set is sized.

2. **`logic_fill` pipeline variants.** GC logic modes (Copy, And, Or, Xor, …) map to render pipeline variants. The pre-B.3 logic_fill body already handles this; the rewrite preserves the same `inner.logic_fill_caches[dst_format].get(logic_mode, opaque_alpha)` cache lookup.

3. **`close_open_frame_for_non_ported_op` removal timing.** Both the helper and its `render_composite_legacy` call site (engine.rs:5877) stay through B.3 — the legacy path still needs the close any time the B.2 kill-switch is OFF (codex round-2 R2.1). B.5 removes the legacy body, the helper, and `CloseReason::NonPortedPaintOp` together.

4. **v1 backend compile fallout.** The `SubmittedOp::scratch` slot rename (Option → Vec) may break v1 if v1 instantiates `SubmittedOp` directly. Per project policy, fix v1 with bare-minimum compile patch only; v1 is scheduled for removal in a later phase.

## Risk register

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Validation VUIDs in MASK family emit (mask scratch + composite cross-barrier) | Medium | High (silent corruption) | Bee vkdebug pass after MASK port lands, before merging. |
| cow_copy_area rewrite wires `cow_id` to wrong DrawableStore entry | Low | High (corruption — wrong drawable's storage written) | Resolved at spec-time per N3 (cow is a regular Drawable in store). Integration test: two `cow_copy_area` calls in one frame collapse correctly and the cow's `storage.current_layout` updates via the standard `commit_close_success` walk. |
| put_image staging-buffer Arc not pinned to the right fence (UAF if staging buffer drops before frame retires) | Low | High (GPU fault) | B.1's pin-set mechanism is proven; mirror exactly. Test with concurrent destroy of the source Pixmap mid-frame. |
| Rip-and-replace makes regressions harder to bisect (no per-family env flip) | Medium | Medium (longer debug cycle if a port regresses) | Codex review depth (B.3 went through ≥10 review rounds before implementation); per-op integration tests; git revert remains the bisect tool of last resort. Project preference accepts this tradeoff. |
| `fill_rect_batch` implementer splits per-rect instead of preserving per-call | Low | Medium (extra recorded ops, no behavior change in output) | Decision is final at spec time per N4: one `RecordedFillRect` per `fill_rect_batch` call carrying the entire rect slice. Splitting per-rect would be new behavior, not preservation. Reviewer enforces the rule at implementation. |
| v1 backend breaks beyond compile fix | Low | Low (v1 is scheduled for removal) | If v1 functionality breaks at runtime due to v2 internal changes, accept the breakage; do not delay B.3 to investigate v1 paths. |

## References

- Phase B spec: `docs/superpowers/specs/2026-05-24-frame-builder-phase-b-design.md`.
- Phase B.2 plan: `docs/superpowers/plans/2026-05-24-frame-builder-phase-b2.md`.
- Phase A spec: `docs/superpowers/specs/2026-05-23-frame-builder-submit-rate-design.md`.
- Audit findings (2026-05-25): `non_ported` aggregate counter confirmed in `telemetry.rs:679-682`; eight close-call-sites enumerated at `engine.rs:2255, 2285, 2505, 2747, 3091, 4140, 4498, 5877, 7215`.
