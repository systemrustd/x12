# Phase 3D — rendering re-architecture — copy-same-overlap migration

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate the last remaining COPY-family paint recorder — `record_copy_area_same_overlap` — from `run_one_shot_op` + `flush_if_needed(ProtocolBarrier)` to `record_paint_batch_op`, completing the COPY family migration started in phase 3B.

**Architecture:** The same-overlap arm of `try_vk_copy_area` (`backend.rs:3271+`) uses a shared `CopyScratch` image to round-trip image-to-image-to-image when src and dst alias. Unlike the upload-backed paint paths migrated in 3C (PutImage, mirror upload), there is **no host-side staging** — the scratch image is GPU-resident, populated via `cmd_copy_image`. So `BatchUploadArena` is not involved. The migration is a textual swap of `run_one_shot_op` → `self.scheduler.record_paint_batch_op` plus a borrow-split that holds three disjoint `&mut` field borrows simultaneously (`scheduler`, `windows`/`pixmaps`, `copy_scratch`).

**Tech Stack:** Rust, ash (Vulkan), the existing 3A–3C infrastructure (`PaintBatch`, `record_paint_batch_op`, `paint_resources()`, `renderer_failed` gate, drawable-destruction barriers, audit catalogue).

---

## Phase context

Read `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3c-results.md` first. Phase 3D's scope is **deliberately narrow** — one recorder, one call site, parallels 3B's distinct/same migrations. Reasoning:

1. **Text-run** (`try_vk_text_run`, `try_vk_render_composite_glyphs`) and **render-composite** (`try_vk_render_traps_or_tris`, `try_vk_render_composite`) are the other recorders left on `run_legacy_paint_op`. Each needs a per-batch MaskScratch staging strategy + BatchDescriptorArena usage — independent infrastructure, sized as their own phase (3E).
2. Copy-same-overlap is the only remaining COPY recorder. Closing it out completes one family, removes the most frequent `ProtocolBarrier` flush in real workloads (xterm scrollback, text-selection drag, MATE file-manager refresh).
3. The migration also serves as a precedent for "shared scratch image used across multiple ops in one batch". The single-shared `CopyScratch` image is reused across all same-overlap ops in a batch; each recorder closure emits its own layout transitions (`record_to_transfer_dst` / `record_to_transfer_src`) so the GPU executes them in order with correct barriers. This is the SAME model text/render-composite will use with the shared `MaskScratch` and `GlyphAtlas` images in 3E — getting it right here in a small recorder is cheap insurance.

Key invariants 3D inherits from 3A/3B/3C:

1. **Drop-order**: `KmsBackend.scheduler` is declared BEFORE `KmsBackend.ops_command_pool`. Don't touch field order. (`feedback_kmsbackend_drop_order` memory.)
2. **Drawable-destruction barriers**: `DestroyWindow`, `configure_window` resize, `FreePixmap`, `RenderFreePicture`, `RenderCreateCursor` rescued path already flush the batch before dropping `VkImage`s. Copy-same-overlap targets are windows + pixmaps; the 3B set covers them. **No new destruction-barrier sites needed.** (`feedback_paintbatch_destruction_barrier` memory.)
3. **`renderer_failed` gate**: every paint entry point goes through `paint_resources()`.
4. **`record_paint_batch_op` is the load-bearing API** for recorders that need `&mut PaintBatch`. **3D does NOT need batch arenas** — it doesn't allocate staging or descriptors per op. So 3D could in principle use the shim `record_paint_op`. **Use `record_paint_batch_op` anyway** for consistency with future migrations and so the audit catalogue grep stays unambiguous (every non-readback paint sits on `record_paint_batch_op`).

## Out of scope (deferred to 3E and beyond)

- `text::record_text_run` (2 call sites: `try_vk_text_run`, `try_vk_render_composite_glyphs`) — phase 3E (text family).
- `render::record_render_composite` (2 call sites: `try_vk_render_traps_or_tris`, `try_vk_render_composite`) — phase 3E (render family). May split into two phases depending on MaskScratch + dst_readback complexity.
- `MaskScratch::upload_r8` migration — co-moves with text/render in 3E.
- Glyph atlas incremental upload — co-moves with text in 3E.
- `record_get_image` (the last `run_one_shot_op` paint-side site) — phase 5 (targeted VkFence per HLD).

## File structure

| File | Role | Touched in |
|---|---|---|
| `crates/yserver/src/kms/backend.rs` | Migrate `try_vk_copy_area` same-overlap arm (lines ~3271–3344); update `run_legacy_paint_op` audit catalogue (~line 1697 doc block, line 1704 entry) | T1 |
| `crates/yserver/src/kms/vk/ops/copy.rs` | `record_copy_area_same_overlap` recorder signature unchanged — already arena-free (uses scratch image, no host staging) | (read only) |
| `crates/yserver/src/kms/vk/copy_scratch.rs` | Add `pub fn needs_grow(&self, w: u32, h: u32) -> bool` accessor so the migration site can detect a pending resize BEFORE entering the batch closure. Existing `ensure_size`, `record_to_transfer_dst`, `record_to_transfer_src`, `image()` unchanged. | T1 |
| `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3d-results.md` | Results doc | T2 |

## Pre-task notes (read before starting)

1. **Three disjoint `&mut` field borrows must coexist**: `&mut self.scheduler` (for `record_paint_batch_op`), `&mut self.windows[xid].vk_mirror` OR `&mut self.pixmaps[xid].vk_mirror` (the `mirror` argument), and `&mut self.copy_scratch.as_mut().unwrap()` (the `scratch` argument). Rust's field-disjoint mutable borrows handle this when you access each via a direct field path. **Do not** combine these into a helper that takes `&mut self` — that hides the disjointness from the borrow checker.

2. **The order matters**:
   - `paint_resources()` first (shared borrow of self, gives back `(Arc<VkContext>, vk::CommandPool)`).
   - Re-borrow `mirror` via `self.windows.get_mut()` / `self.pixmaps.get_mut()`.
   - Re-borrow `scratch` via `self.copy_scratch.as_mut()`.
   - Call `scratch.ensure_size(w, h)?` while you have the `&mut scratch` borrow.
   - Then `self.scheduler.record_paint_batch_op(vk_arc, pool_handle, |vk, _batch, cb| { ... })`. The closure captures `mirror` and `scratch` by move (FnOnce + `&mut T` re-borrow is the move).

3. **Shared scratch across multiple ops in one batch is SAFE (for the back-to-back-same-size case)** because:
   - Within a single CB, the GPU walks recorded commands in program order.
   - `CopyScratch::record_to_transfer_dst` and `record_to_transfer_src` track `self.current_layout` and emit the layout-transition barrier from whatever the scratch was last left in (uses `ALL_COMMANDS` source stage so prior `cmd_copy_image`s on the scratch complete before the next overwrite).
   - So back-to-back same-overlap recorders see: op 1 leaves scratch in TRANSFER_SRC; op 2's `record_to_transfer_dst` transitions TRANSFER_SRC → TRANSFER_DST and op 2 proceeds normally.
   - Scratch may end a batch in TRANSFER_SRC; this is fine because the **next** batch's first op transitions from the tracked layout. State doesn't reset between batches.
   - Submitted batches reach the GPU at `composite_and_flip`'s `VisibleComposite` flush (backend.rs:`composite_and_flip`), which is the consumption point for all batched mirror writes and scratch transitions.

4. **Shared scratch across multiple ops in one batch is NOT safe IF the scratch resizes mid-batch.** `CopyScratch::ensure_size` (`copy_scratch.rs:66`) allocates a new image, `queue_wait_idle`s, then destroys the old image. `queue_wait_idle` does NOT wait for un-submitted commands. So if op 1 recorded a `cmd_copy_image` using the OLD scratch image into the open batch CB, and op 2 calls `ensure_size` requesting a larger size, the old image is freed while still embedded in the unsubmitted CB → UAF at composite-flush time.

   **The mitigation (the load-bearing 3D change):** detect "would this resize?" BEFORE entering `record_paint_batch_op`. If yes, `flush_if_needed(BatchFlushReason::ProtocolBarrier)` first — that submits the open batch, waits idle, and retires it. Then `ensure_size` runs safely (the CB referencing the old image is already retired). This adds a flush on the **first** same-overlap op whose size exceeds the current scratch, which is rare (typically once at startup or when a window grows). The HLD's eventual fix is to adopt the old image into `PaintBatch` for deferred retirement (`waitidle-catalogue.md` row for `CopyScratch::ensure_size` is tagged "phase 3" for exactly this reason). 3D uses the flush-first mitigation because it's a one-line change; the batch-adopt retirement model lands as part of phase 4's general scratch lifetime rework.

5. **No OOM-poison concern in 3D**: copy-same-overlap allocates **nothing** at record time (no arena, no descriptor pool). The `CopyScratch::ensure_size` call happens BEFORE `record_paint_batch_op` and is a CPU-side Result. If it fails, the early-return is before the recorder closure is dispatched, so the batch state is untouched. (3C T1/T2's "outer flag" pattern is not needed here.)

6. **Test coverage**: the function has no direct unit test (matches every other migrated recorder). Coverage is xts5 + rendercheck + hardware smoke. Don't add a test.

7. **clippy**: project preference is plain `cargo clippy` (no `clippy::pedantic`). Five pre-existing `doc_lazy_continuation` warnings remain — no new ones.

---

## Task 1: Migrate `try_vk_copy_area` same-overlap arm to `record_paint_batch_op`

**Goal:** Replace the `run_one_shot_op` + inline `flush_if_needed(ProtocolBarrier)` shape with `paint_resources()` + `self.scheduler.record_paint_batch_op(...)`. Add a pre-resize flush so the CopyScratch image-replacement path does not free an image referenced by the open batch CB.

**Files:**
- Modify: `crates/yserver/src/kms/vk/copy_scratch.rs` — add `pub fn needs_grow(&self, width: u32, height: u32) -> bool`
- Modify: `crates/yserver/src/kms/backend.rs` (`try_vk_copy_area` same-overlap arm, lines ~3271–3344)

### Step 0: Add `CopyScratch::needs_grow` accessor

- [ ] **Step 0a: Read `copy_scratch.rs` to confirm field shape**

`CopyScratch` stores its dimensions in `extent: vk::Extent2D` (not separate `width`/`height` fields).

- [ ] **Step 0b: Add the `needs_grow` accessor**

Append to the `impl CopyScratch` block (right above `ensure_size`):

```rust
    /// True if a later `ensure_size(width, height)` call would reallocate
    /// the scratch image. Callers in batched paint paths use this BEFORE
    /// entering `record_paint_batch_op` so they can flush any in-flight
    /// batch — `ensure_size` destroys the old image after `queue_wait_idle`,
    /// which does NOT wait for un-submitted commands. Without a pre-flush,
    /// an open batch CB embedding the old image would dangle.
    pub fn needs_grow(&self, width: u32, height: u32) -> bool {
        width > self.extent.width || height > self.extent.height
    }
```

- [ ] **Step 0c: Build**

Run: `cargo check -p yserver`
Expected: clean.

### Step 1: Read the existing arm end-to-end

- [ ] **Step 1: Read backend.rs lines 3260–3344**

Note the structure:
1. Lines 3260–3269: `same_target` + `overlapping` rect-overlap check. Keeps as-is.
2. Lines 3271–3287: inline `flush_if_needed(BatchFlushReason::ProtocolBarrier)` borrow-conflict fallback. **REMOVED.**
3. Lines 3289–3310: re-borrow mirror, build `regions`. Mirror re-borrow stays; regions build stays.
4. Lines 3311–3313: `if regions.is_empty() { return true; }` early-return. Keeps as-is.
5. Lines 3315–3323: scratch acquire + `ensure_size`. Stays — but the `&mut scratch` borrow now needs to coexist with the closure's capture.
6. Lines 3324–3343: `run_one_shot_op(...) { record_copy_area_same_overlap(...) }`. **Replaced by `self.scheduler.record_paint_batch_op(...)`.**

### Step 2: Apply the migration

- [ ] **Step 2: Replace lines 3271–3343 (the entire `if overlapping { ... }` block body)**

The new body of the `if overlapping { ... }` arm. **Note:** `vk_arc` and `pool_handle` are already bound at the top of `try_vk_copy_area` (line 3239 calls `paint_resources()` for both branches). **Do NOT re-call `paint_resources()`** — just use the outer-scope bindings.

```rust
            if overlapping {
                // Overlap: round-trip through `copy_scratch` (single
                // shared GPU-resident scratch image, no host staging).
                //
                // 3D migration: append to the open PaintBatch via
                // record_paint_batch_op instead of run_one_shot_op.
                // The closure holds three disjoint &mut field borrows
                // simultaneously: scheduler (via the call), mirror
                // (re-borrowed from windows/pixmaps), and scratch
                // (re-borrowed from self.copy_scratch). Disjoint field
                // paths make this OK with the borrow checker.
                //
                // vk_arc / pool_handle come from the outer-scope
                // paint_resources() call at the top of this function.

                // Step 1: If a scratch resize is needed, pre-flush the
                // batch BEFORE acquiring any mut borrows. ensure_size
                // destroys the old image after queue_wait_idle, which
                // does NOT wait for un-submitted commands — so an open
                // batch CB embedding the old scratch would dangle.
                // This check uses a shared borrow of self.copy_scratch
                // that ends before the flush_if_needed call (&mut self).
                let needs_scratch_grow = self
                    .copy_scratch
                    .as_ref()
                    .is_some_and(|s| s.needs_grow(u32::from(width), u32::from(height)));
                if needs_scratch_grow {
                    use crate::kms::scheduler::paint_batch::BatchFlushReason;
                    if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
                        log::warn!(
                            "vk copy same-overlap: pre-resize flush failed: {e:?}"
                        );
                        return false;
                    }
                }

                // Step 2: Resolve the mirror; we need a single &mut to it.
                let Some(mirror) = self
                    .windows
                    .get_mut(&src_xid)
                    .and_then(|w| w.vk_mirror.as_mut())
                    .or_else(|| {
                        self.pixmaps
                            .get_mut(&src_xid)
                            .and_then(|p| p.vk_mirror.as_mut())
                    })
                else {
                    return false;
                };
                let regions = build_image_copy_regions(
                    &sub_rects,
                    src_x,
                    src_y,
                    dst_x,
                    dst_y,
                    mirror.extent,
                    mirror.extent,
                );
                if regions.is_empty() {
                    return true;
                }

                // Step 3: Re-borrow scratch + ensure_size. If the
                // pre-flush above ran, ensure_size's destroy-old path
                // is safe because the batch was fully retired. If
                // no flush was needed (no grow), ensure_size is a no-op
                // on the dimension check (still safe).
                let Some(scratch) = self.copy_scratch.as_mut() else {
                    return false;
                };
                if let Err(e) = scratch.ensure_size(u32::from(width), u32::from(height)) {
                    log::warn!("vk copy: scratch resize failed: {e:?}");
                    return false;
                }
                let bbox_origin = (i32::from(src_x), i32::from(src_y));

                let result =
                    self.scheduler
                        .record_paint_batch_op(vk_arc, pool_handle, |vk, _batch, cb| {
                            copy::record_copy_area_same_overlap(
                                vk,
                                cb,
                                mirror,
                                scratch,
                                &regions,
                                bbox_origin,
                            )
                        });
                return match result {
                    Ok(()) => true,
                    Err(e) => {
                        log::warn!(
                            "vk copy: same-image overlap record failed on xid {src_xid:#x}: \
                             {e:?}"
                        );
                        false
                    }
                };
            }
```

Also at the top of `try_vk_copy_area` (look for the early `vk_arc` / `pool_handle` acquisition that's currently used by both the same-overlap arm and the non-overlap arm — it's around line 3220+): the early acquisition is no longer needed by the same-overlap arm (it now goes through `paint_resources()`). If the non-overlap arms below still use those names, leave the early acquisition in place. If the same-overlap arm was the only consumer (unlikely — distinct arm and non-overlap same arm both use them per 3B migrations), then it should remain. **Read the function header carefully and only remove the early acquisition if it's now unused.**

### Step 3: Update the audit catalogue at line ~1704

- [ ] **Step 3: Update the entry**

Find:
```bash
grep -n "same-overlap" crates/yserver/src/kms/backend.rs
```

The doc-comment in `run_legacy_paint_op` at around line 1704 currently reads:

```
///   try_vk_copy_area (same-overlap):  copy::record_copy_area_same_overlap — deferred 3D (borrow-conflict fallback, flush moved into arm)
```

Change to:

```
///   try_vk_copy_area (same-overlap):  copy::record_copy_area_same_overlap — migrated 3D (record_paint_batch_op, shared CopyScratch)
```

### Step 4: Build

- [ ] **Step 4: `cargo check`**

Run: `cargo check -p yserver`
Expected: clean.

If you see `cannot borrow X as mutable more than once`:
- You may have combined two `self.X.as_mut()` calls into a single bind that confuses the borrow checker. Split each into its own `let` with a narrow scope.
- Make sure `mirror` is bound to `&mut DrawableImage` (not `Option<...>`) before the closure captures it.

If you see `cannot borrow self.scheduler as mutable because self.* is already borrowed`:
- Confirm `mirror` and `scratch` were re-borrowed via direct field paths (`self.windows.get_mut(...)` / `self.copy_scratch.as_mut()`), not through a helper that takes `&mut self`.

### Step 5: Run tests

- [ ] **Step 5: `cargo test -p yserver --lib`**

Run: `cargo test -p yserver --lib`
Expected: 138 passed, 0 failed, 3 ignored.

### Step 6: fmt + clippy

- [ ] **Step 6: `cargo +nightly fmt --check`**

Expected: no diff.

- [ ] **Step 7: `cargo clippy -p yserver 2>&1 | tail -10`**

Expected: 5 pre-existing `doc_lazy_continuation` warnings; no new warnings.

### Step 7: Commit T1

- [ ] **Step 8: Commit**

```bash
git add crates/yserver/src/kms/backend.rs crates/yserver/src/kms/vk/copy_scratch.rs
git commit -m "$(cat <<'EOF'
refactor(kms): migrate copy-same-overlap to record_paint_batch_op

The last COPY-family recorder still on run_legacy_paint_op moves
into the PaintBatch alongside fill / copy-distinct / copy-same /
PutImage / mirror-upload. Same-overlap reuses the shared CopyScratch
image; the GPU executes recorded ops in program order with
CopyScratch::record_to_transfer_{src,dst} emitting the right
layout transitions between back-to-back same-overlap ops in one
batch.

No host staging is involved — CopyScratch is GPU-resident, populated
via cmd_copy_image. So this migration does not touch the upload
arena. The pre-record `flush_if_needed(ProtocolBarrier)` that fires
on xterm scrollback / text-selection drag is gone.

Adds CopyScratch::needs_grow accessor + a pre-resize flush in the
same-overlap arm: CopyScratch::ensure_size destroys the old image
after queue_wait_idle (which does NOT wait for un-submitted commands),
so an open batch CB referencing the old scratch would dangle. The
pre-resize flush submits + retires any in-flight batch before the
resize. Rare path (size growth only happens when a new max rect is
seen).

Audit catalogue updated to mark try_vk_copy_area same-overlap as
migrated 3D.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Validation + results doc

**Goal:** End-to-end verification + results doc following the 3A/3B/3C template.

**Files:**
- Create: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3d-results.md`

### Step 1: Static verification

- [ ] **Step 1: Cutover greps (semantic, not numeric)**

```bash
cd /home/jos/Projects/yserver
rg -n 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs
```
Expected: ZERO hits inside `try_vk_copy_area` (the same-overlap arm no longer uses it). Remaining hits: `run_legacy_paint_op` body, 3 readback handlers, 3E-deferred fallbacks (text-run × 2, render-composite × 2), `open_with_commit`, `dump_scanout_one`. Compared to end-of-3C, the count should be one lower.

```bash
rg -n 'flush_if_needed[(]BatchFlushReason::ProtocolBarrier' crates/yserver/src/kms/backend.rs
```
Expected: the OLD unconditional borrow-conflict fallback inside `try_vk_copy_area` same-overlap is gone. ONE intentional **resize-only** pre-flush remains in the same-overlap arm — fires only when `CopyScratch::needs_grow` returns true. Other remaining sites: 4 text/render legacy fallbacks (3E-deferred), `run_legacy_paint_op` body, 5 drawable-destruction sites, 2 gradient-create sites.

```bash
rg -n 'record_paint_batch_op' crates/yserver/src/kms/backend.rs
```
Expected: ≥ 3 call sites (3C T1, 3C T2, 3D — one more than at end-of-3C).

- [ ] **Step 2: Tree green**

```bash
cargo +nightly fmt --check
cargo clippy -p yserver 2>&1 | tail -10
cargo test --workspace 2>&1 | tail -20
```

Expected:
- fmt: no diff.
- clippy: 5 pre-existing warnings; no new ones.
- tests: yserver lib 138 passed + workspace green.

### Step 2: Hardware smoke

- [ ] **Step 3: Document smoke as user-deferred**

Like 3C, this migration is correctness-preserving by construction (same recorder body, same scratch-image semantics, just inside the open batch CB instead of a one-shot). xts / rendercheck / MATE smoke remain user discretion. Workloads to verify if running smoke:

- `xterm` scrollback (long output / `seq 1 10000`).
- Text selection drag in any terminal or text widget.
- MATE file-manager refresh.
- Window drag with text content visible.

No GPU faults expected; phase-3B drawable-destruction barriers cover windows/pixmaps which are the copy targets.

### Step 3: Write results doc

- [ ] **Step 4: Create `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3d-results.md`**

Follow the 3A/3B/3C template. Sections:

1. **Header**: title, date (2026-05-13 — or the actual date the implementer ran T2, written as YYYY-MM-DD), plan ref (`phase3d.md`), branch (`graphics-followups`), predecessor (`phase3c-results.md`).
2. **Scope landed**: paragraph + bullets — one recorder migrated (`record_copy_area_same_overlap`), audit catalogue updated, no new infrastructure.
3. **Preflight checks**: fmt, clippy, test counts from your actual run.
4. **Cutover greps**: real `rg` output, semantic site listing.
5. **Done conditions**: enumerated below.
6. **Hardware smoke**: user-deferred (write "Deferred to user. Migration is correctness-preserving by construction; same recorder body, same shared-CopyScratch semantics, now inside the open batch CB. The steady-state / unconditional pre-record ProtocolBarrier flush that fired on every same-overlap op (xterm scrollback / text-selection drag) is gone; a resize-only pre-flush remains, firing only when CopyScratch::needs_grow returns true (typically once at startup / when a new max rect is seen).").
7. **Plan bugs caught (folded back into plan)**: any recipe-level issues hit during T1 execution. If none, write "None — recipe applied cleanly."
8. **Commit summary** table: Plan, T1, T2.
9. **Known deferred items** — point to 3E for: text-run, render-composite, traps, MaskScratch upload, glyph atlas incremental upload. Note `record_get_image` is phase 5 scope.
10. **What's next**: pointer to 3E planning.

### Step 4: Commit T2

- [ ] **Step 5: Commit**

```bash
git add docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3d-results.md
git commit -m "$(cat <<'EOF'
docs(plans): phase-3D validation results

Migrated try_vk_copy_area same-overlap arm to record_paint_batch_op.
Completes the COPY family migration. No new infrastructure; uses the
existing shared CopyScratch image with back-to-back ops in one batch
serialized by CopyScratch::record_to_transfer_{src,dst} layout
transitions in the recorder.

Text-run, render-composite, traps, MaskScratch, glyph atlas
explicitly deferred to phase 3E.

Hardware smoke + xts deferred to user; migration is correctness-
preserving by construction.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Done conditions

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` produces 5 pre-existing `doc_lazy_continuation` warnings; no new warnings.
3. `cargo test --workspace` green.
4. `try_vk_copy_area` same-overlap arm uses `record_paint_batch_op` with three disjoint field borrows (`scheduler` + mirror + scratch).
5. The OLD inline `flush_if_needed(ProtocolBarrier)` borrow-conflict fallback at the top of the same-overlap arm is gone.
6. `CopyScratch::needs_grow` exists; same-overlap arm calls it BEFORE the mirror/scratch borrows and conditionally pre-flushes the batch when a resize is pending. The flush only fires on actual resize events (rare in steady state).
7. `CopyScratch::ensure_size` is called BEFORE `record_paint_batch_op` so CPU-side failure doesn't disturb batch state.
8. The `run_legacy_paint_op` audit catalogue entry for `try_vk_copy_area (same-overlap)` reads `migrated 3D (record_paint_batch_op, shared CopyScratch)`.
9. Hardware smoke green on the user's host (deferred to user, but no `paint batch submit failed` / `renderer_failed` expected under workloads listed in T2 step 3).

## Cutover greps (post-3D — semantic, not numeric)

```
$ rg -n 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs
# SITES expected: run_legacy_paint_op body, 3 readback handlers,
# 3E-deferred (text/render-composite × 4), open_with_commit, dump_scanout_one.
# ZERO hits inside try_vk_copy_area.

$ rg -n 'flush_if_needed[(]BatchFlushReason::ProtocolBarrier' crates/yserver/src/kms/backend.rs
# SITES expected: 4 text/render legacy fallbacks (3E), run_legacy_paint_op
# body, 5 drawable-destruction sites, 2 gradient-create sites.
# Inside try_vk_copy_area: ONE resize-only pre-flush (fires only when
# CopyScratch::needs_grow returns true). The OLD unconditional borrow-
# conflict fallback is gone.

$ rg -n 'record_paint_batch_op' crates/yserver/src/kms/backend.rs
# Expected: at least 3 call sites (3C T1, 3C T2, 3D).
```

## Notes for the implementer

- **The borrow split is the only structural risk.** If `cargo check` complains, you forgot direct field paths. The pattern: `paint_resources()` first, then `mirror` via `self.windows.get_mut` / `self.pixmaps.get_mut`, then `scratch` via `self.copy_scratch.as_mut`, then call `scratch.ensure_size`, then `self.scheduler.record_paint_batch_op(...)`. Each binding is a `&mut` to a distinct field of `self`.
- **`mirror: &mut DrawableImage` and `scratch: &mut CopyScratch` are both moved into the FnOnce closure.** Don't reuse them after the call returns.
- **`record_copy_area_same_overlap` is unchanged** — the recorder itself doesn't care whether it's inside a one-shot CB or a batched CB. The change is purely at the call site.
- **No tests to write.** Coverage comes from xts5 + rendercheck + hardware smoke. The codebase has no unit test for copy-same-overlap and adding one would require a Vulkan ICD harness that doesn't exist.
