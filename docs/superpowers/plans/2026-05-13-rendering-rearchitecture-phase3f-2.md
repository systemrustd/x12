# Phase 3F-2 — rendering re-architecture — render-traps/triangles migration + MaskScratch arena

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate `try_vk_render_traps_or_tris` (RENDER `Trapezoids` / `Triangles` / `TriStrip` / `TriFan`) from the legacy `run_one_shot_op + pre-record ProtocolBarrier flush` shape to `record_paint_batch_op`. Move `MaskScratch::upload_r8` from its private host-mapped staging buffer to per-batch `BatchUploadArena` staging. Add `MaskScratch::needs_image_grow` and a pre-resize batch flush, mirroring 3F-1's `DstReadback::needs_grow` mitigation. Once the legacy paint paths are gone, remove `RenderPipelineCache::reset_descriptors` + `allocate_descriptor_for_views` + the `descriptor_pool` field that backed them.

**Architecture:** Three structural changes vs 3F-1:

1. **`MaskScratch` decomposes into three primitives.** `upload_r8` currently does ensure-image-size + ensure-staging + copy + `run_one_shot_op(barriers + buffer→image copy)` + `queue_wait_idle` in a single call. Under batching that's incompatible — both the image grow and the embedded one-shot CB break the unsubmitted-CB lifetime invariant. The new shape: `needs_image_grow(w, h) -> bool` (predicate, T1), `ensure_image_size(w, h)` (already exists, becomes `pub`, T2), and `record_upload_r8(vk, cb, src_buffer, src_offset, w, h)` (new, T2). The caller drives orchestration: pre-resize-flush + ensure on `&mut self`, then inside the closure allocates from the arena, copies bytes, and records the upload barrier-copy-barrier into the batch CB. **The staging fields and `allocate_staging` helper disappear** — that role moves entirely to `BatchUploadArena`.

2. **Two pre-resize flush predicates.** `try_vk_render_traps_or_tris` calls both `MaskScratch::ensure_image_size` and `DstReadback::ensure`. Either can grow and destroy old images. The pre-flush check OR-combines `mask_scratch.needs_image_grow(...)` and `dst_readback.needs_grow(...)`; the flush fires if either would grow. Steady-state path (no grow) skips the flush.

3. **Family-B closure body.** The closure adds two recorder steps before the family-A body that 3F-1 established:
   - `let alloc = batch.upload_arena_mut().alloc(needed, 1)?;` — arena allocation. Outer-flag OOM-poison-avoidance pattern from 3C (failure here doesn't poison the batch; reports back via an outer flag).
   - `std::ptr::copy_nonoverlapping` into `alloc.mapped_ptr`.
   - `mask_scratch.record_upload_r8(vk, cb, alloc.buffer, alloc.offset, bbox_w, bbox_h)` — records barrier-copy-barrier into the batch CB.

   The rest (descriptor alloc via `allocate_descriptor_for_views_into`, solid_src clear, dst_readback copy, `record_render_composite`) is identical to 3F-1.

The recorder body (`record_render_composite`) is unchanged. After T3 lands, `try_vk_render_traps_or_tris` is the only RENDER recorder migrated by 3F-2, so T4 can safely delete the legacy `RenderPipelineCache::reset_descriptors` / `allocate_descriptor_for_views` / `descriptor_pool` field.

**Tech Stack:** Rust, ash (Vulkan), existing 3A–3F-1 infrastructure (`PaintBatch`, `BatchUploadArena`, `BatchDescriptorArena`, `record_paint_batch_op`, `paint_resources()`, `renderer_failed` gate, drawable-destruction barriers, audit catalogue, `DstReadback::needs_grow`, `RenderPipelineCache::allocate_descriptor_for_views_into`).

---

## Prerequisite — confirm post-3F-1 baseline

Before T1, verify the tree state:

```bash
cd /home/jos/Projects/yserver
git log --oneline graphics-followups | head -15
rg -n 'record_paint_batch_op|record_paint_op' crates/yserver/src/kms/backend.rs | wc -l
rg -n 'allocate_descriptor_for_views\b' crates/yserver/src/kms/backend.rs
rg -n 'allocate_descriptor_for_views_into' crates/yserver/src/kms/
```

Expected:
- 3F-1 commits landed: `afc18f6` (DstReadback::needs_grow), `3fe108b` (allocate_descriptor_for_views_into), `fade626` (try_vk_render_composite migration), `c4a4965` (pre-flush comment clarity), `3e62044` (3F-1 results doc), `a4f68ab` (status split), plus the diagnostic patch `baad644` and the known-issue commit `7dbd1e6`.
- `allocate_descriptor_for_views\b` returns exactly 1 hit in backend.rs (inside `try_vk_render_traps_or_tris` — the recorder this phase migrates).
- `allocate_descriptor_for_views_into` returns 1 def (`render_pipeline.rs`) + 1 call (`try_vk_render_composite` in backend.rs).
- `cargo test --workspace`, `cargo clippy -p yserver`, `cargo +nightly fmt --check` all green.

If any of the above don't hold, STOP — the prerequisite chain didn't fully land.

## Phase context

Read `docs/superpowers/specs/2026-05-12-rendering-rearchitecture.md` (esp. "Shape of the new code → `PaintBatch`") and `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-1-results.md` for the immediate predecessor's outcomes.

Phase 3F-2 lands the **family-B** half of the render family migration. The HLD's phasing model has phase 3 = "Migrate recorders to `PaintBatch`. One family at a time (fill → copy → image → render → text → traps)." `try_vk_render_traps_or_tris` is the last paint-side recorder on `run_one_shot_op + ProtocolBarrier flush`.

| | `try_vk_render_composite` (3F-1) | `try_vk_render_traps_or_tris` (3F-2) |
|---|---|---|
| Source | Drawable / Solid / Gradient / None | Same (gradient resolver is `_with_gradient_xid` for trap path) |
| Mask | Drawable / Solid / Gradient / None | `MaskScratch` (R8, CPU-rasterised coverage) |
| dst-readback (Disjoint/Conjoint) | YES (via shader binding 2) | YES (same path; uses 3F-1's `DstReadback::needs_grow`) |
| Pre-record uploads | None — all sources are pre-existing | `MaskScratch::upload_r8(bbox_w, bbox_h, coverage_mask)` |
| Family | A (no upload) | B (has upload) |
| Descriptor alloc | `allocate_descriptor_for_views_into` (3F-1 added) | Currently legacy `reset_descriptors + allocate_descriptor_for_views`; T3 switches to `_into`; T4 removes the legacy API |

After 3F-2 lands, all paint-side recorders are on `record_paint_batch_op` / `record_paint_op` and the legacy `run_one_shot_op` sites in `backend.rs` are confined to readback/cursor/scanout-dump handlers + `run_legacy_paint_op` body (the last fallback shim).

### Key invariants 3F-2 inherits

1. **Drop-order**: `KmsBackend.scheduler` before `KmsBackend.ops_command_pool`. Don't touch field order.
2. **Drawable-destruction barriers**: 3B's 5 sites cover dst (windows + pixmaps). No new barriers needed.
3. **`renderer_failed` gate**: every paint entry goes through `paint_resources()`.
4. **Shared `SolidColorImage` invariant (from 3F-1 #6)**: `solid_src_image` is a single backend-wide 1×1 image; multiple `record_solid_color_clear` calls in the same batch are safe because each records barrier-clear-barrier-sample sequentially and `set_current_layout` runs AFTER the barriers go into the CB. `solid_mask_image` is not used by the trap path (the mask is `MaskScratch`).
5. **`record_paint_batch_op` is the load-bearing API**: 3F-2 needs `&mut PaintBatch` inside the closure to call both `batch.upload_arena_mut()` AND `batch.descriptor_arena_mut()`. Use the wide API.
6. **`BatchUploadArena::alloc` failure must use the outer-flag OOM-poison-avoidance pattern** (from 3C T2 fix folded after codex review). The alloc happens BEFORE any CB recording, so the batch state is still untouched — returning `Err` from the closure would poison the batch and discard unrelated prior recorders' work. Pattern (see `upload_bgra_to_mirror`, `backend.rs:2633-2682`):
   ```rust
   let mut arena_oom = false;
   let result = self.scheduler.record_paint_batch_op(vk_arc, pool_handle, |vk, batch, cb| {
       let alloc = match batch.upload_arena_mut().alloc(needed, 1) {
           Ok(a) => a,
           Err(e) => {
               log::warn!("vk render_traps: arena alloc {needed} bytes failed: {e:?}");
               arena_oom = true;
               return Ok(());  // do NOT poison
           }
       };
       // ... copy bytes, record_upload_r8, descriptor alloc (this one CAN poison via `?`)
   });
   if arena_oom { return false; }
   match result { Ok(()) => true, Err(_) => false }
   ```
7. **`MaskScratch` no longer owns staging**. After T2 the struct shrinks to (image, view, image_memory, extent, current_layout). The staging buffer, staging memory, mapping pointer, `ensure_staging`, `allocate_staging`, and the inline `run_one_shot_op` all leave. This is a real API break — `MaskScratch::upload_r8(pool, w, h, bytes)` ceases to exist; the new shape is caller-orchestrated with three methods (`needs_image_grow`, `ensure_image_size`, `record_upload_r8`). The only caller is `try_vk_render_traps_or_tris` (T3 migrates it).
8. **Mask alignment**: `cmd_copy_buffer_to_image` requires `buffer_offset` to be a multiple of the texel block size (1 byte for `R8_UNORM`) AND a multiple of 4 per Vulkan core spec valid-usage `VUID-vkCmdCopyBufferToImage-srcImage-04053`. Pass `alignment = 4` to `arena.alloc(needed, 4)`. (3C's `put_image` uses `alignment = 16`; 1 also works for R8 in practice but 4 is the spec-safe minimum and matches the `BGRA8` paths.)

### Out of scope (deferred to phases 4/5/6)

- Phase 4 — sync rework: retire `vkQueueWaitIdle` from `run_one_shot_op` (the hot-path drain) and from `PaintBatch::submit_and_wait`. Real `VkFence`/timeline semaphore.
- Phase 5 — targeted `VkFence` for `record_get_image` + `GlyphAtlas::intern`.
- Phase 6 — Resource lifetime: batch-owned refcounted handles.

After 3F-2 lands, the **only** uses of `queue_wait_idle` left in `backend.rs`-reachable paint paths are inside `Drop` impls (`MaskScratch::drop`, `SolidColorImage::drop`, etc., which run at backend teardown only) and inside the readback / cursor / scanout-dump one-shot handlers (Phase 5 scope). Hot-path paint is fully drained by `PaintBatch::submit_and_wait`'s single wait.

## File structure

| File | Role | Touched in |
|---|---|---|
| `crates/yserver/src/kms/vk/mask_scratch.rs` | Add `pub fn needs_image_grow`. Make `ensure_image_size` `pub`. Add `pub fn record_upload_r8(&mut self, vk, cb, src_buffer, src_offset, w, h)`. Remove `pub fn upload_r8`, `fn ensure_staging`, `fn allocate_staging`, the `staging_*` fields, and the `run_one_shot_op` import. | T1 + T2 |
| `crates/yserver/src/kms/backend.rs` | Migrate `try_vk_render_traps_or_tris` (~lines 4731–5159); update `run_legacy_paint_op` audit catalogue entry; remove the stale `3D-deferred: render-traps needs...` comment. | T3 |
| `crates/yserver/src/kms/vk/render_pipeline.rs` | Remove `pub fn reset_descriptors`, `pub fn allocate_descriptor_for_views`, the `descriptor_pool` field, the descriptor-pool create in `new`, the `destroy_descriptor_pool` in `Drop`, the `MAX_DESCRIPTOR_SETS_PER_FRAME` const. | T4 |
| `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-2-results.md` | Results doc | T5 |

## Pre-task notes (read before starting)

1. **The borrow split inside the migrated `try_vk_render_traps_or_tris` is the load-bearing structural change.** Disjoint fields needed simultaneously:
   - `&mut self.scheduler` (the `record_paint_batch_op` receiver).
   - `&mut DrawableImage` for `dst_mirror` (via `self.windows.get_mut` / `self.pixmaps.get_mut`).
   - `&mut SolidColorImage` for `solid_src_image` (via `self.solid_src_image.as_mut()`).
   - `&mut MaskScratch` for the new arena-aware `record_upload_r8` call (via `self.mask_scratch.as_mut()`).
   - `Option<&mut DstReadback>` for `dst_readback` (via `self.dst_readback.as_mut()`).
   - `&self.render_pipelines` (shared) — to call `allocate_descriptor_for_views_into` inside the closure.

   All field paths must be `self.X` directly (NOT through a `&mut self` helper). The borrow checker accepts disjoint field projection but not helper methods.

2. **`pipeline` and `pipeline_layout` are NOT borrows.** Same as 3F-1 — `RenderPipelineCache::get()` returns `vk::Pipeline` (`Copy`); `pipeline_layout()` returns `vk::PipelineLayout` (`Copy`). Read them via transient `&mut self.render_pipelines` and `&self.render_pipelines`; both borrows end at the `?`/`;`.

3. **Pre-resize flush ordering**: the flush MUST happen BEFORE the mut borrows of dst_mirror / dst_readback / solid_src / mask_scratch are acquired. Combined predicate:

   ```rust
   let needs_mask_grow = self.mask_scratch.as_ref().is_some_and(|m| m.needs_image_grow(bbox_w, bbox_h));
   let needs_readback_grow = needs_dst_readback
       && self.dst_readback.as_ref().is_some_and(|r| r.needs_grow(dst_format, dst_extent.width, dst_extent.height));
   if needs_mask_grow || needs_readback_grow {
       use crate::kms::scheduler::paint_batch::BatchFlushReason;
       if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
           log::warn!("vk render_traps: pre-resize flush failed: {e:?}");
           return false;
       }
   }
   ```

   Both checks are shared borrows; they end at the `if` boundary. Then the `&mut self`-taking `flush_if_needed` runs cleanly.

4. **`dst_readback.view()` is `&mut self.dst_readback`** because it lazily creates the no-alpha view. Call it AFTER `ensure()` (which is also `&mut`). Both borrows are transient.

5. **`mask_scratch.ensure_image_size(...)` happens BEFORE the closure** — `&mut self.mask_scratch` is taken for the ensure call, then released. Inside the closure, the same `&mut self.mask_scratch` is re-borrowed (via direct field projection) for the `record_upload_r8` call. The two borrows don't overlap because the ensure call's `&mut` ends at the `;`.

6. **No new destruction barriers needed.** Same as 3F-1.

7. **OOM-poison concern (real here)**. Arena alloc failure must use the outer-flag pattern (Invariant 6 above). Descriptor alloc failure CAN poison via `?` — that's after CB recording has started, so poisoning is the right state transition. The pre-resize flush failure is detected BEFORE the closure runs; on failure the function early-returns without touching the batch.

8. **Record-time CPU layout tracking under poisoning.** `MaskScratch::record_upload_r8` advances `self.current_layout` immediately after recording the final barrier into the CB. If a later `?` in the closure poisons the batch, the CB never executes — but the CPU-side `current_layout` already reflects the would-be-executed state. The next batch's `record_upload_r8` would then emit a wrong-old-layout barrier (transitioning from `SHADER_READ_ONLY` when the GPU actually has the image in the prior layout). This is the same pattern `SolidColorImage` and the drawable mirrors already follow; the subsystem accepts it because the alternative (mirroring submit/retire-time layout updates from the CPU side) is much more invasive. Worth noting because 3F-2 brings MaskScratch into this contract for the first time; mention in the results doc that this is a known limitation shared with the other paint-side resources, with Phase 6 (batch-owned refcounted handles) as the eventual structural fix.

9. **Test coverage**: no direct unit test for `try_vk_render_traps_or_tris`. Coverage is xts5 + rendercheck + hardware smoke. T5 hardware smoke is the gate. The adapta-nokto + mate-cc reproducer in `docs/known-issues.md` is a natural workload for "did this phase land cleanly?" — a successful T5 smoke should show **noticeably reduced lag** even before phase 5 lands.

9. **clippy / fmt**: plain `cargo clippy -p yserver`, `cargo +nightly fmt`. 5 pre-existing `doc_lazy_continuation` warnings; no new ones.

---

## Task 1: Add `MaskScratch::needs_image_grow` accessor

**Goal:** Expose the image-grow predicate so callers can detect "would the next `ensure_image_size(w, h)` call reallocate?" without mutating state.

**Files:**
- Modify: `crates/yserver/src/kms/vk/mask_scratch.rs`

### Step 1: Read the current `ensure_image_size` body

- [ ] **Step 1: Read `mask_scratch.rs:100-121`**

The grow predicate lives implicitly in `ensure_image_size`:

```rust
if width <= self.extent.width && height <= self.extent.height {
    return Ok(());
}
// otherwise: allocate + queue_wait_idle + destroy old + swap
```

`needs_image_grow` is the negation of that early-return.

### Step 2: Add the accessor

- [ ] **Step 2: Append to `impl MaskScratch`, immediately above `ensure_image_size`**

```rust
    /// True if a later `ensure_image_size(width, height)` call would
    /// reallocate the per-format scratch image. Callers in batched
    /// paint paths use this BEFORE entering `record_paint_batch_op`
    /// so they can flush any in-flight batch — `ensure_image_size`
    /// destroys the old image after `queue_wait_idle`, which does
    /// NOT wait for un-submitted commands. Without a pre-flush, an
    /// open batch CB embedding the old scratch image would dangle.
    /// Mirrors `DstReadback::needs_grow` (3F-1) and
    /// `CopyScratch::needs_grow` (3D).
    pub fn needs_image_grow(&self, width: u32, height: u32) -> bool {
        width > self.extent.width || height > self.extent.height
    }
```

### Step 3: Build

- [ ] **Step 3: `cargo check -p yserver`**

Expected: clean.

### Step 4: Tests + fmt + clippy

- [ ] **Step 4: `cargo test -p yserver --lib 2>&1 | tail -5`**

Expected: 138 passed, 0 failed, 3 ignored (unchanged from post-3F-1 baseline).

- [ ] **Step 5: `cargo +nightly fmt --check`**

Expected: no diff.

- [ ] **Step 6: `cargo clippy -p yserver 2>&1 | tail -10`**

Expected: 5 pre-existing `doc_lazy_continuation` warnings; no new ones.

### Step 5: Commit T1

- [ ] **Step 7: Commit**

```bash
git add crates/yserver/src/kms/vk/mask_scratch.rs
git commit -m "$(cat <<'EOF'
refactor(kms): add MaskScratch::needs_image_grow accessor

3F-2 prep. Callers in batched paint paths need to detect "would
ensure_image_size() grow this scratch?" before acquiring any mut
borrows, so they can pre-flush the open PaintBatch —
ensure_image_size() destroys the old image after queue_wait_idle,
which does NOT wait for un-submitted commands. Without the
pre-flush, an open batch CB referencing the old scratch would
dangle. Same hazard 3D fixed for CopyScratch and 3F-1 for
DstReadback.

Pure read; no behaviour change.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Migrate `MaskScratch::upload_r8` to per-batch arena staging

**Goal:** Drop `MaskScratch`'s private host-mapped staging buffer and the embedded `run_one_shot_op` inside `upload_r8`. The caller orchestrates: it pre-flushes if needed, calls `ensure_image_size` directly (now `pub`), allocates from `BatchUploadArena` inside the closure, copies bytes, and calls a new `record_upload_r8(vk, cb, src_buffer, src_offset, w, h)` recorder that records the barrier-copy-barrier into the supplied CB.

**Files:**
- Modify: `crates/yserver/src/kms/vk/mask_scratch.rs`

### Step 1: Read the current `upload_r8` body

- [ ] **Step 1: Re-read `mask_scratch.rs:149-233`**

Identify the three sub-phases of the current `upload_r8`:
1. `ensure_image_size(w, h)?` — image grow.
2. `ensure_staging(needed)?` + `copy_nonoverlapping` into `self.staging_mapped` — staging fill.
3. `run_one_shot_op(...) { barrier old_layout → TRANSFER_DST + cmd_copy_buffer_to_image(buffer = self.staging_buffer) + barrier TRANSFER_DST → SHADER_READ_ONLY }` — record + submit + waitidle.
4. `self.current_layout = SHADER_READ_ONLY_OPTIMAL`.

After T2: (1) is the caller's responsibility (call `ensure_image_size` directly, now public, on `&mut self.mask_scratch`); (2) is the caller's responsibility (arena alloc + copy); (3) becomes the new `record_upload_r8(vk, cb, src_buffer, src_offset, w, h, &mut self)` — same barrier-copy-barrier sequence, but written into the supplied CB instead of a one-shot.

### Step 2: Make `ensure_image_size` public + extract the grow side from the upload path

- [ ] **Step 2: Edit `mask_scratch.rs:100`**

Change:
```rust
fn ensure_image_size(&mut self, width: u32, height: u32) -> Result<(), MaskScratchError> {
```

To:
```rust
/// Ensure the scratch image is at least `(width, height)` pixels,
/// reallocating if smaller. After this returns the image is in
/// `UNDEFINED` layout (treated as new) when reallocation happens.
///
/// 3F-2: callers in batched paint paths MUST pre-flush the open
/// PaintBatch with `BatchFlushReason::ProtocolBarrier` before
/// calling this if `needs_image_grow(width, height)` returns
/// `true`. The grow path destroys the old image after
/// `queue_wait_idle`, which does NOT wait for un-submitted
/// commands.
pub fn ensure_image_size(&mut self, width: u32, height: u32) -> Result<(), MaskScratchError> {
```

(`pub` is the only behavioural change; doc-comment added so future readers understand the contract.)

### Step 3: Add the new `record_upload_r8` recorder

- [ ] **Step 3: Append to `impl MaskScratch`, replacing the entire old `upload_r8` method**

Delete the old `pub fn upload_r8(&mut self, pool, w, h, bytes) -> Result<(), MaskScratchError>` (lines 145-233). Replace with:

```rust
    /// Record the barrier-copy-barrier sequence that uploads
    /// `width × height` R8 pixels from `src_buffer + src_offset`
    /// to the scratch image's top-left `(0, 0)` rect, into the
    /// supplied CB. After this returns the image's `current_layout`
    /// reflects `SHADER_READ_ONLY_OPTIMAL` (the CB's terminal
    /// transition); on CB execute the image lands in that layout.
    ///
    /// Caller is responsible for:
    ///   1. Calling `ensure_image_size(width, height)?` BEFORE this
    ///      method (after any required pre-resize batch flush, per
    ///      `needs_image_grow`).
    ///   2. Allocating staging via `BatchUploadArena::alloc(width *
    ///      height, 4)` and copying the row-major coverage bytes
    ///      into the returned `mapped_ptr`.
    ///   3. Passing the resulting `buffer` + `offset` here.
    ///
    /// `width` / `height` must be ≤ `self.extent` (no grow inside
    /// this method); zero-sized rects no-op.
    pub fn record_upload_r8(
        &mut self,
        vk: &VkContext,
        cb: vk::CommandBuffer,
        src_buffer: vk::Buffer,
        src_offset: u64,
        width: u32,
        height: u32,
    ) {
        if width == 0 || height == 0 {
            return;
        }
        debug_assert!(
            width <= self.extent.width && height <= self.extent.height,
            "MaskScratch::record_upload_r8: caller must ensure_image_size first",
        );
        let device = &vk.device;
        let old_layout = self.current_layout;
        let to_dst = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .old_layout(old_layout)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .image(self.image)
            .subresource_range(color_subresource_range())];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&to_dst);
        unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

        let region = vk::BufferImageCopy::default()
            .buffer_offset(src_offset)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(
                vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
            )
            .image_offset(vk::Offset3D::default())
            .image_extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            });
        let regions = [region];
        unsafe {
            device.cmd_copy_buffer_to_image(
                cb,
                src_buffer,
                self.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );
        }

        let to_read = [vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::FRAGMENT_SHADER)
            .dst_access_mask(vk::AccessFlags2::SHADER_SAMPLED_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .image(self.image)
            .subresource_range(color_subresource_range())];
        let dep = vk::DependencyInfo::default().image_memory_barriers(&to_read);
        unsafe { device.cmd_pipeline_barrier2(cb, &dep) };

        self.current_layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
    }
```

(The body is byte-for-byte the same `cmd_pipeline_barrier2 + cmd_copy_buffer_to_image + cmd_pipeline_barrier2` sequence the old `upload_r8` recorded — only the staging buffer/offset come from the caller now, and there's no enclosing `run_one_shot_op`. CPU-side `set_current_layout` happens AFTER the barriers are recorded, matching the `record_solid_color_clear` invariant from 3F-1 #6: multiple `record_upload_r8` calls in the same batch are safe because each records its own barrier-clear-barrier sequentially.)

### Step 4: Strip the staging-side fields + helpers

- [ ] **Step 4: Remove the four staging fields from the struct (line 43-46)**

Change:
```rust
pub struct MaskScratch {
    vk: Arc<VkContext>,
    image: vk::Image,
    view: vk::ImageView,
    image_memory: vk::DeviceMemory,
    extent: vk::Extent2D,
    current_layout: vk::ImageLayout,
    staging_buffer: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    staging_mapped: NonNull<u8>,
    staging_size: u64,
}
```

To:
```rust
pub struct MaskScratch {
    vk: Arc<VkContext>,
    image: vk::Image,
    view: vk::ImageView,
    image_memory: vk::DeviceMemory,
    extent: vk::Extent2D,
    current_layout: vk::ImageLayout,
}
```

- [ ] **Step 5: Strip the initialiser in `new`**

In `MaskScratch::new` (line 53), drop the `allocate_staging` call and its error-cleanup branch (`let initial_staging = ...; let (staging_buffer, ...) = allocate_staging(...)`) plus the corresponding `Ok(Self { ... staging_buffer, ... })` lines. After the edit, `new` reads roughly:

```rust
pub fn new(vk: Arc<VkContext>) -> Result<Self, MaskScratchError> {
    let extent = vk::Extent2D {
        width: 256,
        height: 256,
    };
    let (image, view, image_memory) = allocate_image(&vk, extent)?;
    Ok(Self {
        vk,
        image,
        view,
        image_memory,
        extent,
        current_layout: vk::ImageLayout::UNDEFINED,
    })
}
```

(Note: the `match allocate_staging` cleanup branch that destroyed the just-allocated image on staging-alloc failure is no longer needed — staging doesn't exist here. The `allocate_image?` call's `?` is the only failure point.)

- [ ] **Step 6: Strip `ensure_staging` (lines 123-143) and `allocate_staging` (lines 334-394)**

Delete both functions entirely. Their callers are gone after the `upload_r8` removal in Step 3.

- [ ] **Step 7: Strip the staging cleanup in `Drop` (lines 237-247)**

Change:
```rust
impl Drop for MaskScratch {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.unmap_memory(self.staging_memory);
            self.vk.device.destroy_buffer(self.staging_buffer, None);
            self.vk.device.free_memory(self.staging_memory, None);
            self.vk.device.destroy_image_view(self.view, None);
            self.vk.device.destroy_image(self.image, None);
            self.vk.device.free_memory(self.image_memory, None);
        }
    }
}
```

To:
```rust
impl Drop for MaskScratch {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
            self.vk.device.destroy_image_view(self.view, None);
            self.vk.device.destroy_image(self.image, None);
            self.vk.device.free_memory(self.image_memory, None);
        }
    }
}
```

(The `queue_wait_idle` on Drop is still right — `Drop` only runs at backend teardown when no in-flight CBs reference the image. It's not part of any paint hot path.)

- [ ] **Step 8: Strip the now-unused imports**

Remove from the top of the file:
- `use std::ptr::NonNull;` (was used by `staging_mapped`)
- `use super::{device::VkContext, ops::run_one_shot_op};` becomes `use super::device::VkContext;` (the new `record_upload_r8` does NOT need `run_one_shot_op`).

Also remove the now-unused `unsafe impl Send` / `unsafe impl Sync` for MaskScratch (the only fields needing the SAFETY argument were the staging mapping pointer — `NonNull<u8>` isn't Send/Sync by default. Once that's gone, the remaining fields are all `Send + Sync` through normal blanket impls, so the manual `unsafe impl` becomes dead code AND a `safe_code_in_unsafe_impl` lint trigger). The struct will still be Send+Sync automatically.

  - Delete lines:
    ```rust
    unsafe impl Send for MaskScratch {}
    unsafe impl Sync for MaskScratch {}
    ```

### Step 5: Build

- [ ] **Step 9: `cargo check -p yserver`**

Expected: clean. If `unused_import` warnings appear for `NonNull` or `run_one_shot_op` — go back to Step 8.

### Step 6: Tests + fmt + clippy

- [ ] **Step 10: `cargo test -p yserver --lib 2>&1 | tail -5`**

Expected: 138 passed.

- [ ] **Step 11: `cargo +nightly fmt --check`**

Expected: no diff.

- [ ] **Step 12: `cargo clippy -p yserver 2>&1 | tail -10`**

Expected: 5 pre-existing warnings, no new ones. If clippy flags the freed manual `Send + Sync` impl, that's expected — already removed in Step 8.

### Step 7: Commit T2

- [ ] **Step 13: Commit**

```bash
git add crates/yserver/src/kms/vk/mask_scratch.rs
git commit -m "$(cat <<'EOF'
refactor(kms): decompose MaskScratch::upload_r8 for batched recording

3F-2 prep. MaskScratch ceases to own its private host-mapped
staging buffer; staging moves to per-batch BatchUploadArena. The
upload primitive splits into three: needs_image_grow (T1 +
public), ensure_image_size (now public; pre-flush contract
documented), and record_upload_r8 (records barrier-copy-barrier
into a caller-supplied CB from a caller-supplied source buffer +
offset). The old upload_r8(pool, w, h, bytes) was incompatible
with batched recording on two counts: image grow's
queue_wait_idle doesn't wait for un-submitted CBs, and the
embedded run_one_shot_op forced a synchronous drain per call.

The staging fields (staging_buffer / staging_memory /
staging_mapped / staging_size) and the ensure_staging /
allocate_staging helpers are deleted; the manual Send/Sync impls
follow (no NonNull pointer field remaining).

No caller changes here — try_vk_render_traps_or_tris is the sole
caller and migrates in T3, which is where the orchestration
moves.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Migrate `try_vk_render_traps_or_tris` to `record_paint_batch_op`

**Goal:** Replace the `mask_scratch.upload_r8(pool, ...) + reset_descriptors() + allocate_descriptor_for_views(...) + pre-record ProtocolBarrier flush + run_one_shot_op(...)` shape with `paint_resources()` + needs-grow pre-flush + `self.scheduler.record_paint_batch_op(...)`. The mask coverage staging lands in `batch.upload_arena_mut()` inside the closure. Descriptor allocation uses 3F-1's `allocate_descriptor_for_views_into`.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (`try_vk_render_traps_or_tris`, ~lines 4731–5159; line numbers are approximate, use the anchor strings as the source of truth)

### Step 1: Read the existing function

- [ ] **Step 1: Read backend.rs lines 4731–5159**

Disposition table (same orientation note as 3F-1 — line numbers approximate):

| What | Disposition |
|---|---|
| `use` for `run_one_shot_op`, `record_solid_color_clear`, `StdPictOp` | Drop `run_one_shot_op` from the use list. Keep `vk_render` + `record_solid_color_clear` + `StdPictOp`. |
| empty-bbox / empty-mask early return | Keeps. |
| StdPictOp check | Keeps. |
| `resolve_render_pic_with_gradient_xid` | Keeps. |
| dst pictures resolve + clip clone | Keeps. |
| self-composite alias check | Keeps. |
| raw `vk_arc` + `pool_handle` bindings | **REPLACED** by `paint_resources()`. |
| pipeline/scratch presence checks + `needs_dst_readback` | Keeps. |
| dst format / extent / depth resolution | Keeps. |
| `ensure_drawable_mirror_sampleable` for src | Keeps. |
| drawable src format gating | Keeps. |
| **`mask_scratch.upload_r8(pool_handle, bbox_w, bbox_h, coverage_mask)`** (~lines 4849-4859) | **REPLACED**: mask upload moves INSIDE the closure (arena alloc + copy + `record_upload_r8`). The `ensure_image_size` part moves out, gated on `needs_image_grow + pre-flush`. |
| `render_pipelines.get(...)` + `pipeline_layout()` + **`reset_descriptors()`** | **`reset_descriptors` REMOVED**. Keep `get` + `pipeline_layout`. |
| `solid_src_view` / `mask_view` / `mask_extent` reads (~lines 4891-4901) | **REORDERED**: `mask_view` and `mask_extent` MUST be re-fetched AFTER the new `mask_scratch.ensure_image_size(...)` call (Step 5 below) — `ensure_image_size` destroys the old `vk::ImageView` on grow (see `mask_scratch.rs:111`), so a pre-ensure fetch would dangle. `solid_src_view` is unaffected. |
| src/mask/solid view resolution | Keeps. |
| src view resolution (R8 mask / depth-24 / regular paths) | Keeps. |
| `dst_readback.ensure` + `view` | **REORDERED** with new pre-flush gating (combined needs_grow). |
| `allocate_descriptor_for_views(...)` (legacy shared-pool) | **REPLACED**: descriptor set is allocated inside the closure from `batch.descriptor_arena_mut()`. |
| xform composition + repeat translation + `CompositeAttrs` build | Keeps. |
| `3D-deferred: render-traps needs...` comment + `flush_if_needed(ProtocolBarrier)` block (~5096-5106) | **DELETED.** |
| re-borrow dst_mirror / solid_src / dst_readback for the closure | Keeps (the borrows themselves; the captures move into the new closure). |
| `run_one_shot_op(...) { record_solid_color_clear(s) + dst_readback.record_copy_from + record_render_composite }` | **REPLACED** by `self.scheduler.record_paint_batch_op(...)` with arena alloc + copy + record_upload_r8 + descriptor alloc INSIDE the closure. |

### Step 2: Replace the early raw `vk_arc` / `pool_handle` binding

- [ ] **Step 2: Replace lines ~4779–4784**

Find:

```rust
        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return false;
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return false;
        };
```

Replace with:

```rust
        // 3F-2: acquire batch resources up-front (gated by
        // renderer_failed). Same shape try_vk_render_composite uses
        // since 3F-1.
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            log::debug!(
                "vk render_traps bail: paint_resources unavailable (renderer_failed or vk/pool absent) (dst=0x{dst_xid:x})"
            );
            return false;
        };
```

### Step 3: Remove the `mask_scratch.upload_r8(...)` call and the `reset_descriptors` call; insert combined pre-resize flush

- [ ] **Step 3: Delete the inline mask upload (~lines 4849–4859)**

Find:

```rust
        // Upload CPU-rasterised mask first — independent of the
        // composite recording. Resizes the scratch on demand.
        if let Err(e) = self
            .mask_scratch
            .as_mut()
            .expect("checked above")
            .upload_r8(pool_handle, bbox_w, bbox_h, coverage_mask)
        {
            log::warn!("vk render_traps: mask upload failed: {e:?}");
            return false;
        }
```

Delete the entire block. The arena-driven upload happens inside the closure (Step 6).

- [ ] **Step 4: Delete the `reset_descriptors` block (~lines 4881–4889)**

Find:

```rust
        if let Err(e) = self
            .render_pipelines
            .as_ref()
            .expect("checked above")
            .reset_descriptors()
        {
            log::warn!("vk render_traps: descriptor pool reset failed: {e:?}");
            return false;
        }
```

Delete the entire block.

- [ ] **Step 5: Insert combined pre-resize flush BEFORE `dst_readback.ensure`**

Locate the `let dst_readback_view = if needs_dst_readback {` block (~line 5004). Immediately ABOVE it, insert:

```rust
        // 3F-2: both MaskScratch::ensure_image_size and
        // DstReadback::ensure may grow (destroy old image after
        // queue_wait_idle), which does NOT wait for un-submitted
        // batch commands. Pre-flush the batch before either grow.
        // needs_*_grow are false for the steady-state path (no
        // resize), so this only fires on first use of a given size
        // (no old image to dangle, harmless) or on a real grow.
        // Same mitigation 3D applied to CopyScratch and 3F-1 to
        // DstReadback.
        let needs_mask_grow = self
            .mask_scratch
            .as_ref()
            .is_some_and(|m| m.needs_image_grow(bbox_w, bbox_h));
        let needs_readback_grow = needs_dst_readback
            && self
                .dst_readback
                .as_ref()
                .is_some_and(|r| r.needs_grow(dst_format, dst_extent.width, dst_extent.height));
        if needs_mask_grow || needs_readback_grow {
            use crate::kms::scheduler::paint_batch::BatchFlushReason;
            if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
                log::warn!("vk render_traps: pre-resize flush failed: {e:?}");
                return false;
            }
        }

        // 3F-2: ensure_image_size now happens outside the closure
        // (it's `&mut self.mask_scratch` and takes the
        // post-pre-flush window). The recorder below uses the
        // already-sized image and only records the upload's
        // barrier-copy-barrier.
        if let Err(e) = self
            .mask_scratch
            .as_mut()
            .expect("checked above")
            .ensure_image_size(bbox_w, bbox_h)
        {
            log::warn!("vk render_traps: mask ensure_image_size failed: {e:?}");
            return false;
        }
```

(The mask sizes are now guaranteed `≥ bbox_w × bbox_h` by `ensure_image_size`; the closure can safely call `record_upload_r8` without re-checking.)

**P1 follow-on**: the existing `mask_view = ...image_view()` + `mask_extent = ...extent()` reads at ~lines 4896-4901 currently sit BEFORE the insertion point above. Move them to immediately AFTER this `ensure_image_size` block, so the captured `vk::ImageView` reflects the post-grow image (a pre-grow fetch would dangle when `ensure_image_size` destroys the old view via `mask_scratch.rs:111`'s `destroy_image_view(self.view, ...)`). `solid_src_view` can stay at its current position — it isn't affected by mask grow. Concretely, after this step the order should be:

```rust
// 1. needs_grow checks + conditional pre-flush (this step)
// 2. self.mask_scratch.as_mut().expect("...").ensure_image_size(bbox_w, bbox_h)?;
// 3. let mask_view = self.mask_scratch.as_ref().expect("...").image_view();
// 4. let mask_extent = self.mask_scratch.as_ref().expect("...").extent();
// 5. (existing dst_readback ensure + view + descriptor + xform + CompositeAttrs)
// 6. (closure captures + record_paint_batch_op)
```

The `solid_src_view` read at ~line 4891 can stay where it is (it doesn't depend on the mask grow). When verifying with `cargo check`, an "use of moved value" or "dangling reference" diagnostic on `mask_view` is the symptom of forgetting to move it.

### Step 4: Delete the legacy `allocate_descriptor_for_views` call

- [ ] **Step 6: Remove the descriptor-set alloc block (~lines 5021-5032)**

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
                log::warn!("vk render_traps: descriptor alloc failed: {e:?}");
                return false;
            }
        };
```

Delete the entire block. The descriptor set will be allocated inside the closure (Step 7).

### Step 5: Delete the stale `3D-deferred: render-traps...` comment + unconditional pre-record flush

- [ ] **Step 7: Remove the legacy pre-record flush block (~lines 5096–5106)**

Find:

```rust
        // 3D-deferred: render-traps needs per-batch MaskScratch + descriptor
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

Delete the entire block. The conditional needs-grow flush from Step 5 replaces it.

### Step 6: Replace the run_one_shot_op call with record_paint_batch_op

- [ ] **Step 8: Replace lines ~5125–5158 (the `match run_one_shot_op(...) { ... }` block)**

Find:

```rust
        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            if let Some(color) = src_clear_color {
                record_solid_color_clear(vk, cb, solid_src_image, color);
            }
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
                &rects,
                scissor,
            )
        }) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("vk render_traps: record failed on dst xid {dst_xid:#x}: {e:?}");
                false
            }
        }
    }
```

Replace with:

```rust
        // 3F-2: mask upload + descriptor alloc move into the closure.
        // `render_cache` is a shared borrow on self.render_pipelines —
        // disjoint from &mut self.scheduler and the other &mut field
        // captures.
        //
        // Arena alloc failure uses the outer-flag pattern (3C T2):
        // failure happens BEFORE any CB recording, so a poisoned-batch
        // return would discard unrelated 3B/3C/3E/3F-1 work already
        // recorded in this batch. Set `arena_oom = true`, return Ok,
        // and report failure to the caller after `record_paint_batch_op`
        // returns.
        let render_cache = self.render_pipelines.as_ref().expect("checked above");
        let mut arena_oom = false;
        let mask_bytes = coverage_mask;
        let mask_w = bbox_w;
        let mask_h = bbox_h;
        let mask_len = mask_bytes.len();
        let result = self.scheduler.record_paint_batch_op(
            vk_arc,
            pool_handle,
            |vk, batch, cb| {
                let needed = u64::from(mask_w) * u64::from(mask_h);
                let alloc = match batch.upload_arena_mut().alloc(needed, 4) {
                    Ok(a) => a,
                    Err(e) => {
                        log::warn!(
                            "vk render_traps: arena alloc {needed} bytes failed: {e:?} — \
                             mask upload will fail without poisoning batch"
                        );
                        arena_oom = true;
                        return Ok(());
                    }
                };
                // SAFETY: alloc.mapped_ptr is HOST_VISIBLE |
                // HOST_COHERENT mapped at alloc.buffer + alloc.offset
                // covering `needed` bytes; mask_bytes is valid for
                // mask_len = needed bytes (debug-asserted in the
                // recorder upstream as `coverage_mask.len() == w * h`).
                debug_assert_eq!(mask_len, needed as usize);
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        mask_bytes.as_ptr(),
                        alloc.mapped_ptr.as_ptr(),
                        mask_len,
                    );
                }
                mask_scratch.record_upload_r8(
                    vk,
                    cb,
                    alloc.buffer,
                    alloc.offset,
                    mask_w,
                    mask_h,
                );

                let descriptor_set = render_cache.allocate_descriptor_for_views_into(
                    batch.descriptor_arena_mut(),
                    src_view,
                    mask_view,
                    dst_readback_view,
                )?;

                if let Some(color) = src_clear_color {
                    record_solid_color_clear(vk, cb, solid_src_image, color);
                }
                // Disjoint/Conjoint: snapshot dst into the readback
                // scratch so the shader can sample it at binding 2.
                // Mirrors try_vk_render_composite's sequencing.
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
                    &rects,
                    scissor,
                )
            },
        );
        if arena_oom {
            return false;
        }
        match result {
            Ok(()) => true,
            Err(e) => {
                log::warn!(
                    "vk render_traps: record failed on dst xid {dst_xid:#x}: \
                     {e:?} — falling back to pixman"
                );
                false
            }
        }
    }
```

(`mask_scratch` is the captured `&mut MaskScratch` re-borrow. The capture chain just below the pre-flush in Step 5 needs to add `mask_scratch` to the disjoint-field list — done in Step 9 below.)

### Step 7: Acquire the closure captures, including `&mut mask_scratch`

- [ ] **Step 9: Verify the closure-capture acquisition block immediately above the replaced `record_paint_batch_op` call**

The existing code at ~lines 5108–5123 acquires `dst_mirror`, `solid_src_image`, `dst_readback`. Confirm — and adjust if necessary — that the block reads:

```rust
        let dst_mirror = if let Some(w) = self.windows.get_mut(&dst_xid) {
            w.vk_mirror.as_mut()
        } else if let Some(p) = self.pixmaps.get_mut(&dst_xid) {
            p.vk_mirror.as_mut()
        } else {
            None
        };
        let Some(dst_mirror) = dst_mirror else {
            return false;
        };
        let solid_src_image = self.solid_src_image.as_mut().expect("checked above");
        let mask_scratch = self.mask_scratch.as_mut().expect("checked above");
        let dst_readback = if needs_dst_readback {
            Some(self.dst_readback.as_mut().expect("checked above"))
        } else {
            None
        };
```

The only addition vs the pre-3F-2 code is the `let mask_scratch = self.mask_scratch.as_mut().expect(...);` line. Insert it between the `solid_src_image` and the `dst_readback` bindings.

### Step 8: Clean up the `use` line at the top of the function

- [ ] **Step 10: Drop `run_one_shot_op` from the use list at ~line 4742**

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

### Step 9: Update the audit catalogue

- [ ] **Step 11: Find the `run_legacy_paint_op` doc-comment block**

Run: `grep -n "render_composite\|render_traps" crates/yserver/src/kms/backend.rs | head -5`

The doc-comment block at ~line 1720 currently reads (from 3F-1):

```
///   try_vk_render_traps (composite):  render::record_render_composite     — borrow-conflict fallback (3F-2)
///   try_vk_render_composite_glyphs:   text::record_text_run               — migrated 3E T2 (record_paint_op)
///   try_vk_render_composite:          render::record_render_composite     — migrated 3F-1 (record_paint_batch_op + BatchDescriptorArena)
```

- [ ] **Step 12: Update the traps row and fix the label**

Replace with:

```
///   try_vk_render_traps_or_tris:      render::record_render_composite     — migrated 3F-2 (record_paint_batch_op + arena upload + arena descriptors)
///   try_vk_render_composite_glyphs:   text::record_text_run               — migrated 3E T2 (record_paint_op)
///   try_vk_render_composite:          render::record_render_composite     — migrated 3F-1 (record_paint_batch_op + BatchDescriptorArena)
```

(The label fix `try_vk_render_traps (composite)` → `try_vk_render_traps_or_tris` is the audit-catalogue followup noted in 3F-1 results.)

### Step 10: Build

- [ ] **Step 13: `cargo check -p yserver`**

Expected: clean.

Troubleshooting (similar to 3F-1's table — see plan):

| Compiler complaint | Cause | Fix |
|---|---|---|
| `cannot borrow self.scheduler as mutable because self.* is already borrowed` | A captured value is still holding a borrow on `self.X` outside the closure | Confirm `dst_mirror` / `solid_src_image` / `mask_scratch` / `dst_readback` / `render_cache` are bound via direct field paths (NOT helper methods on `&mut self`) |
| `cannot borrow self.render_pipelines as mutable because it is also borrowed as immutable` | The shared `render_cache` borrow is held across a `self.render_pipelines.as_mut()` call | Bind `render_cache` AFTER all `self.render_pipelines.as_mut()` reads finish |
| `mismatched types: expected vk::Result, found ArenaError` | Closure's `?` propagates `vk::Result`, but `arena.alloc` returns `ArenaError` | The outer-flag pattern AVOIDS `?` on arena alloc — use the `match { Ok / Err }` shown in Step 8. Only `?` on `vk::Result`-returning calls (`allocate_descriptor_for_views_into`, `record_render_composite`) |
| `mask_bytes does not live long enough` | Closure capture lifetime conflict — `coverage_mask` is `&[u8]` with the function's lifetime; the closure borrows it by reference | The pattern in Step 8 binds `mask_bytes = coverage_mask` (a `&[u8]` copy) before the closure; the closure captures the local binding. Both live for the function call |

### Step 11: Tests + fmt + clippy

- [ ] **Step 14: `cargo test -p yserver --lib 2>&1 | tail -5`**

Expected: 138 passed.

- [ ] **Step 15: `cargo +nightly fmt --check`**

Expected: no diff.

- [ ] **Step 16: `cargo clippy -p yserver 2>&1 | tail -10`**

Expected: 5 pre-existing warnings, no new ones.

### Step 12: Commit T3

- [ ] **Step 17: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "$(cat <<'EOF'
refactor(kms): migrate try_vk_render_traps_or_tris to record_paint_batch_op

The last paint-side recorder on the legacy run_one_shot_op +
pre-record ProtocolBarrier flush shape moves to
record_paint_batch_op. RENDER Trapezoids/Triangles now pack into
the open PaintBatch alongside fill / copy / put_image / text /
render-composite.

Mask coverage upload moves from MaskScratch's private staging
buffer (which embedded a queue_wait_idle per call) to per-batch
BatchUploadArena staging. The barrier-copy-barrier sequence is
recorded into the open batch CB via MaskScratch::record_upload_r8;
the arena chunk lives until batch retirement so the un-submitted
CB safely references it.

Descriptor set is allocated inside the closure from
batch.descriptor_arena_mut() (same path try_vk_render_composite
took in 3F-1). The unconditional pre-record ProtocolBarrier flush
is gone; the only remaining flush in this path is the conditional
needs_*_grow gate covering both MaskScratch image growth and
DstReadback resize.

Audit catalogue updated: try_vk_render_traps_or_tris is now
marked migrated 3F-2, and the row label is corrected from
"try_vk_render_traps (composite)" to the real function name.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Remove the legacy `RenderPipelineCache` shared-pool API

**Goal:** With `try_vk_render_traps_or_tris` migrated in T3, no caller invokes the legacy `RenderPipelineCache::reset_descriptors` or `allocate_descriptor_for_views` any more. Remove both methods, the `descriptor_pool` field, the pool-create in `new`, the pool-destroy in `Drop`, and the now-orphaned `MAX_DESCRIPTOR_SETS_PER_FRAME` const local to `render_pipeline.rs`.

**Files:**
- Modify: `crates/yserver/src/kms/vk/render_pipeline.rs`

### Step 1: Verify zero remaining callers

- [ ] **Step 1: Confirm zero remaining callable sites**

```bash
cd /home/jos/Projects/yserver
rg -n 'reset_descriptors\(' crates/yserver/src crates/yserver-core/src crates/yserver-protocol/src
rg -n '\.allocate_descriptor_for_views\(' crates/yserver/src crates/yserver-core/src crates/yserver-protocol/src
```

Expected: hits ONLY inside `crates/yserver/src/kms/vk/render_pipeline.rs` (the function definitions about to be removed). Note the `\(` discriminator — bare-name doc-comment references in `crates/yserver/src/kms/scheduler/batch_descriptor_arena.rs:14-45` (audit catalogue scaffolding from 3A) don't count and will be cleaned up in Step 8 below.

If any other caller remains, STOP — T3 didn't fully land or there's a path the plan didn't anticipate.

### Step 2: Remove the methods

- [ ] **Step 2: Delete `reset_descriptors` (lines ~444–453)**

Find:

```rust
    /// Reset the per-call descriptor pool. Invalidates every
    /// descriptor set allocated since the last reset; call before
    /// allocating a new set for a fresh `render_composite` call.
    pub fn reset_descriptors(&self) -> Result<(), vk::Result> {
        unsafe {
            self.vk
                .device
                .reset_descriptor_pool(self.descriptor_pool, vk::DescriptorPoolResetFlags::empty())
        }
    }
```

Delete the entire block (doc-comment + fn body + trailing blank line).

- [ ] **Step 3: Delete `allocate_descriptor_for_views` (lines ~455–504)**

Find the legacy shared-pool variant (the `_into` variant added in 3F-1 stays — it's the one render-composite + render-traps now use). The legacy function starts at the doc-comment "Allocate a fresh descriptor set bound to..." Delete from there through the closing `}` of the function (about 50 lines).

### Step 3: Remove the field + creator + destroyer

- [ ] **Step 4: Delete the `descriptor_pool` field (line ~284)**

Find:

```rust
    descriptor_pool: vk::DescriptorPool,
```

Delete the line.

- [ ] **Step 5: Delete the pool creation in `new` (lines ~385–402)**

Find:

```rust
        // Each set has 3 combined-image-samplers (src + mask + dst readback).
        let pool_sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(MAX_DESCRIPTOR_SETS_PER_FRAME * 3)];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(MAX_DESCRIPTOR_SETS_PER_FRAME)
            .pool_sizes(&pool_sizes);
        let descriptor_pool = match unsafe { device.create_descriptor_pool(&pool_info, None) } {
            Ok(p) => p,
            Err(e) => {
                unsafe {
                    device.destroy_pipeline_layout(pipeline_layout, None);
                    device.destroy_descriptor_set_layout(descriptor_set_layout, None);
                    device.destroy_sampler(sampler, None);
                }
                return Err(e.into());
            }
        };
```

Delete the entire block.

- [ ] **Step 6: Update `Ok(Self { ... })` in `new` (line ~404)**

Find:

```rust
        Ok(Self {
            vk,
            pipeline_layout,
            descriptor_set_layout,
            sampler,
            descriptor_pool,
            pipelines: HashMap::new(),
        })
```

Change to:

```rust
        Ok(Self {
            vk,
            pipeline_layout,
            descriptor_set_layout,
            sampler,
            pipelines: HashMap::new(),
        })
```

(Drop the `descriptor_pool,` line.)

- [ ] **Step 7: Remove `destroy_descriptor_pool` from `Drop` (line ~569)**

Find the line:

```rust
                .destroy_descriptor_pool(self.descriptor_pool, None);
```

And the surrounding `self.vk.device` access. The Drop's full structure should shrink by one `destroy_*` call. Don't touch the others (pipeline_layout / descriptor_set_layout / sampler are still owned).

- [ ] **Step 8: Remove the now-orphaned `MAX_DESCRIPTOR_SETS_PER_FRAME` (line ~313)**

Find:

```rust
pub const MAX_DESCRIPTOR_SETS_PER_FRAME: u32 = 1024;
```

Confirm it has no remaining users in this file (rg already greens it with no callers), then delete the line.

(Note: there is a `MAX_DESCRIPTOR_SETS_PER_FRAME` of the same name in `crates/yserver/src/kms/vk/pipeline.rs` — that's a separate const for the compositor pipeline and is used by `backend.rs:7172`. Leave that one alone. Confirm by `grep -rn 'MAX_DESCRIPTOR_SETS_PER_FRAME' crates/yserver/src/` after the edit — should show only the pipeline.rs definition and the one backend.rs usage of the pipeline.rs one.)

- [ ] **Step 9: Bring the `batch_descriptor_arena.rs` header doc comment current**

The current header at `crates/yserver/src/kms/scheduler/batch_descriptor_arena.rs:14-45` is the 3A-era audit catalogue ("`# Paint-side descriptor pool catalogue (for 3D plan author)`") that called out the now-deleted `RenderPipelineCache::reset_descriptors` + `allocate_descriptor_for_views` as the migration targets. After T4's deletions those references are stale.

Open `crates/yserver/src/kms/scheduler/batch_descriptor_arena.rs` and replace lines 14-45 (the catalogue block, starting with `//! # Paint-side descriptor pool catalogue` and ending with the 3D-migration-plan note) with a single short paragraph reflecting the post-migration state:

```rust
//! # Migration history
//!
//! Created in 3A to replace `RenderPipelineCache`'s shared
//! `descriptor_pool`. Wired up in 3F-1 (`try_vk_render_composite`)
//! and 3F-2 (`try_vk_render_traps_or_tris`); the legacy shared-pool
//! API on `RenderPipelineCache` was removed at the end of 3F-2.
//! `TextPipeline` still owns a single per-pipeline pool for its
//! atlas binding — that's a one-pre-allocated-set pattern, not a
//! per-call allocation, so it doesn't have the
//! reset-invalidates-live-sets hazard this arena solves.
```

Leave lines 1-12 (the actual module doc) intact.

### Step 4: Build

- [ ] **Step 9: `cargo check -p yserver`**

Expected: clean. If `unused_import` warnings on `MAX_DESCRIPTOR_SETS_PER_FRAME`, go back to Step 8 and confirm the const is genuinely orphaned.

### Step 5: Tests + fmt + clippy

- [ ] **Step 10: `cargo test -p yserver --lib 2>&1 | tail -5`**

Expected: 138 passed.

- [ ] **Step 11: `cargo +nightly fmt --check`**

Expected: no diff.

- [ ] **Step 12: `cargo clippy -p yserver 2>&1 | tail -10`**

Expected: 5 pre-existing warnings, no new ones.

### Step 6: Commit T4

- [ ] **Step 13: Commit**

```bash
git add crates/yserver/src/kms/vk/render_pipeline.rs crates/yserver/src/kms/scheduler/batch_descriptor_arena.rs
git commit -m "$(cat <<'EOF'
refactor(kms): remove legacy RenderPipelineCache shared-pool API

With try_vk_render_traps_or_tris migrated in 3F-2 T3, no caller
invokes the legacy reset_descriptors or allocate_descriptor_for_views
any more. Remove both methods, the descriptor_pool field, the
pool-create branch in new, the pool-destroy in Drop, and the
now-orphaned MAX_DESCRIPTOR_SETS_PER_FRAME const local to this
file. The pipeline.rs const of the same name (compositor pipeline)
is unrelated and stays.

All RENDER paths now allocate descriptors per-batch via
BatchDescriptorArena (3F-1's allocate_descriptor_for_views_into);
the shared-pool's reset-invalidates-live-sets hazard is gone for
good.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Validation + results doc

**Goal:** End-to-end verification + results doc following the 3F-1 template.

**Files:**
- Create: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-2-results.md`

### Step 1: Static verification

- [ ] **Step 1: Cutover greps (semantic, not numeric)**

```bash
cd /home/jos/Projects/yserver

rg -n 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs
```

Expected: NO hits inside `try_vk_render_traps_or_tris` or `try_vk_render_composite`. Remaining hits: `run_legacy_paint_op` body, 3 readback handlers (`try_vk_get_image_pixels`, `hw_cursor_refresh`, `read_mirror_pixels`), `open_with_commit`, `dump_scanout_one`. Compared to end-of-3F-1, the count drops by one (the traps `run_one_shot_op` is gone).

```bash
rg -n 'flush_if_needed[(]BatchFlushReason::ProtocolBarrier' crates/yserver/src/kms/backend.rs
```

Expected: the OLD unconditional pre-record flush inside `try_vk_render_traps_or_tris` is gone. ONE **needs-grow-only** pre-flush remains inside `try_vk_render_traps_or_tris` (covering both mask and dst_readback grow). Other remaining sites: `run_legacy_paint_op` body, drawable-destruction sites, gradient-create sites, `try_vk_copy_area` resize-only, `try_vk_render_composite` resize-only.

```bash
rg -n '\.allocate_descriptor_for_views\(' crates/yserver/src/kms/
```

Expected: ZERO hits (the legacy variant is fully gone after T4). The `\(` discriminator distinguishes callable invocations from bare-name doc references; the `batch_descriptor_arena.rs` audit catalogue was refreshed in T4 Step 9 so doc-only mentions are also gone, but the `\(` guards against re-introduction if a future doc edit references the removed name.

```bash
rg -n 'allocate_descriptor_for_views_into' crates/yserver/src/kms/
```

Expected: 1 definition (`render_pipeline.rs`) + 2 call sites (`backend.rs` — inside `try_vk_render_composite` from 3F-1 + inside `try_vk_render_traps_or_tris` from 3F-2).

```bash
rg -n 'reset_descriptors\(' crates/yserver/src/kms/
```

Expected: ZERO hits (the function is gone after T4).

```bash
rg -n 'needs_grow|needs_image_grow' crates/yserver/src/kms/
```

Expected: 3 defs (`copy_scratch.rs` 3D, `dst_readback.rs` 3F-1, `mask_scratch.rs` 3F-2) + 3 call sites in `backend.rs` (`try_vk_copy_area` 3D, `try_vk_render_composite` 3F-1, `try_vk_render_traps_or_tris` 3F-2).

```bash
rg -n '3D-deferred: render-traps' crates/yserver/src/kms/backend.rs
```

Expected: ZERO hits (the stale comment was inside the removed pre-flush block).

```bash
rg -n 'MaskScratch::upload_r8\|fn upload_r8' crates/yserver/src/kms/
```

Expected: ZERO hits (the function is replaced by `needs_image_grow + ensure_image_size + record_upload_r8`).

```bash
rg -n 'descriptor_pool' crates/yserver/src/kms/vk/render_pipeline.rs
```

Expected: ZERO hits (field, creator, destroyer all gone after T4).

- [ ] **Step 2: Tree green**

```bash
cargo +nightly fmt --check
cargo clippy -p yserver 2>&1 | tail -10
cargo test --workspace 2>&1 | tail -20
```

Expected:
- fmt: no diff.
- clippy: 5 pre-existing warnings; no new ones.
- tests: yserver lib 138 passed; workspace green.

### Step 2: Hardware smoke (REQUIRED — traps is the load-bearing path for adapta-nokto + mate-cc)

- [ ] **Step 3: Hardware smoke from a separate TTY** (per phase-3D-results.md teardown workflow)

The known-issue at `docs/known-issues.md` (mate-cc + adapta-nokto catastrophic lag) is the **primary reproducer** for this phase. The hypothesis behind that entry was per-call `queue_wait_idle` in `try_vk_render_traps_or_tris` and `GlyphAtlas::intern`. 3F-2 retires the traps half. **A successful 3F-2 smoke should show noticeably reduced lag with adapta-nokto + mate-cc**, even without Phase 5's atlas rewrite — the glyph atlas alone shouldn't be the whole story; trapezoids are the high-volume path under adapta-nokto's rounded-everything.

```bash
just yserver-mate-hw-release
# OR `just yserver-xfce-hw` (xfce4 also exercises traps under thin themes)
```

Exercise:

1. **rendercheck on yserver** — `just rendercheck-yserver`. **Trapezoid + triangle stress is the new code path's smoke test.** Expected: `triangles` 456/456 still passes; `composite`/`cacomposite` partial counts unchanged or better (the descriptor-arena lifetime work was already validated by 3F-1, but the new MaskScratch arena introduces a new failure mode if mis-wired). Failures here surface as visual corruption or `paint batch submit failed` in `yserver-hw.log`.

2. **adapta-nokto + mate-cc smoke** — switch the system theme to adapta-nokto (or test theme), open mate-control-center, hover Appearance/Background/etc. **Compare lag character vs pre-3F-2.** A 50-90% reduction in cursor lag would be the expected sign that the traps wait-idle was the bottleneck; if the lag is unchanged, the per-glyph `GlyphAtlas::intern` wait-idle is the dominant remaining cost and Phase 5 will close the gap. Either way log the result in the results doc.

3. **GTK theme transitions** — open `gtk3-demo` or similar; click through the theme browser. Specifically exercises trapezoid + composite-glyphs cross-batch interaction.

4. **fvwm3 / xfce4-panel** — these stress thin-theme trapezoid usage; the migration shouldn't regress them.

**Pass criteria** (all must hold):

- No `vk render_traps: record failed` warnings in `yserver-hw.log`.
- No `paint batch submit failed`, no `renderer_failed`, no `DEVICE_LOST`.
- No `vk render_traps: pre-resize flush failed` warnings (rare path; capture context if it fires — indicates a batch was already Poisoned).
- No `vk render_traps: mask ensure_image_size failed` warnings (rare; only on OOM).
- No `vk render_traps: arena alloc` warnings under realistic workloads (rare; only on OOM).
- No `descriptor set` validation messages from `VK_LAYER_KHRONOS_validation` if enabled.
- No kernel GPU fault in `journalctl -k --since "yserver-start-time"`.
- rendercheck composite/cacomposite/triangles all pass the same set of cases as pre-3F-2 baseline (read pre-3F-2 baseline from a fresh run before starting).

If any fail, **STOP** — do not commit T5 or claim 3F-2 done. Most likely failure modes and root causes:

| Failure | Likely root cause |
|---|---|
| `descriptor set` validation msg | Same as 3F-1 — arena wasn't wired correctly. Check `BatchDescriptorArena::allocate_set` is called inside the closure |
| `paint batch submit failed` on traps-heavy frames | Closure returned an Err that poisoned the batch. Check for a missed `?` or a layout transition mistake in `record_upload_r8` |
| GPU fault on mask sample (R8 path) | Mask scratch image's `current_layout` got out of sync with the CB's actual layout. Check that `record_upload_r8` sets `self.current_layout = SHADER_READ_ONLY_OPTIMAL` only AFTER recording the final barrier (the spec requires CPU-side updates AFTER the barrier write to the CB) |
| GPU fault on rendercheck Disjoint/Conjoint trap cases | `dst_readback` lifetime hole. Check that `needs_readback_grow` fires on first sight of a new dst extent |
| Visual corruption on rounded GTK widgets | Coverage mask data is wrong — wrong offset/stride from arena. Check `alloc.offset` is what gets passed to `record_upload_r8` (NOT 0) |
| Lag in mate-cc + adapta-nokto unchanged | Expected ONLY if `GlyphAtlas::intern` dominates. Capture a `perf` profile during the lag; if `vkQueueWaitIdle` is the top frame under `GlyphAtlas::intern`, Phase 5 is the remaining gap. If it's under `try_vk_render_traps_or_tris`, the migration didn't fully land — re-check `run_one_shot_op` is gone from the traps function |

### Step 3: Write results doc

- [ ] **Step 4: Create `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-2-results.md`**

Follow the 3F-1 template. Sections:

1. **Header**: title `Phase 3F-2 — rendering re-architecture — render-traps/triangles migration + MaskScratch arena — results`, date, plan ref (`phase3f-2.md`), branch (`graphics-followups`), predecessor (`phase3f-1-results.md`).
2. **Scope landed**: T1 `MaskScratch::needs_image_grow`, T2 MaskScratch decompose, T3 traps migration, T4 legacy RenderPipelineCache API removal.
3. **Preflight checks**: real fmt / clippy / test counts.
4. **Cutover greps**: actual `rg` output captured semantically.
5. **Done conditions**: enumerated below.
6. **Hardware smoke results**: report the actual run, including the adapta-nokto + mate-cc lag delta. Cross-reference `docs/known-issues.md`. If the lag is materially better, update the known-issue with "3F-2 reduced this; remaining lag attributed to Phase 5 atlas rewrite." If unchanged or worse, capture the perf profile and surface as a discovery.
7. **Plan bugs caught**: any recipe-level issues hit during T1–T4.
8. **Commit summary** table.
9. **Known deferred items**: Phase 4 (sync rework), Phase 5 (targeted VkFence for readback + GlyphAtlas::intern), Phase 6 (batch-owned refcounted handles).
10. **What's next**: Phase 4 planning. With 3F-2 landed, the entire paint hot path is batched; Phase 4's `vkQueueWaitIdle` retirement from `PaintBatch::submit_and_wait` is the next clear win and what would close the remaining gap on weaker hardware.

### Step 4: Commit T5

- [ ] **Step 5: Commit**

```bash
git add docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-2-results.md
git commit -m "$(cat <<'EOF'
docs(plans): phase-3F-2 validation results

T1 added MaskScratch::needs_image_grow. T2 decomposed
MaskScratch::upload_r8 into pub-ensure_image_size +
record_upload_r8, dropping the private staging buffer. T3
migrated try_vk_render_traps_or_tris to record_paint_batch_op
with mask coverage staging through BatchUploadArena and
descriptor allocation through the per-batch arena (3F-1's
allocate_descriptor_for_views_into). T4 removed the
RenderPipelineCache shared-pool API (its last caller).

The render family is now fully migrated. All paint-side
recorders pack into the open PaintBatch; the unconditional
ProtocolBarrier flush before each RENDER op is gone for both
families (A: composite, B: traps/triangles).

Hardware smoke on <host>: <result>. adapta-nokto + mate-cc lag
character: <better / same / capture-and-attribute-to-phase-5>.

Next: Phase 4 (sync rework) to retire vkQueueWaitIdle from
PaintBatch::submit_and_wait.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Done conditions

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` produces 5 pre-existing warnings; no new warnings.
3. `cargo test --workspace` green; yserver lib 138 passed.
4. `MaskScratch::needs_image_grow(w, h)` exists. Returns `true` when `w > extent.width || h > extent.height`, `false` otherwise.
5. `MaskScratch::ensure_image_size` is `pub`. `MaskScratch::record_upload_r8(&mut self, vk, cb, src_buffer, src_offset, w, h)` exists.
6. `MaskScratch::upload_r8(pool, w, h, bytes)` does NOT exist. `MaskScratch::ensure_staging` and `MaskScratch::allocate_staging` do NOT exist. `MaskScratch.staging_buffer / staging_memory / staging_mapped / staging_size` fields do NOT exist.
7. `try_vk_render_traps_or_tris` uses `record_paint_batch_op` with:
   - 6 disjoint captures (`dst_mirror`, `solid_src_image`, `mask_scratch`, `dst_readback`, `render_cache` plus the `batch` argument).
   - Mask upload via `batch.upload_arena_mut().alloc(needed, 4)` + `copy_nonoverlapping` + `mask_scratch.record_upload_r8(...)` inside the closure.
   - Descriptor set allocated INSIDE the closure via `render_cache.allocate_descriptor_for_views_into(batch.descriptor_arena_mut(), ...)`.
   - Outer-flag `arena_oom` pattern used for arena alloc failure (does NOT poison batch).
8. The OLD `flush_if_needed(BatchFlushReason::ProtocolBarrier)` block inside `try_vk_render_traps_or_tris` is GONE.
9. The `3D-deferred: render-traps needs...` comment is GONE.
10. The combined-grow pre-flush (`needs_mask_grow || needs_readback_grow`) exists and fires only when either `ensure_image_size` or `ensure` would reallocate.
11. **Ordering invariant**: the pre-flush MUST happen BEFORE `mask_scratch.ensure_image_size` AND BEFORE `dst_readback.ensure`. Either ordering violation would let `ensure_*` free state still referenced by un-submitted CB commands.
12. `RenderPipelineCache::reset_descriptors` does NOT exist.
13. `RenderPipelineCache::allocate_descriptor_for_views` (legacy variant) does NOT exist.
14. `RenderPipelineCache.descriptor_pool` field does NOT exist.
15. `MAX_DESCRIPTOR_SETS_PER_FRAME` in `render_pipeline.rs` does NOT exist (the same name in `pipeline.rs` is unrelated and stays).
16. The `run_legacy_paint_op` audit-catalogue entry for `try_vk_render_traps_or_tris` (corrected name) reads `migrated 3F-2 (record_paint_batch_op + arena upload + arena descriptors)`.
17. Hardware smoke green per T5 step 3 — rendercheck delta against pre-3F-2 baseline shows no regressions; no GPU faults; no descriptor validation warnings; adapta-nokto + mate-cc lag delta captured (improved or attributed to Phase 5).

## Cutover greps (post-3F-2 — semantic, not numeric)

```
$ rg -n 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs
# SITES expected: run_legacy_paint_op body, 3 readback handlers,
# open_with_commit, dump_scanout_one. ZERO inside any try_vk_render_*
# function.

$ rg -n 'flush_if_needed[(]BatchFlushReason::ProtocolBarrier' crates/yserver/src/kms/backend.rs
# SITES: run_legacy_paint_op body, 5 drawable-destruction sites,
# 2 gradient-create sites, try_vk_copy_area same-overlap resize-only,
# try_vk_render_composite resize-only, try_vk_render_traps_or_tris
# combined-resize-only.

$ rg -n 'record_paint_batch_op|record_paint_op' crates/yserver/src/kms/backend.rs
# ≥ 11 call sites total.

$ rg -n '\.allocate_descriptor_for_views\(' crates/yserver/src/kms/
# ZERO (legacy variant deleted; \( discriminates calls from doc-only references).

$ rg -n 'allocate_descriptor_for_views_into' crates/yserver/src/kms/
# 1 def + 2 call sites (try_vk_render_composite + try_vk_render_traps_or_tris).

$ rg -n 'reset_descriptors\(' crates/yserver/src/kms/
# ZERO (the function is gone).

$ rg -n 'needs_grow|needs_image_grow' crates/yserver/src/kms/
# 3 defs (copy_scratch, dst_readback, mask_scratch) + 3 call sites in backend.rs.

$ rg -n '3D-deferred: render-traps' crates/yserver/src/kms/backend.rs
# ZERO.

$ rg -n 'MaskScratch::upload_r8\|fn upload_r8' crates/yserver/src/kms/
# ZERO.

$ rg -n 'descriptor_pool' crates/yserver/src/kms/vk/render_pipeline.rs
# ZERO.
```

## Notes for the implementer

- **Borrow split is the only structural risk.** The 6-disjoint-field-plus-shared-render-cache pattern extends 3F-1's 5-disjoint pattern by one (`mask_scratch`). If `cargo check` complains, you forgot direct field paths. Same diagnostic recipe as 3F-1.
- **`pipeline` and `pipeline_layout` are `Copy` Vulkan handles.** Same as 3F-1.
- **`record_render_composite` is unchanged.** Same as 3F-1.
- **No tests to write.** Same as 3F-1.
- **Watch for the OOM-poison pattern.** The arena alloc inside the closure MUST use the outer-flag pattern, NOT `?`. Anything else in the closure (descriptor alloc, `record_render_composite`) uses `?` normally — poisoning the batch is correct for in-flight recorder failures.
- **Watch for arena alignment**. `arena.alloc(needed, 4)` — 4-byte alignment for `cmd_copy_buffer_to_image` of R8 data (Vulkan spec VUID-vkCmdCopyBufferToImage-srcImage-04053). 1 also works in practice on AMD but 4 is spec-safe.
- **Hardware smoke is the gate on the perf claim, not the correctness claim**. Correctness rides on rendercheck (and the absence of GPU faults). The adapta-nokto + mate-cc lag delta is observational — capture the result honestly even if it's unchanged. An unchanged lag with the entire paint hot path batched would mean `GlyphAtlas::intern`'s per-glyph wait-idle dominates, and Phase 5 is the next clear win.
- **3F-2 is the last 3F sub-phase.** Phase 4 (sync rework) is the next phase. Phase 4's scope is `vkQueueWaitIdle` retirement from `PaintBatch::submit_and_wait` + `run_one_shot_op`. The render family migration is complete after 3F-2.
