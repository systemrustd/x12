# Phase 3F-1 — rendering re-architecture — render-composite migration

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate `try_vk_render_composite` (RENDER `Composite`) from `run_one_shot_op + pre-record ProtocolBarrier flush` to `record_paint_batch_op`. Lands the per-batch descriptor arena infrastructure (`RenderPipelineCache::allocate_descriptor_for_views_into`) and the `DstReadback::needs_grow` + pre-resize-flush pattern that mirrors 3D's `CopyScratch` fix. `try_vk_render_traps_or_tris` stays on the legacy path; it migrates in phase 3F-2 once `MaskScratch::upload_r8` moves into `BatchUploadArena`.

**Architecture:** Two structural changes vs 3D/3E:

1. **Descriptor allocation moves into the per-batch `BatchDescriptorArena`.** The current `RenderPipelineCache::reset_descriptors() + allocate_descriptor_for_views(...)` shape is *fundamentally* unsafe under batched recording: the shared pool's `reset_descriptors` invalidates every set previously allocated from it, including sets still referenced by un-submitted CBs in the open batch. A new `allocate_descriptor_for_views_into(&mut BatchDescriptorArena, ...)` API allocates from the batch arena (chunks added on demand, all retired at batch retirement). The legacy `reset_descriptors() + allocate_descriptor_for_views()` API stays — `try_vk_render_traps_or_tris` keeps using it until 3F-2.

2. **`DstReadback::ensure` grow path needs a pre-resize batch flush** (same hazard 3D fixed for `CopyScratch`): `ensure` calls `queue_wait_idle` then destroys the old per-format scratch image, but `queue_wait_idle` does NOT wait for un-submitted commands. If a prior batch op recorded a `cmd_copy_image` referencing the old scratch image, freeing it would dangle. The mitigation: add `DstReadback::needs_grow(format, w, h)`; the migrated `try_vk_render_composite` checks BEFORE entering `record_paint_batch_op` and flushes the open batch first. Rare in steady state (grows only on first sight of a larger dst window).

The recorder body (`record_render_composite`) is unchanged. The descriptor set is allocated inside the batch closure (where `&mut PaintBatch` is available) and passed into the recorder exactly as before.

**Tech Stack:** Rust, ash (Vulkan), existing 3A–3E infrastructure (`PaintBatch`, `BatchDescriptorArena`, `record_paint_batch_op`, `paint_resources()`, `renderer_failed` gate, drawable-destruction barriers, audit catalogue).

---

## Prerequisite — confirm post-3E baseline

Before T1, verify the tree state:

```bash
cd /home/jos/Projects/yserver
git log --oneline graphics-followups | head -10
rg -n 'record_paint_batch_op\|record_paint_op' crates/yserver/src/kms/backend.rs | head
```

Expected:
- Phase 3E is landed (commit `492b4bc` plus any subsequent fixes). `try_vk_text_run` and `try_vk_render_composite_glyphs` use `record_paint_op`. Pre-3E `ProtocolBarrier` flush at the head of each is GONE.
- ≥ 5 `record_paint_op`/`record_paint_batch_op` call sites in `backend.rs`: 3B fill ×3, 3B copy ×3, 3C `put_image` + `upload_bgra_to_mirror`, 3D same-overlap, 3E text-run ×2.
- The two stale `3D-deferred: render-composite/render-traps needs per-batch...` comments at backend.rs `~5096` (traps) and `~6044` (composite) are still present — these are the **explicit migration markers** for 3F-1 / 3F-2.
- `cargo test --workspace`, `cargo clippy -p yserver`, `cargo +nightly fmt --check` all green.

If any of the above don't hold, STOP — the prerequisite chain didn't fully land.

## Phase context

Read `docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md` first (esp. "Shape of the new code → `PaintBatch`" and "What gets replaced, by current file") and `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3e-results.md` for the immediate predecessor's outcomes.

Phase 3F is the **render family** of recorders. The HLD's phasing model has phase 3 = "Migrate recorders to `PaintBatch`. One family at a time (fill → copy → image → render → text → traps)." `try_vk_render_composite` and `try_vk_render_traps_or_tris` are the last two paint-side recorders on `run_one_shot_op + ProtocolBarrier flush`. They differ in two ways:

| | `try_vk_render_composite` | `try_vk_render_traps_or_tris` |
|---|---|---|
| Source | Drawable / Solid / Gradient / None | Same + CPU-rasterised coverage mask |
| Mask | Drawable / Solid / Gradient / None | `MaskScratch` (R8, CPU-rasterised) |
| dst-readback (Disjoint/Conjoint) | YES (via shader binding 2) | YES (same path) |
| Pre-record uploads | None — all sources are pre-existing images/scratches | `MaskScratch::upload_r8` of the per-call rasterised mask |
| Family | Family A (no upload) | Family B (has upload) |

3F-1 lands the family-A migration (render-composite) which proves out the descriptor-arena + `dst_readback::needs_grow` patterns. 3F-2 picks up family B (traps) on top of those, plus the `MaskScratch::upload_r8` arena-staging work.

`try_vk_render_composite_glyphs` is **already migrated** (3E). It does NOT use `RenderPipelineCache`; it uses `TextPipeline`'s pre-existing atlas descriptor binding. So 3E getting in first didn't disturb the descriptor-arena story.

### Why the descriptor-arena migration is load-bearing (not nice-to-have)

The current shared-pool API is:

```rust
// render_pipeline.rs:447
pub fn reset_descriptors(&self) -> Result<(), vk::Result> {
    unsafe { self.vk.device.reset_descriptor_pool(self.descriptor_pool, ...) }
}

// render_pipeline.rs:461
pub fn allocate_descriptor_for_views(
    &self,
    src_view: vk::ImageView, mask_view: vk::ImageView, dst_view: vk::ImageView,
) -> Result<vk::DescriptorSet, vk::Result> { /* alloc from self.descriptor_pool, write */ }
```

Today `try_vk_render_composite` calls `reset_descriptors()` then `allocate_descriptor_for_views(...)` to get a fresh set per call. Under `run_one_shot_op`, the CB is submitted + waited-idle inside the call, so by the time the NEXT `try_vk_render_composite` resets the pool, the GPU is done with the previous set. **Under batched recording that invariant is gone.** If two render-composites land in the same batch, the second's `reset_descriptors` invalidates the first's descriptor set, but the first set is still referenced by un-submitted CB commands → UB.

`BatchDescriptorArena` (3A T6) solves this by chunking pools per batch and only retiring them at batch retirement. The arena already exists; 3F-1 wires the first consumer.

The legacy `RenderPipelineCache::reset_descriptors / allocate_descriptor_for_views` API stays around for `try_vk_render_traps_or_tris` until 3F-2. 3F-2 removes it.

### Why dst_readback needs a `needs_grow` accessor

`DstReadback::ensure` (`dst_readback.rs:76`) reads:

```rust
if let Some(old) = slot.take() {
    unsafe {
        let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
        // destroy old image + view + memory
    }
}
let new_img = allocate(...)?;
*slot = Some(new_img);
```

`queue_wait_idle` waits for submitted GPU work; un-submitted batch commands are not part of that. So an open batch holding `cmd_copy_image(dst → old_scratch_image, ...)` would dangle when the old image is destroyed. Exactly the 3D `CopyScratch` hazard. 3F-1 applies the same mitigation: add a `needs_grow(format, w, h)` accessor; the migrated function calls it BEFORE the mut borrows and pre-flushes the batch when grow is pending.

The grow path is rare:
- `DstReadback::ensure` only allocates new scratches on first use OR when a larger dst extent is seen. After the first frame of stable dst sizes, no grow happens.
- Tracked per-format (BGRA + R8), so each format has its own grow trajectory.

### Key invariants 3F-1 inherits

1. **Drop-order**: `KmsBackend.scheduler` before `KmsBackend.ops_command_pool`. Don't touch field order (`feedback_kmsbackend_drop_order` memory).
2. **Drawable-destruction barriers**: 3B's 5 sites cover dst (windows + pixmaps). No new barriers needed.
3. **`renderer_failed` gate**: every paint entry goes through `paint_resources()`.
4. **`record_paint_batch_op` is the load-bearing API**: 3F-1 needs `&mut PaintBatch` inside the closure to call `batch.descriptor_arena_mut()`. Use the wide API.
5. **`PaintBatch::descriptor_arena_mut()` lazily creates the arena** — first call per batch allocates the first pool chunk. No setup needed in the migration.

### Out of scope (deferred to 3F-2 and later)

- `try_vk_render_traps_or_tris` migration — 3F-2.
- `MaskScratch::upload_r8` → `BatchUploadArena` migration — 3F-2.
- `MaskScratch::needs_grow` for the image-side grow path — 3F-2.
- Removing the legacy `RenderPipelineCache::reset_descriptors / allocate_descriptor_for_views` API — 3F-2 (when its last user, `try_vk_render_traps_or_tris`, migrates).
- `RenderPipelineCache.descriptor_pool` field removal — 3F-2.
- Phase-4 sync rework (replacing remaining waitIdles).
- Phase-5 `GetImage` targeted fence + `GlyphAtlas::intern` rewrite.

## File structure

| File | Role | Touched in |
|---|---|---|
| `crates/yserver/src/kms/vk/dst_readback.rs` | Add `pub fn needs_grow(&self, format, w, h) -> bool`. | T1 |
| `crates/yserver/src/kms/vk/render_pipeline.rs` | Add `pub fn allocate_descriptor_for_views_into(&self, arena: &mut BatchDescriptorArena, src, mask, dst) -> Result<DescriptorSet, vk::Result>`. Keep the legacy `allocate_descriptor_for_views` until 3F-2. | T2 |
| `crates/yserver/src/kms/backend.rs` | Migrate `try_vk_render_composite` (~lines 5580–6163); update `run_legacy_paint_op` audit catalogue (~line 1697 doc block); delete the stale `3D-deferred: render-composite needs...` comment at ~line 6044. | T3 |
| `crates/yserver/src/kms/vk/ops/render.rs` | `record_render_composite` signature unchanged — descriptor set is still passed as a `vk::DescriptorSet` parameter. | (read only) |
| `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-1-results.md` | Results doc | T4 |

## Pre-task notes (read before starting)

1. **The borrow split inside the migrated function is the load-bearing structural change.** Disjoint fields needed simultaneously:
   - `&mut self.scheduler` (the `record_paint_batch_op` receiver).
   - `&mut DrawableImage` for `dst_mirror` (via `self.windows.get_mut` / `self.pixmaps.get_mut`).
   - `&mut SolidColorImage` for `solid_src_image` (via `self.solid_src_image.as_mut()`).
   - `&mut SolidColorImage` for `solid_mask_image` (via `self.solid_mask_image.as_mut()`).
   - `Option<&mut DstReadback>` for `dst_readback` (via `self.dst_readback.as_mut()`).
   - `&self.render_pipelines` (shared) — to call `allocate_descriptor_for_views_into` inside the closure. `RenderPipelineCache::allocate_descriptor_for_views_into` takes `&self` (NOT `&mut self`) — it doesn't mutate the cache, just allocates from the supplied arena. So this is a shared borrow that coexists with the disjoint `&mut`s above.

   All field paths must be `self.X` directly (NOT through a `&mut self` helper). The borrow checker accepts disjoint field projection but not helper methods.

2. **`pipeline` and `pipeline_layout` are NOT borrows.** `RenderPipelineCache::get()` returns `vk::Pipeline` (a Vulkan handle, `Copy`). `pipeline_layout()` returns `vk::PipelineLayout` (also `Copy`). Calls to `.get()` need a transient `&mut self.render_pipelines` (HashMap insert) but the returned handle is no-borrow. So:

   ```rust
   let pipeline = self.render_pipelines.as_mut().expect("...").get(...)?;  // &mut, transient
   let pipeline_layout = self.render_pipelines.as_ref().expect("...").pipeline_layout();  // &, transient
   // both borrows END here; pipeline and pipeline_layout are Copy
   ```

3. **Pre-resize flush ordering**: the flush MUST happen BEFORE the mut borrows of dst_mirror / dst_readback / solid_src / solid_mask are acquired, because `flush_if_needed` takes `&mut self`. Pattern:

   ```rust
   // 1. paint_resources() — shared self
   let Some((vk_arc, pool_handle)) = self.paint_resources() else { ... };

   // 2. Pre-resize check on dst_readback (immutable borrow; ends quickly)
   let needs_readback_grow = needs_dst_readback
       && self
           .dst_readback
           .as_ref()
           .is_some_and(|r| r.needs_grow(dst_format, dst_extent.width, dst_extent.height));
   if needs_readback_grow {
       self.flush_if_needed(BatchFlushReason::ProtocolBarrier)?;  // &mut self, OK now
   }

   // 3. ensure_drawable_mirror_sampleable for src/mask  (already exists in current body)
   // 4. dst_readback.ensure (&mut self.dst_readback)
   // 5. Resolve all the views (& mut transient where needed)
   // 6. Build attrs
   // 7. Re-borrow dst_mirror + solid_src + solid_mask + dst_readback into closure-captures
   // 8. self.scheduler.record_paint_batch_op(...)
   ```

   `flush_if_needed` returns `Result<(), BatchError>`. On failure, return `false` from `try_vk_render_composite` so the caller falls back to pixman. Don't `?` it directly — wrap.

4. **`dst_readback.view()` is `&mut self.dst_readback`** because it lazily creates the no-alpha view. Call it AFTER `ensure()` (which is also `&mut`). Both borrows are transient (end at the assignment).

5. **No new destruction barriers needed.** Dst is a window/pixmap; 3B's 5 sites already flush+leak before freeing window/pixmap mirrors. The dst_readback scratch is owned by `DstReadback` on `KmsBackend` and only drops at backend teardown (covered by the KMS-teardown fix's `disarm` path indirectly — DstReadback's Drop waits idle which is correct at backend lifetime).

6. **No OOM-poison concern.** Both descriptor-set allocation (inside the closure) and clears + record_render_composite happen inside the `record_paint_batch_op` closure. The closure's `Result` propagates. If `allocate_descriptor_for_views_into` fails inside the closure, `record_paint_batch_op` poisons the batch — same as any other recorder failure. The post-flush failure is reported back to the caller (return `false` → pixman fallback). The dst_readback grow's pre-flush failure is detected BEFORE the recorder runs; on failure the function early-returns without touching the batch.

7. **Test coverage**: no direct unit test for `try_vk_render_composite` (matches every other migrated recorder). Coverage is xts5 + rendercheck + hardware smoke. T4 hardware smoke is the gate.

8. **clippy**: project preference is plain `cargo clippy`. 5 pre-existing `doc_lazy_continuation` warnings; no new ones.

9. **fmt**: nightly fmt required for the project.

---

## Task 1: Add `DstReadback::needs_grow` accessor

**Goal:** Expose the grow predicate so callers can detect "would the next `ensure` reallocate?" without mutating state.

**Files:**
- Modify: `crates/yserver/src/kms/vk/dst_readback.rs`

### Step 1: Read the current `ensure` body

- [ ] **Step 1: Read `dst_readback.rs:76–117`**

The grow predicate lives implicitly in `ensure`:

```rust
let slot = match format {
    vk::Format::B8G8R8A8_UNORM => &mut self.bgra,
    vk::Format::R8_UNORM => &mut self.r8,
    _ => return Err(DstReadbackError::NoMemoryType),
};
if let Some(img) = slot.as_ref()
    && img.extent.width >= width
    && img.extent.height >= height
{
    return Ok(());
}
// otherwise: grow
```

`needs_grow` is the negation of that early-return.

### Step 2: Add the accessor

- [ ] **Step 2: Append to `impl DstReadback`, immediately above `ensure`**

```rust
    /// True if a later `ensure(format, width, height)` call would
    /// reallocate the per-format scratch image. Callers in batched
    /// paint paths use this BEFORE entering `record_paint_batch_op`
    /// so they can flush any in-flight batch — `ensure` destroys
    /// the old image after `queue_wait_idle`, which does NOT wait
    /// for un-submitted commands. Without a pre-flush, an open
    /// batch CB embedding the old scratch image would dangle.
    ///
    /// Unknown formats return `false` (the caller's `ensure` will
    /// fail with `NoMemoryType` for the same input — a flush wouldn't
    /// change that outcome).
    pub fn needs_grow(&self, format: vk::Format, width: u32, height: u32) -> bool {
        let slot = match format {
            vk::Format::B8G8R8A8_UNORM => self.bgra.as_ref(),
            vk::Format::R8_UNORM => self.r8.as_ref(),
            _ => return false,
        };
        match slot {
            Some(img) => width > img.extent.width || height > img.extent.height,
            None => true, // first allocation also counts as "grow"
        }
    }
```

### Step 3: Build

- [ ] **Step 3: `cargo check -p yserver`**

Expected: clean.

### Step 4: Tests + fmt + clippy

- [ ] **Step 4: `cargo test -p yserver --lib 2>&1 | tail -5`**

Expected: 138 passed, 0 failed, 3 ignored (unchanged from post-3E baseline).

- [ ] **Step 5: `cargo +nightly fmt --check`**

Expected: no diff.

- [ ] **Step 6: `cargo clippy -p yserver 2>&1 | tail -10`**

Expected: 5 pre-existing `doc_lazy_continuation` warnings; no new ones.

### Step 5: Commit T1

- [ ] **Step 7: Commit**

```bash
git add crates/yserver/src/kms/vk/dst_readback.rs
git commit -m "$(cat <<'EOF'
refactor(kms): add DstReadback::needs_grow accessor

3F-1 prep. Callers in batched paint paths need to detect "would
ensure() grow this scratch?" before acquiring any mut borrows, so
they can pre-flush the open PaintBatch — ensure() destroys the
old image after queue_wait_idle, which does NOT wait for
un-submitted commands. Without the pre-flush, an open batch CB
referencing the old scratch would dangle. Same hazard 3D fixed
for CopyScratch.

Pure read; no behaviour change. needs_grow on an unallocated
slot returns true (first allocation is also a "grow" point).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Add `RenderPipelineCache::allocate_descriptor_for_views_into`

**Goal:** Add a per-batch arena-backed descriptor allocator. The set is allocated from the supplied `BatchDescriptorArena` (chunks retired at batch retirement) instead of the shared `RenderPipelineCache::descriptor_pool` (which is invalidated by `reset_descriptors`). The legacy API stays — `try_vk_render_traps_or_tris` continues to use it until 3F-2.

**Files:**
- Modify: `crates/yserver/src/kms/vk/render_pipeline.rs`

### Step 1: Read the legacy allocator

- [ ] **Step 1: Read `render_pipeline.rs:461–504`**

Capture the shape of `allocate_descriptor_for_views`:
- Allocates one set from `self.descriptor_pool` with `self.descriptor_set_layout`.
- Writes three `COMBINED_IMAGE_SAMPLER` descriptors (bindings 0/1/2: src/mask/dst) sharing `self.sampler`, layout `SHADER_READ_ONLY_OPTIMAL`.

The new function is identical except the allocation source is the supplied arena. The descriptor writes are byte-for-byte the same.

### Step 2: Add the arena-aware allocator

- [ ] **Step 2: Append to `impl RenderPipelineCache`, immediately below the legacy `allocate_descriptor_for_views`**

(Don't replace the legacy — `try_vk_render_traps_or_tris` still uses it until 3F-2.)

```rust
    /// Per-batch variant of `allocate_descriptor_for_views`. Allocates
    /// the descriptor set from the supplied `BatchDescriptorArena`
    /// (whose chunks live until the batch retires) instead of the
    /// per-cache shared pool (which `reset_descriptors` invalidates).
    /// Callers that record into an open `PaintBatch` MUST use this
    /// path — the shared-pool variant is unsafe under batching
    /// because subsequent ops in the same batch would reset and
    /// invalidate sets still referenced by un-submitted CB commands.
    ///
    /// 3F-1: first consumer is `try_vk_render_composite`. 3F-2 will
    /// migrate `try_vk_render_traps_or_tris` and remove the legacy
    /// shared-pool path along with `reset_descriptors`.
    pub fn allocate_descriptor_for_views_into(
        &self,
        arena: &mut crate::kms::scheduler::batch_descriptor_arena::BatchDescriptorArena,
        src_view: vk::ImageView,
        mask_view: vk::ImageView,
        dst_view: vk::ImageView,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        let set = arena.allocate_set(self.descriptor_set_layout)?;
        let src_info = [vk::DescriptorImageInfo::default()
            .image_view(src_view)
            .sampler(self.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let mask_info = [vk::DescriptorImageInfo::default()
            .image_view(mask_view)
            .sampler(self.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let dst_info = [vk::DescriptorImageInfo::default()
            .image_view(dst_view)
            .sampler(self.sampler)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&src_info),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&mask_info),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(2)
                .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .image_info(&dst_info),
        ];
        unsafe { self.vk.device.update_descriptor_sets(&writes, &[]) };
        Ok(set)
    }
```

### Step 3: Build

- [ ] **Step 3: `cargo check -p yserver`**

Expected: clean.

If you see `unresolved import` for `BatchDescriptorArena`: the qualified path in the signature is `crate::kms::scheduler::batch_descriptor_arena::BatchDescriptorArena`. Avoid adding a `use` — keep the type inline so the dependency direction (vk → scheduler) is explicit at the signature.

### Step 4: Tests + fmt + clippy

- [ ] **Step 4: `cargo test -p yserver --lib 2>&1 | tail -5`**

Expected: 138 passed.

- [ ] **Step 5: `cargo +nightly fmt --check`**

Expected: no diff.

- [ ] **Step 6: `cargo clippy -p yserver 2>&1 | tail -10`**

Expected: 5 pre-existing warnings; no new ones. If clippy complains about `too_many_arguments` (the new fn has 5 args: self, arena, 3 views — under the default threshold) or anything else, fix immediately rather than adding `#[allow(...)]`.

### Step 5: Commit T2

- [ ] **Step 7: Commit**

```bash
git add crates/yserver/src/kms/vk/render_pipeline.rs
git commit -m "$(cat <<'EOF'
refactor(kms): add RenderPipelineCache::allocate_descriptor_for_views_into

3F-1 prep. Per-batch descriptor allocator backed by
BatchDescriptorArena. Allocates the descriptor set from the
supplied arena (whose pool chunks are retired with the batch)
instead of RenderPipelineCache's shared descriptor_pool (which
reset_descriptors invalidates).

The shared-pool path is unsafe under batched recording: two
RENDER ops in the same batch would have the second's
reset_descriptors invalidate the first's set, but the first set
is still referenced by un-submitted CB commands.

Legacy allocate_descriptor_for_views + reset_descriptors stay
around for try_vk_render_traps_or_tris until 3F-2 migrates it.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Migrate `try_vk_render_composite` to `record_paint_batch_op`

**Goal:** Replace the `pre-record ProtocolBarrier flush + run_one_shot_op` shape with `paint_resources()` + `self.scheduler.record_paint_batch_op(...)`. Allocate the descriptor set inside the closure from `batch.descriptor_arena_mut()`. Pre-flush the batch only if `dst_readback.needs_grow` returns true.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (`try_vk_render_composite`, ~lines 5580–6163)

### Step 1: Read the existing function

- [ ] **Step 1: Read backend.rs lines 5580–6163**

Note the structure that survives the migration vs gets replaced:

| Lines (approx) | What | Disposition |
|---|---|---|
| 5594–5598 | `use` for `run_one_shot_op`, `record_solid_color_clear`, `StdPictOp` | Drop `run_one_shot_op` from the use list. Keep `vk_render` + `record_solid_color_clear` + `StdPictOp`. |
| 5599–5605 | rects empty + StdPictOp check | Keeps. |
| 5607–5622 | self-composite alias check | Keeps. |
| 5624–5631 | raw `vk_arc` + `pool_handle` bindings (debug-bail logging) | **REPLACED** by `paint_resources()` (single bail). |
| 5632–5646 | pipeline/scratch presence checks + `needs_dst_readback` | Keeps. |
| 5648–5680 | dst format / extent / depth resolution | Keeps. |
| 5682–5722 | `ensure_drawable_mirror_sampleable` for src + mask | Keeps. Note: this is `&mut self` already. |
| 5723–5746 | drawable src format gating | Keeps. |
| 5749–5778 | `render_pipelines.get(...)` + `pipeline_layout()` + `reset_descriptors()` | **`reset_descriptors` REMOVED** (arena handles this). Keep `get` + `pipeline_layout`. |
| 5780–5794 | `solid_src_view` / `solid_mask_view` / `white_mask_view` immutable reads | Keeps. |
| 5796–6005 | src + mask view resolution (R8/depth-24/regular paths) — `&mut self.windows/pixmaps` for swizzled-view caching | Keeps. |
| 6008–6028 | `dst_readback.ensure` + `view` (Disjoint/Conjoint path) | **REORDERED**: needs_grow check + pre-flush MUST come BEFORE `ensure`. |
| 6031–6042 | `allocate_descriptor_for_views(...)` (legacy shared-pool) | **REPLACED**: descriptor set is allocated inside the closure from `batch.descriptor_arena_mut()`. |
| 6044–6054 | `3D-deferred: render-composite needs...` comment + `flush_if_needed(ProtocolBarrier)` block | **DELETED.** |
| 6056–6074 | re-borrow dst_mirror / solid_src / solid_mask / dst_readback for the closure | Keeps (the borrows themselves; the captures move into the new closure). |
| 6075–6122 | xform composition + repeat translation + `CompositeAttrs` build | Keeps. |
| 6123–6162 | `run_one_shot_op(...) { record_solid_color_clear(s) + dst_readback.record_copy_from + record_render_composite }` | **REPLACED** by `self.scheduler.record_paint_batch_op(...)` with the descriptor allocation inside the closure. |

### Step 2: Replace the early raw `vk_arc` / `pool_handle` binding

- [ ] **Step 2: Replace lines ~5624–5631**

Find:

```rust
        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            log::debug!("vk composite bail: no Vk context (dst=0x{dst_xid:x})");
            return false;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            log::debug!("vk composite bail: no ops_command_pool (dst=0x{dst_xid:x})");
            return false;
        };
```

Replace with:

```rust
        // 3F-1: acquire batch resources up-front (gated by
        // renderer_failed). Replaces the raw vk + ops_pool reads;
        // the renderer_failed gate inside paint_resources() is the
        // same one fill/copy/image/text now go through.
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            log::debug!(
                "vk composite bail: paint_resources unavailable (renderer_failed or vk/pool absent) (dst=0x{dst_xid:x})"
            );
            return false;
        };
```

### Step 3: Remove the `reset_descriptors` call

- [ ] **Step 3: Delete the `reset_descriptors` block (~lines 5770–5778)**

Find:

```rust
        if let Err(e) = self
            .render_pipelines
            .as_ref()
            .expect("checked above")
            .reset_descriptors()
        {
            log::warn!("vk render_composite: descriptor pool reset failed: {e:?}");
            return false;
        }
```

Delete the entire block (including the `if let Err` line and the closing brace + trailing blank line).

### Step 4: Insert the pre-resize flush BEFORE `dst_readback.ensure`

- [ ] **Step 4: Add the pre-resize flush immediately above the `dst_readback_view = if needs_dst_readback { ... }` block at ~line 6013**

Find the lines starting with:

```rust
        // For Disjoint/Conjoint ops the shader reads the dst pixel
        // through binding 2; we copy dst → scratch inside the CB
        // below and bind the scratch's sampleable view here. For
        // standard ops the binding is unused — bind the white-mask
        // scratch to satisfy the descriptor layout.
        let dst_readback_view = if needs_dst_readback {
            let scratch = self.dst_readback.as_mut().expect("checked above");
            if let Err(e) = scratch.ensure(dst_format, dst_extent.width, dst_extent.height) {
                log::warn!("vk render_composite: dst readback ensure failed: {e:?}");
                return false;
            }
```

Insert immediately ABOVE the `let dst_readback_view = ...` line:

```rust
        // 3F-1: dst_readback.ensure may grow (destroy old scratch
        // image after queue_wait_idle), which does NOT wait for
        // un-submitted batch commands. If an earlier op in the
        // open batch recorded `cmd_copy_image(dst → old_scratch)`,
        // freeing the old image would dangle. Pre-flush the batch
        // before the grow. needs_grow is false for the steady-state
        // path (no resize) so this only fires on first sight of a
        // larger dst. Same mitigation 3D applied to CopyScratch.
        let needs_readback_grow = needs_dst_readback
            && self
                .dst_readback
                .as_ref()
                .is_some_and(|r| r.needs_grow(dst_format, dst_extent.width, dst_extent.height));
        if needs_readback_grow {
            use crate::kms::scheduler::paint_batch::BatchFlushReason;
            if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
                log::warn!("vk render_composite: pre-resize flush failed: {e:?}");
                return false;
            }
        }
```

(Leave the existing `let dst_readback_view = if needs_dst_readback { let scratch = ...; if let Err(e) = scratch.ensure(...) ...` block immediately below unchanged. `ensure` is now safe because the open batch is retired.)

### Step 5: Delete the legacy `allocate_descriptor_for_views` call

- [ ] **Step 5: Remove the descriptor-set alloc block (~lines 6031–6042) AND the stale 3D-deferred comment + flush_if_needed below it (~lines 6044–6054)**

Find:

```rust
        let descriptor_set = match self
            .render_pipelines
            .as_ref()
            .expect("checked above")
            .allocate_descriptor_for_views(src_view, mask_view, dst_readback_view)
        {
            Ok(s) => s,
            Err(e) => {
                log::warn!("vk render_composite: descriptor alloc failed: {e:?}");
                return false;
            }
        };

        // 3D-deferred: render-composite needs per-batch MaskScratch + descriptor
        // strategy + dst_readback lifetime before migrating. The pre-record
        // ProtocolBarrier flush keeps this legacy path safe alongside batched
        // fill/copy/PutImage in the same protocol cycle.
        {
            use crate::kms::scheduler::paint_batch::BatchFlushReason;
            if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
                log::warn!("legacy paint flush failed: {e:?}");
                return false;
            }
        }
```

Delete the **entire** block (both the legacy descriptor-set alloc AND the 3D-deferred comment + flush). The descriptor set will be allocated inside the closure (T6 below).

### Step 6: Replace the run_one_shot_op call with record_paint_batch_op

- [ ] **Step 6: Replace lines ~6123–6162 (the `match run_one_shot_op(...) { ... }` block)**

Find:

```rust
        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            if let Some(c) = src_clear_color {
                record_solid_color_clear(vk, cb, solid_src_image, c);
            }
            if let Some(c) = mask_clear_color {
                record_solid_color_clear(vk, cb, solid_mask_image, c);
            }
            // Disjoint/Conjoint: snapshot dst into the readback
            // scratch, then restore dst to its current layout so
            // record_render_composite can transition it normally.
            if let Some(rb) = dst_readback {
                rb.record_copy_from(
                    cb,
                    dst_mirror.vk_image,
                    dst_mirror.current_layout(),
                    dst_format,
                    dst_mirror.extent,
                );
            }
            vk_render::record_render_composite(
                vk,
                cb,
                dst_mirror,
                pipeline,
                pipeline_layout,
                descriptor_set,
                &attrs,
                rects,
                scissor,
            )
        }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk render_composite: record failed on dst xid {dst_xid:#x}: \
                     {e:?} — falling back to pixman"
                );
                false
            }
        }
```

Replace with:

```rust
        // 3F-1: descriptor allocation moves into the closure where
        // `&mut PaintBatch` is available — `batch.descriptor_arena_mut()`
        // returns the per-batch arena. The set lives until batch
        // retirement (NOT until the next render-composite call), so
        // multiple render-composites in one batch don't trample each
        // other's descriptors the way the shared-pool path would.
        //
        // `render_cache` is a shared borrow on self.render_pipelines
        // — disjoint from the &mut self.scheduler that record_paint_batch_op
        // takes, and from the &mut captures (dst_mirror / solid_src /
        // solid_mask / dst_readback) which are all in disjoint fields.
        let render_cache = self.render_pipelines.as_ref().expect("checked above");
        let result = self.scheduler.record_paint_batch_op(
            vk_arc,
            pool_handle,
            |vk, batch, cb| {
                let descriptor_set = render_cache.allocate_descriptor_for_views_into(
                    batch.descriptor_arena_mut(),
                    src_view,
                    mask_view,
                    dst_readback_view,
                )?;
                if let Some(c) = src_clear_color {
                    record_solid_color_clear(vk, cb, solid_src_image, c);
                }
                if let Some(c) = mask_clear_color {
                    record_solid_color_clear(vk, cb, solid_mask_image, c);
                }
                // Disjoint/Conjoint: snapshot dst into the readback
                // scratch, then restore dst to its current layout so
                // record_render_composite can transition it normally.
                if let Some(rb) = dst_readback {
                    rb.record_copy_from(
                        cb,
                        dst_mirror.vk_image,
                        dst_mirror.current_layout(),
                        dst_format,
                        dst_mirror.extent,
                    );
                }
                vk_render::record_render_composite(
                    vk,
                    cb,
                    dst_mirror,
                    pipeline,
                    pipeline_layout,
                    descriptor_set,
                    &attrs,
                    rects,
                    scissor,
                )
            },
        );
        match result {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk render_composite: record failed on dst xid {dst_xid:#x}: \
                     {e:?} — falling back to pixman"
                );
                false
            }
        }
    }
```

### Step 7: Clean up the `use` line at the top of the function

- [ ] **Step 7: Drop `run_one_shot_op` from the use list at ~line 5594**

Find:

```rust
        use crate::kms::vk::{
            ops::{render as vk_render, run_one_shot_op},
            render_pipeline::{StdPictOp, record_solid_color_clear},
        };
```

Change to:

```rust
        use crate::kms::vk::{
            ops::render as vk_render,
            render_pipeline::{StdPictOp, record_solid_color_clear},
        };
```

### Step 8: Build

- [ ] **Step 8: `cargo check -p yserver`**

Expected: clean.

Troubleshooting (likely failures and fixes):

| Compiler complaint | Cause | Fix |
|---|---|---|
| `cannot borrow self.scheduler as mutable because self.* is already borrowed` | A captured value is still holding a borrow on `self.X` outside the closure | Confirm `dst_mirror` / `solid_src_image` / `solid_mask_image` / `dst_readback` / `render_cache` are bound via direct field paths (`self.windows.get_mut` etc., not via helper methods on `&mut self`) |
| `cannot borrow self.render_pipelines as mutable because it is also borrowed as immutable` | The shared `render_cache` borrow is being held across a `self.render_pipelines.as_mut()` call | The `pipeline` and `pipeline_layout` reads happen EARLIER (before `render_cache` is bound). Move the `render_cache = self.render_pipelines.as_ref()` binding to AFTER all `self.render_pipelines.as_mut()` reads are done |
| `render_cache` referenced inside closure when not captured | Closure inferred wrong capture mode | Bind `render_cache` to a `let` immediately before `self.scheduler.record_paint_batch_op(...)`; the closure captures by reference |
| `mismatched types: expected vk::Result, found BatchError` | `allocate_descriptor_for_views_into?` propagates `vk::Result`, but the closure must return `Result<(), vk::Result>` | The closure signature is `(vk, batch, cb) -> Result<(), vk::Result>` — `?` on `vk::Result` works directly |

### Step 9: Delete the stale `3D-deferred` reference in the audit catalogue

- [ ] **Step 9: Find the catalogue entry**

Run: `grep -n "render_composite\|render_traps" crates/yserver/src/kms/backend.rs | head -5`

The doc-comment block in `run_legacy_paint_op` (~line 1720) currently includes:

```
///   try_vk_render_traps (composite):  render::record_render_composite     — borrow-conflict fallback
///   try_vk_render_composite_glyphs:   text::record_text_run               — migrated 3E T2 (record_paint_op)
///   try_vk_render_composite:          render::record_render_composite     — borrow-conflict fallback
```

(The exact label on the render-composite entries reads "borrow-conflict fallback" today.)

- [ ] **Step 10: Update the entries**

Update `try_vk_render_composite` only (leave traps as-is for 3F-2):

```
///   try_vk_render_traps (composite):  render::record_render_composite     — borrow-conflict fallback (3F-2)
///   try_vk_render_composite_glyphs:   text::record_text_run               — migrated 3E T2 (record_paint_op)
///   try_vk_render_composite:          render::record_render_composite     — migrated 3F-1 (record_paint_batch_op + BatchDescriptorArena)
```

### Step 10: Tests + fmt + clippy

- [ ] **Step 11: `cargo test -p yserver --lib 2>&1 | tail -5`**

Expected: 138 passed.

- [ ] **Step 12: `cargo +nightly fmt --check`**

Expected: no diff.

- [ ] **Step 13: `cargo clippy -p yserver 2>&1 | tail -10`**

Expected: 5 pre-existing `doc_lazy_continuation` warnings; no new ones.

### Step 11: Commit T3

- [ ] **Step 14: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "$(cat <<'EOF'
refactor(kms): migrate try_vk_render_composite to record_paint_batch_op

Drops the pre-record ProtocolBarrier flush + run_one_shot_op
shape; RENDER Composite now lands in the open PaintBatch
alongside fill / copy / put_image / text. The descriptor set is
allocated inside the closure from batch.descriptor_arena_mut(),
so multiple RENDER Composites in one batch don't trample each
other's descriptor sets — the shared-pool reset_descriptors path
is no longer reachable from this call.

dst_readback's grow path keeps a small pre-record
flush_if_needed(ProtocolBarrier) gated on
DstReadback::needs_grow: ensure() destroys the old image after
queue_wait_idle, which does NOT wait for un-submitted commands,
so an open batch CB referencing the old scratch would dangle.
Same mitigation 3D applied to CopyScratch. Steady-state path
(no resize) skips the flush.

try_vk_render_traps_or_tris stays on the legacy path until 3F-2.

Audit catalogue updated: try_vk_render_composite is now marked
migrated 3F-1.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Validation + results doc

**Goal:** End-to-end verification + results doc following the 3D/3E template.

**Files:**
- Create: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-1-results.md`

### Step 1: Static verification

- [ ] **Step 1: Cutover greps (semantic, not numeric)**

```bash
cd /home/jos/Projects/yserver
rg -n 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs
```

Expected: NO hits inside `try_vk_render_composite` (its `run_one_shot_op` body is replaced). Remaining hits: `run_legacy_paint_op` body, 3 readback handlers (`try_vk_get_image_pixels`, `hw_cursor_refresh`, `read_mirror_pixels`), `try_vk_render_traps_or_tris` (3F-2 deferred), `open_with_commit`, `dump_scanout_one`. Compared to end-of-3E, the count drops by ONE.

```bash
rg -n 'flush_if_needed[(]BatchFlushReason::ProtocolBarrier' crates/yserver/src/kms/backend.rs
```

Expected: the OLD unconditional pre-record flush inside `try_vk_render_composite` (formerly at ~line 6049) is gone. ONE **resize-only** pre-flush remains inside `try_vk_render_composite` (fires only when `DstReadback::needs_grow` returns true). The traps function (`try_vk_render_traps_or_tris`) still has its unconditional pre-record flush — that's 3F-2's scope. Other remaining sites: `run_legacy_paint_op` body, drawable-destruction sites, gradient-create sites, `try_vk_copy_area` resize-only.

```bash
rg -n 'record_paint_batch_op|record_paint_op' crates/yserver/src/kms/backend.rs
```

Expected: ≥ 10 call sites (3B fill ×3, 3B copy ×3, 3C put_image, 3C upload_bgra, 3D same-overlap, 3E text-run ×2, 3F-1 render-composite). Should be one more than at end-of-3E.

```bash
rg -n 'allocate_descriptor_for_views\b' crates/yserver/src/kms/backend.rs
```

Expected: ZERO hits (the legacy variant). The new `_into` variant has its only call site inside the migrated `try_vk_render_composite` closure.

```bash
rg -n 'allocate_descriptor_for_views_into' crates/yserver/src/kms/
```

Expected: 1 definition (`render_pipeline.rs`) + 1 call site (`backend.rs` inside `try_vk_render_composite`).

```bash
rg -n 'reset_descriptors\(' crates/yserver/src/kms/backend.rs
```

Expected: ONE hit inside `try_vk_render_traps_or_tris` (legacy path until 3F-2). ZERO hits inside `try_vk_render_composite`.

```bash
rg -n 'needs_grow' crates/yserver/src/kms/
```

Expected: 2 definitions (`copy_scratch.rs` from 3D, `dst_readback.rs` from 3F-1 T1) + their respective call sites in `backend.rs` (try_vk_copy_area same-overlap from 3D + try_vk_render_composite from 3F-1).

```bash
rg -n '3D-deferred: render-composite' crates/yserver/src/kms/backend.rs
```

Expected: ZERO hits (the stale comment was inside the deleted pre-flush block).

```bash
rg -n '3D-deferred: render-traps' crates/yserver/src/kms/backend.rs
```

Expected: ONE hit (still present inside `try_vk_render_traps_or_tris` — that's 3F-2's scope).

- [ ] **Step 2: Tree green**

```bash
cargo +nightly fmt --check
cargo clippy -p yserver 2>&1 | tail -10
cargo test --workspace 2>&1 | tail -20
```

Expected:
- fmt: no diff.
- clippy: 5 pre-existing `doc_lazy_continuation` warnings; no new ones.
- tests: yserver lib 138 passed; workspace green.

### Step 2: Hardware smoke (REQUIRED — render-composite is a load-bearing path for GTK and RENDER tests)

- [ ] **Step 3: Run from a separate TTY** (per phase-3D-results.md teardown workflow)

The KMS teardown fix is landed, so yserver exits cleanly. F3 → F1 toggle still recommended.

```bash
just yserver-mate-hw-release
# OR `just yserver-xfce-hw` to re-check the xfce4 path against this migration
```

Exercise composite-heavy workloads:

1. **rendercheck on yserver** — `just rendercheck-yserver` (or the project-equivalent recipe). All categories: composite (every op), trapezoids, triangles. **Critical:** prior to 3F-1 every rendercheck composite case fired the pre-record `ProtocolBarrier` flush; if 3F-1 introduced a descriptor lifetime bug, rendercheck composite cases will fail visually (corrupted pixels) or with `paint batch submit failed` in the log.
2. **mate-control-center hover** — RenderComposite-heavy when hover gradients are active.
3. **GTK theme transitions** — open `gtk3-demo` or similar; click through the theme browser.
4. **gedit / pluma text + selection drag** — picks up render-composite via the cairo selection-highlight overlay (composite of solid colour over text).

**Pass criteria** (all must hold):

- No `vk render_composite: record failed` warnings in `yserver-hw.log`.
- No `paint batch submit failed`, no `renderer_failed`, no `DEVICE_LOST`.
- No `vk render_composite: pre-resize flush failed` warnings (rare path; if it fires, capture the context — it indicates a batch was already Poisoned).
- No `descriptor set` validation messages from `VK_LAYER_KHRONOS_validation` if it's enabled in the smoke build.
- No kernel GPU fault in `journalctl -k --since "yserver-start-time"`.
- rendercheck passes the same set of cases it passed pre-3F-1 (read the prior baseline from a recent yserver-hw run before starting, then diff).
- Subjective: GTK app rendering looks normal; mate-CC hover gradients are smooth; no flicker introduced.

If any fail, **STOP** — do not commit T4 or claim 3F-1 done. Most likely failure modes and root causes:

| Failure | Likely root cause |
|---|---|
| `descriptor set` validation msg | The descriptor arena wasn't actually wired — fell back to shared pool, or the arena's `allocate_set` returned a set from a stale pool. Check `BatchDescriptorArena::allocate_set` is being called inside the closure |
| `paint batch submit failed` on composite-heavy frames | Closure returned an Err that poisoned the batch. Check for a debug-asserted view handle or missing layout transition |
| GPU fault on rendercheck Disjoint/Conjoint cases | `dst_readback` lifetime hole — the pre-resize flush mitigation didn't fire when it should have. Check `needs_grow` returns true on first sight of a new dst extent |
| Visual corruption on Composite without GPU fault | Descriptor-set lifetime bug — set freed before submit. Re-check that the set is allocated from `batch.descriptor_arena_mut()` (lives until batch retire) and not from the shared pool |

### Step 3: Write results doc

- [ ] **Step 4: Create `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-1-results.md`**

Follow the 3D/3E template. Sections:

1. **Header**: title `Phase 3F-1 — rendering re-architecture — render-composite migration — results`, date (2026-05-13 or actual implementation date), plan ref (`phase3f-1.md`), branch (`graphics-followups`), predecessor (`phase3e-results.md`).
2. **Scope landed**: paragraph + bullets — `DstReadback::needs_grow` accessor (T1), `RenderPipelineCache::allocate_descriptor_for_views_into` arena-backed allocator (T2), `try_vk_render_composite` migrated to `record_paint_batch_op` (T3). Note that `try_vk_render_traps_or_tris` and the legacy shared-pool allocator + `reset_descriptors` are deliberately retained for 3F-2.
3. **Preflight checks**: real fmt / clippy / test counts from your run.
4. **Cutover greps**: actual `rg` output captured semantically (numbers will shift over time; capture the SITE list, not the count).
5. **Done conditions**: enumerated below.
6. **Hardware smoke results**: report the actual run — hostname, log file summary (lines matching the pass criteria), rendercheck delta vs pre-3F-1 baseline, and any anomalies. If the workload couldn't be run for environmental reasons, mark the phase as DONE_WITH_CONCERNS and surface the gap.
7. **Plan bugs caught (folded back into plan)**: any recipe-level issues hit during T1–T3 execution. If none, write "None — recipe applied cleanly."
8. **Commit summary** table: Plan, T1, T2, T3, T4.
9. **Known deferred items**: 3F-2 = `try_vk_render_traps_or_tris` + `MaskScratch::upload_r8 → BatchUploadArena` + `MaskScratch::needs_grow` + removal of the legacy `RenderPipelineCache::reset_descriptors` / `allocate_descriptor_for_views` / `RenderPipelineCache.descriptor_pool` field.
10. **What's next**: phase 3F-2 planning (the trapezoids/triangles family + MaskScratch arena-staging).

### Step 4: Commit T4

- [ ] **Step 5: Commit**

```bash
git add docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-1-results.md
git commit -m "$(cat <<'EOF'
docs(plans): phase-3F-1 validation results

T1 added DstReadback::needs_grow accessor. T2 added
RenderPipelineCache::allocate_descriptor_for_views_into routed
through BatchDescriptorArena. T3 migrated try_vk_render_composite
to record_paint_batch_op; descriptor set is allocated per-batch,
dst_readback grow is gated on a small needs_grow pre-flush.

The unconditional ProtocolBarrier flush before each RENDER
Composite is gone; composite-heavy frames now pack into the
open PaintBatch alongside fill / copy / put_image / text.

Hardware smoke on <host>: <result>. rendercheck composite cases
pass; no GPU faults; no descriptor validation warnings.

try_vk_render_traps_or_tris + MaskScratch arena migration is 3F-2.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Done conditions

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` produces 5 pre-existing `doc_lazy_continuation` warnings; no new warnings.
3. `cargo test --workspace` green; yserver lib 138 passed.
4. `DstReadback::needs_grow(format, w, h)` exists. Returns `true` for unallocated slot. Returns `width > extent.width || height > extent.height` for allocated slot. Unknown formats return `false`.
5. `RenderPipelineCache::allocate_descriptor_for_views_into(&self, arena: &mut BatchDescriptorArena, src, mask, dst) -> Result<vk::DescriptorSet, vk::Result>` exists and is the **only** descriptor-allocation path used by `try_vk_render_composite`.
6. The legacy `RenderPipelineCache::allocate_descriptor_for_views` and `reset_descriptors` are still present in the codebase but have ZERO callers inside `try_vk_render_composite`. They still have callers inside `try_vk_render_traps_or_tris` (3F-2 will remove).
7. `try_vk_render_composite` uses `record_paint_batch_op` with:
   - 5 disjoint field borrows captured by the closure (`dst_mirror`, `solid_src_image`, `solid_mask_image`, `dst_readback`, `render_cache`).
   - Descriptor set allocated INSIDE the closure via `render_cache.allocate_descriptor_for_views_into(batch.descriptor_arena_mut(), ...)`.
8. The OLD `flush_if_needed(BatchFlushReason::ProtocolBarrier)` block (formerly at ~line 6049) is GONE.
9. The `3D-deferred: render-composite needs...` comment is GONE.
10. The pre-resize flush for `dst_readback` exists and is **gated on `DstReadback::needs_grow`** — fires only when the next `ensure` would reallocate. In steady state (no dst extent growth) the flush does NOT fire.
11. **Ordering invariant**: the dst_readback pre-flush MUST happen BEFORE `scratch.ensure(...)`, not merely before `record_paint_batch_op`. If `ensure` runs first, it could free the old image while an unsubmitted batch CB still references it. The recipe orders it correctly; this done condition is here so a future reader can spot a regression.
12. The `run_legacy_paint_op` audit catalogue entry for `try_vk_render_composite` reads `migrated 3F-1 (record_paint_batch_op + BatchDescriptorArena)`.
13. The `run_legacy_paint_op` audit catalogue entry for `try_vk_render_traps (composite)` reads `borrow-conflict fallback (3F-2)` (carry-over annotation noting it's the next phase's scope).
14. Hardware smoke green per T4 step 3 — rendercheck delta against pre-3F-1 baseline shows no regressions; no GPU faults; no descriptor validation warnings; no `vk render_composite: record failed`.

## Cutover greps (post-3F-1 — semantic, not numeric)

```
$ rg -n 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs
# SITES expected: run_legacy_paint_op body, 3 readback handlers
# (try_vk_get_image_pixels, hw_cursor_refresh, read_mirror_pixels),
# try_vk_render_traps_or_tris (3F-2-deferred), open_with_commit,
# dump_scanout_one. ZERO hits inside try_vk_render_composite.

$ rg -n 'flush_if_needed[(]BatchFlushReason::ProtocolBarrier' crates/yserver/src/kms/backend.rs
# SITES: run_legacy_paint_op body, 5 drawable-destruction sites,
# 2 gradient-create sites, try_vk_copy_area same-overlap resize-only,
# try_vk_render_traps_or_tris (3F-2-deferred unconditional flush),
# try_vk_render_composite resize-only (NEW — only fires on dst_readback grow).

$ rg -n 'record_paint_batch_op\|record_paint_op' crates/yserver/src/kms/backend.rs
# Expected: ≥ 10 call sites.

$ rg -n 'allocate_descriptor_for_views\b' crates/yserver/src/kms/backend.rs
# ZERO (legacy variant has no remaining callers in backend.rs from
# render-composite; traps still uses it).

$ rg -n 'allocate_descriptor_for_views_into' crates/yserver/src/kms/
# 1 def + 1 call site (inside try_vk_render_composite closure).

$ rg -n 'reset_descriptors\(' crates/yserver/src/kms/backend.rs
# 1 hit inside try_vk_render_traps_or_tris (3F-2 will remove).

$ rg -n 'needs_grow' crates/yserver/src/kms/
# 2 defs (copy_scratch.rs, dst_readback.rs) + 2 call sites in backend.rs.

$ rg -n '3D-deferred: render-composite' crates/yserver/src/kms/backend.rs
# ZERO (comment was inside the removed pre-flush block).

$ rg -n '3D-deferred: render-traps' crates/yserver/src/kms/backend.rs
# 1 hit inside try_vk_render_traps_or_tris (3F-2 scope).
```

## Notes for the implementer

- **The borrow split is the only structural risk.** The 5-disjoint-field-plus-shared-render-cache pattern extends 3D's three-disjoint and 3E's four-disjoint patterns; if `cargo check` complains, you forgot direct field paths. The pattern, in order:
  1. `let Some((vk_arc, pool_handle)) = self.paint_resources() else { ... };`
  2. (existing pipeline checks)
  3. `let pipeline = self.render_pipelines.as_mut().expect("...").get(...)?;` (transient `&mut`, drops at `?`)
  4. `let pipeline_layout = self.render_pipelines.as_ref().expect("...").pipeline_layout();` (transient `&`, drops at `;`)
  5. (existing src/mask view resolution — keep as-is)
  6. `let needs_readback_grow = needs_dst_readback && self.dst_readback.as_ref().is_some_and(...);` (transient `&`)
  7. `if needs_readback_grow { self.flush_if_needed(...)?; }` (&mut self, drops after the if)
  8. `scratch.ensure(...) + scratch.view(...)` (transient &mut self.dst_readback)
  9. (existing xform composition + CompositeAttrs build)
  10. **Now acquire the closure captures**, all via direct field paths:
      - `let dst_mirror = if let Some(w) = self.windows.get_mut(...) { ... };` (`&mut DrawableImage`)
      - `let solid_src_image = self.solid_src_image.as_mut().expect(...);`
      - `let solid_mask_image = self.solid_mask_image.as_mut().expect(...);`
      - `let dst_readback = if needs_dst_readback { Some(self.dst_readback.as_mut().expect(...)) } else { None };`
      - `let render_cache = self.render_pipelines.as_ref().expect(...);`
  11. `self.scheduler.record_paint_batch_op(vk_arc, pool_handle, |vk, batch, cb| { ... })` — moves all captures.

- **`pipeline` and `pipeline_layout` are `Copy` Vulkan handles.** They live across the closure call (`Copy` semantics). No borrow conflict.

- **`record_render_composite` is unchanged.** The recorder body's barriers + dynamic-rendering begin + draws record the same way whether the CB is a one-shot or a batched CB.

- **No tests to write.** Coverage comes from rendercheck + xts5 + hardware smoke. The codebase has no unit test for Vulkan recorders; adding one would require a software ICD harness that doesn't exist.

- **Watch for**: if rendercheck reports new failures in Disjoint/Conjoint cases ONLY (not in standard ops), the dst_readback path is the suspect — verify the pre-resize flush fires on grow, and that the descriptor set is bound with the correct `dst_readback_view` (not `white_mask_view`) inside the closure.

- **Watch for the 3F-1/3F-2 carry-over**: `try_vk_render_traps_or_tris` is **deliberately left on the legacy path** in this phase. Don't migrate it accidentally. Its `reset_descriptors() + allocate_descriptor_for_views(...) + flush_if_needed(ProtocolBarrier) + run_one_shot_op(...)` shape is the 3F-2 starting point.
