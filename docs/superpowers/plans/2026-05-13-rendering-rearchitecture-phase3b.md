# Rendering re-architecture — phase 3B implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

Date: 2026-05-13
Status: ready for execution
Branch: `graphics-followups`
Predecessor: phase 3A (`4af9e01` results doc; tip `4af9e01`)

**Goal:** Migrate the four scratch-free paint recorders (`fill::record_fill_rectangles`, `fill::record_logic_fill`, `copy::record_copy_area_distinct`, `copy::record_copy_area_same`) from `run_one_shot_op` (per-op submit + `vkQueueWaitIdle`) to appending into the per-frame `PaintBatch` via the 3A APIs. After 3B, every call site for these four recorders goes through `record_paint_op`; the batch carries real recorded work into `flush_if_needed(VisibleComposite)` for the first time. **`copy::record_copy_area_same_overlap` is deferred to 3D** (uses `CopyScratch` shared scratch image; needs the upload-arena infrastructure 3C lays).

**Architecture:** Three pieces of infrastructure land before the migrations:

1. **Legacy paint boundary (T0).** Once fill/copy are in the batch, any *later* paint op still on `run_one_shot_op` (image / render / text / traps / copy-same-overlap / mirror.record_upload_rect) that touches the same drawable would observe the batched recorder's CPU-side `set_current_layout` mutation while the GPU hasn't yet executed the batched work. Fix: every paint-side `run_one_shot_op` call gets routed through a `run_legacy_paint_op` wrapper that calls `flush_if_needed(ProtocolBarrier)` first. Migrate every paint-side site to the wrapper before any recorder migrates.

2. **`renderer_failed` gate (T1).** Set on fatal Vk error from `flush_if_needed`. Gates every paint entry point so a single failure doesn't cascade into more abandoned CBs each cycle.

3. **`record_paint_op` on `RenderScheduler` (T1).** 3A's versions live on `KmsBackend` (taking `&mut self`) — conflicts with `&mut self.windows[id]` borrows recorder call sites hold. T1 moves the implementation onto `RenderScheduler` so call sites can split borrows: `&mut self.scheduler` is disjoint from `&mut self.windows` / `&mut self.pixmaps`. A `KmsBackend::paint_resources()` helper returns `Option<(Arc<VkContext>, vk::CommandPool)>` only when `!renderer_failed` — call sites use it to ensure the gate runs even when going through scheduler-direct calls.

**Tech Stack:** Rust 2021, `ash` for Vulkan. Existing `kms/scheduler/`, `kms/vk/ops/{fill,copy}.rs`, `kms/backend.rs`.

**Reference:**
- `docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md` — HLD; 3B implements the "fill / copy" portion of the per-family migration.
- `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase3.md` — phase-3 master plan; 3B section starts at "# Phase 3B".
- `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase3a-results.md` — 3A results, audit catalogues, plan bugs to avoid.
- Phase-3A late-mutation audit (T4): all four 3B-target recorders trivially satisfy the invariant — they have NO error paths, so `Poisoned` is unreachable from these recorders.

---

## Pre-task: global checks

Every task ends with:

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

**Pedantic clippy: NOT enforced.** Run `cargo clippy` without `-W clippy::pedantic`. Carries forward from 3A.

Use explicit `git add <file>` per file — **do NOT use `git commit -am`**.

## Plan bugs from 3A — avoid these in 3B

(Carried forward from `phase3.md`; restated for the implementer's convenience.)

1. **`#[derive(Debug)]` on any new type holding `Arc<VkContext>` won't compile.** Use a manual `Debug` impl with `finish_non_exhaustive()`.
2. **`unsafe impl Send` requires a `// SAFETY:` comment** at the impl site, not just on a related trait.
3. The phase-3A `dirty_outputs` local in `composite_and_flip` is dead. If T0's edits revisit `composite_and_flip`, leave it alone — `flush_if_needed` rebuilds the vec internally.

## Out of scope (deferred)

- `copy::record_copy_area_same_overlap` — uses `CopyScratch` shared scratch image; needs 3C's upload-arena infrastructure. Migrates in 3D.
- All other recorder families (`image`, `render`, `text`, `traps`).
- Removing `run_one_shot_op` itself or `OpsStaging` — both still used by GetImage and the deferred families.
- Removing the close-time `vkQueueWaitIdle` in `PaintBatch::submit_and_wait` — phase 4.

---

## Task 0: Legacy paint boundary — `run_legacy_paint_op` wrapper

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (add `run_legacy_paint_op` helper + migrate every paint-side `run_one_shot_op` call site to use it)

**Why this exists.** Once T2 migrates fill, here's the failure mode:

```text
1. Request handler A calls fill (migrated): records fill commands into the batch CB,
   mutates mirror.current_layout → SHADER_READ_ONLY_OPTIMAL on the CPU. No submit yet.
2. Request handler B calls image::record_put_image (NOT migrated):
   reads mirror.current_layout (SHADER_READ_ONLY_OPTIMAL), emits a
   "from SHADER_READ_ONLY → TRANSFER_DST" barrier in its own CB,
   submits via run_one_shot_op + vkQueueWaitIdle.
3. The GPU executes B's CB before A's CB — A is still in the batch, unsubmitted.
4. B's barrier transitions FROM a layout the GPU hasn't reached yet.
   At minimum: validation errors. At worst: corrupt pixels.
```

The fix: every remaining paint-side `run_one_shot_op` call gets prefixed with `flush_if_needed(ProtocolBarrier)` so the batch is force-submitted (and idle-waited via `submit_and_wait`) before the legacy op runs. Wrap the pattern as `KmsBackend::run_legacy_paint_op<F>(F)` so it's a single point of audit.

**`ProtocolBarrier` choice.** From 3A T5 the enum has `ProtocolBarrier` documented as "An explicit protocol barrier requested it. (Place-holder for future use; X11 doesn't define one directly today.)" A legacy paint op is exactly that — a synchronous barrier saying "everything before me must complete before I run." Update the variant's doc as part of T0 Step 1 to mention this concrete use, so the next reader knows it's no longer a placeholder.

**Scope.** Migrate every paint-side `run_one_shot_op` call. The three readback handlers (`try_vk_get_image_pixels`, `hw_cursor_refresh`, `read_mirror_pixels`) keep their explicit `flush_if_needed(Readback)` + `run_one_shot_op` pattern — Readback is stricter than ProtocolBarrier (surfaces failures, doesn't just no-op-and-continue), so wrapping them would weaken the contract. Document this distinction inline.

- [ ] **Step 1: Update `BatchFlushReason::ProtocolBarrier` doc + add `run_legacy_paint_op` helper**

First, update the `ProtocolBarrier` variant doc in `crates/yserver/src/kms/scheduler/paint_batch.rs` (added in 3A T5). Replace the placeholder text with the concrete use:

```rust
    /// An explicit protocol barrier requested it. The phase-3B
    /// `KmsBackend::run_legacy_paint_op` wrapper uses this reason
    /// to flush the batch before any paint op still on
    /// `run_one_shot_op`, so a migrated recorder's CPU-side layout
    /// mutation has GPU-completed before the legacy op reads it.
    ProtocolBarrier,
```

Then, in `crates/yserver/src/kms/backend.rs`, near the existing `flush_if_needed`:

```rust
    /// Run a paint op via `run_one_shot_op` after first flushing any
    /// pending `PaintBatch`. Use this at every paint-side
    /// `run_one_shot_op` call site that is NOT yet migrated to
    /// `record_paint_op` — phase-3B–3D migrations replace these
    /// wrappers one family at a time.
    ///
    /// **Why the flush:** a migrated recorder (e.g., fill) records
    /// commands into the batch and mutates CPU-side
    /// `DrawableImage::current_layout` immediately, while the
    /// batch CB hasn't been submitted. A later legacy op reading
    /// `current_layout` would emit barriers from a layout the GPU
    /// hasn't reached. The flush forces the batch to submit + wait
    /// idle before the legacy op runs.
    ///
    /// Readback handlers (`GetImage`, `read_mirror_pixels`,
    /// `hw_cursor_refresh`) DO NOT use this wrapper — they keep
    /// their existing `flush_if_needed(Readback)` + direct
    /// `run_one_shot_op` for semantic clarity that they read
    /// CPU-visible pixels. Behaviour-wise Readback and
    /// ProtocolBarrier are both strict (both surface Vk errors
    /// via `ERROR_DEVICE_LOST`); only the audit signal differs.
    pub fn run_legacy_paint_op<F>(&mut self, record: F) -> Result<(), ash::vk::Result>
    where
        F: FnOnce(&crate::kms::vk::device::VkContext, ash::vk::CommandBuffer) -> Result<(), ash::vk::Result>,
    {
        use crate::kms::scheduler::paint_batch::BatchFlushReason;
        if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
            // ProtocolBarrier is strict — flush failure here means the
            // batch was Poisoned or the renderer is failed. Either way
            // the legacy op cannot proceed safely.
            log::warn!("run_legacy_paint_op: pre-flush failed ({e:?})");
            return Err(e);
        }
        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return Err(ash::vk::Result::ERROR_INITIALIZATION_FAILED);
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return Err(ash::vk::Result::ERROR_INITIALIZATION_FAILED);
        };
        crate::kms::vk::ops::run_one_shot_op(&vk_arc, pool_handle, record)
    }
```

- [ ] **Step 2: Catalogue every paint-side `run_one_shot_op` call site**

```bash
rg -nB2 -A5 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs > /tmp/paint-sites.txt
wc -l /tmp/paint-sites.txt
```

Phase 2 results doc reported 39 sites. Cross-reference with the per-family count from 3A:
- fill: 4 (migrated in T2)
- copy: 5 (4 migrated in T3, 1 = same_overlap deferred)
- image: 4 (PutImage migrated in 3C; 2 GetImage sites stay synchronous-with-flush)
- render: 8
- text: 6
- traps: 10
- plus `mirror.record_upload_rect` and other non-family sites.

For each catalogued site, decide:
- **Wrap with `run_legacy_paint_op`**: every paint-side site that is NOT already a readback handler. This includes fill and copy-distinct/same too — they migrate in T2/T3, but the intermediate state between T2's commit (fill batched) and T3's commit (copy migrated) would otherwise have the exact ordering/layout hazard T0 prevents (a batched fill followed by a still-raw copy-distinct reads CPU-mutated layout while GPU hasn't reached it). T2/T3 then unwrap, replacing `run_legacy_paint_op` with `self.scheduler.record_paint_op(...)`. The "wrap then unwrap" pattern is the price of safe per-commit intermediate states.
- **Leave alone (Readback handler)**: `try_vk_get_image_pixels`, `hw_cursor_refresh`, `read_mirror_pixels` — they already do `flush_if_needed(Readback)` + `run_one_shot_op` and that's the correct pattern.

Wrap targets after this filter: fill (4 sites), copy-distinct + copy-same (4 sites), copy-same-overlap (1 site), image::PutImage (until 3C migrates), all render sites, all text sites, all traps sites, mirror.record_upload_rect, render_pipeline::record_solid_color_clear (around line 1801), and any other paint-side `run_one_shot_op` call. By T0's commit, every paint-side raw `run_one_shot_op` is either inside `run_legacy_paint_op`'s body or a documented borrow-conflict fallback.

Document the catalogue as a comment block at the top of the relevant `backend.rs` section so 3C/3D authors see the list.

- [ ] **Step 3: Migrate every catalogued site to `run_legacy_paint_op`**

For each site, the transformation is:

```rust
// Before:
run_one_shot_op(&vk_arc, pool_handle, |vk, cb| { ... recorder ... })

// After:
self.run_legacy_paint_op(|vk, cb| { ... recorder ... })
```

The closure body is unchanged. The `vk_arc` and `pool_handle` extractions can be deleted from the surrounding code if no other op uses them (`run_legacy_paint_op` extracts them internally).

**Borrow conflicts.** Same risk as the migration in T2/T3: `&mut self` for `run_legacy_paint_op` collides with `&mut self.windows[id]` borrows for the recorder. The `paint_resources()` helper from T1 doesn't apply here (it returns the resources for direct scheduler calls; legacy ops still go through `run_one_shot_op`). **If a site fails to compile, leave it on raw `run_one_shot_op`-with-explicit-flush:**

```rust
// Borrow-conflict fallback:
if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
    log::warn!("legacy paint flush failed: {e:?}");
    return /* whatever the no-op fallback is */;
}
let vk_arc = self.vk.as_ref().cloned()?;
let pool_handle = self.ops_command_pool.as_ref()?.handle();
let mirror = &mut self.windows[id]....;
run_one_shot_op(&vk_arc, pool_handle, |vk, cb| { ... recorder ... })
```

Document each fallback with `// borrow-conflict fallback for run_legacy_paint_op`. T1's scheduler-API move informs whether to lift these into a helper later (a `run_legacy_paint_op_with_resources(vk_arc, pool_handle, ...)` shape, mirroring the scheduler-direct call pattern).

- [ ] **Step 4: Verify**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Expected: green. No new warnings. No behaviour change yet — every paint cycle has an Idle batch (no migrations) so `flush_if_needed(ProtocolBarrier)` is a cheap state transition.

- [ ] **Step 5: Audit cutover**

```bash
# Every paint-side run_one_shot_op site is now either:
# (a) inside run_legacy_paint_op's body (one site, the dispatch),
# (b) one of the explicit-flush borrow-conflict fallbacks (commented),
# (c) one of the three Readback handlers.
rg -nB2 -A4 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs | grep -v '//' | wc -l
```

Inspect the output: every remaining raw `run_one_shot_op` should be in one of the three categories above. If any unaudited site remains (in particular, any raw fill/copy/render/text/traps/etc. call), file as a concern — T2/T3 should be starting from a fully-wrapped baseline.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver/src/kms/backend.rs \
        crates/yserver/src/kms/scheduler/paint_batch.rs
git commit -m "feat(kms): run_legacy_paint_op wrapper — flush PaintBatch before legacy paint ops"
```

---

## Task 1: `renderer_failed` gate + move record_paint_op onto RenderScheduler

**Files:**
- Modify: `crates/yserver/src/kms/scheduler/paint_batch.rs` (no change to PaintBatch; just touched if needed)
- Modify: `crates/yserver/src/kms/scheduler/mod.rs` (add `record_paint_op` + `record_paint_batch_op` to `RenderScheduler`)
- Modify: `crates/yserver/src/kms/backend.rs` (add `renderer_failed` field + gates; reduce existing `record_paint_op{,_batch_op}` to thin shims; gate `composite_and_flip` and `flush_if_needed`)

This task lands the infrastructure that T2/T3 build on. **Two orthogonal pieces** — could split into two tasks, but they're tightly coupled (the gate runs at the same entry points the move touches), so single task.

### Why move to `RenderScheduler`?

3A put `record_paint_op` on `KmsBackend` (taking `&mut self`). T2's first call site looks like:

```rust
let mirror = &mut self.windows[id].vk_mirror.unwrap();  // &mut self.windows
self.record_paint_op(|vk, cb| {                         // &mut self ⇒ conflict
    fill::record_fill_rectangles(vk, cb, mirror, color, &rects, scissor)
})
```

That doesn't compile: `&mut self.windows[id]` extends `&mut self` over the `record_paint_op` call. Field-disjointness is invisible through method dispatch. The fix: move the implementation to `RenderScheduler` where the call site can use field projection:

```rust
let scheduler = &mut self.scheduler;
let vk_arc = self.vk.as_ref().cloned().ok_or(...)?;
let pool_handle = self.ops_command_pool.as_ref().ok_or(...)?.handle();
let mirror = &mut self.windows[id].vk_mirror.unwrap();
scheduler.record_paint_op(vk_arc, pool_handle, |vk, cb| {  // &mut self.scheduler — disjoint
    fill::record_fill_rectangles(vk, cb, mirror, color, &rects, scissor)
})
```

`&mut self.scheduler` and `&mut self.windows` are disjoint fields; the borrow checker handles them.

The KmsBackend `record_paint_op` / `record_paint_batch_op` become shims for callers that don't hold a conflicting borrow. T2/T3 will reveal whether they're actually used; if not, a future cleanup can delete them.

### `renderer_failed` flag — Option A from the design appendix

`composite_and_flip` returns fatal Vk errors via `io::Error::other(...)`, but the trait-surface `Backend::on_page_flip_ready` returns `()` and drops the error on the floor. Once T2 migrates fill, a fatal `submit_and_wait` failure produces an abandoned `Submitted`-state batch; the next composite cycle would try to use it and produce more abandoned CBs each tick.

**Option A** (recommended in the plan appendix): in-band `renderer_failed: bool` on `KmsBackend`. Set true on any fatal Vk failure from `flush_if_needed`. Gate every paint entry point on it: `record_paint_op`, `record_paint_batch_op`, `flush_if_needed`, `composite_and_flip`, `try_vulkan_composite_flip` — all early-return / no-op when the flag is set.

- [ ] **Step 1: Move `record_paint_op` + `record_paint_batch_op` onto `RenderScheduler`**

In `crates/yserver/src/kms/scheduler/mod.rs`, add to `impl RenderScheduler`:

```rust
    /// Append a paint-recorder op into the current `PaintBatch`'s
    /// command buffer. Closure receives `(&VkContext, &mut PaintBatch, vk::CommandBuffer)`.
    ///
    /// **This is the load-bearing API** with `&mut PaintBatch`
    /// exposed so 3C+ recorders can call `batch.upload_arena_mut()`,
    /// `batch.descriptor_arena_mut()`, `batch.adopt(resource)` from
    /// inside the closure. 3B fill/copy recorders ignore the batch
    /// parameter (use `record_paint_op` shim).
    ///
    /// `vk` and `pool` are passed in (not pulled from `self`) so the
    /// `&mut self` here is `&mut RenderScheduler` only — disjoint
    /// from `&mut KmsBackend.windows` / `.pixmaps` borrows the call
    /// site holds for the recorder's `&mut DrawableImage` argument.
    pub fn record_paint_batch_op<F>(
        &mut self,
        vk_arc: Arc<VkContext>,
        pool: vk::CommandPool,
        record: F,
    ) -> Result<(), vk::Result>
    where
        F: FnOnce(
            &VkContext,
            &mut PaintBatch,
            vk::CommandBuffer,
        ) -> Result<(), vk::Result>,
    {
        // open_batch consumes the Arc — clone for the closure invocation.
        // Note: parameter is `vk_arc` not `vk` so it doesn't shadow the
        // `ash::vk` module used for `vk::Result::*` below.
        let _ = self.open_batch(vk_arc.clone(), pool);
        let batch = self
            .current_paint_batch
            .as_mut()
            .expect("open_batch just ran");
        match batch.state() {
            BatchState::Poisoned => return Err(vk::Result::ERROR_DEVICE_LOST),
            BatchState::Closed | BatchState::Submitted | BatchState::Retired => {
                log::error!(
                    "record_paint_batch_op: batch in non-recording state {:?}",
                    batch.state()
                );
                return Err(vk::Result::ERROR_UNKNOWN);
            }
            BatchState::Idle => {
                if let Err(e) = batch.begin_recording_explicit() {
                    return Err(match e {
                        BatchError::Vk(r) => r,
                        _ => vk::Result::ERROR_UNKNOWN,
                    });
                }
            }
            BatchState::Recording => {}
        }
        let cb = batch.command_buffer().expect("Recording implies cb");
        match record(&vk_arc, batch, cb) {
            Ok(()) => Ok(()),
            Err(e) => {
                batch.poison_external();
                Err(e)
            }
        }
    }

    /// Thin shim for recorders that don't need the batch handle
    /// (3B fill/copy). Same closure signature as `run_one_shot_op`
    /// for textual-rewrite migration.
    pub fn record_paint_op<F>(
        &mut self,
        vk_arc: Arc<VkContext>,
        pool: vk::CommandPool,
        record: F,
    ) -> Result<(), vk::Result>
    where
        F: FnOnce(&VkContext, vk::CommandBuffer) -> Result<(), vk::Result>,
    {
        self.record_paint_batch_op(vk_arc, pool, |vk, _batch, cb| record(vk, cb))
    }
```

The `Arc<VkContext>` and `vk::CommandPool` types are already in scope (file imports them at the top from 3A T1 step 2). If not, add the imports.

- [ ] **Step 2: Add `KmsBackend::paint_resources()` helper**

This is the load-bearing fix for the renderer-failed-bypass concern: every direct call to `self.scheduler.record_paint_op(vk_arc, pool_handle, ...)` MUST go through this helper to extract `vk_arc` and `pool_handle`. The helper is the single point where `renderer_failed` is checked.

```rust
    /// Returns the resources needed to call
    /// `self.scheduler.record_paint_op` / `record_paint_batch_op`,
    /// or `None` if the renderer is failed / Vk is unavailable /
    /// the ops pool is not yet built.
    ///
    /// **Use this at every paint call site that needs to take a
    /// `&mut self.windows[id]` / `&mut self.pixmaps[id]` borrow
    /// for the recorder's `&mut DrawableImage` argument.** Going
    /// through `self.record_paint_op(...)` (the shim added in
    /// step 3) is convenient when no such borrow conflict exists,
    /// but the shim is `&mut self` and conflicts with field
    /// borrows.
    ///
    /// Both shim and helper gate on `renderer_failed` — every
    /// paint entry point checks the flag.
    fn paint_resources(
        &self,
    ) -> Option<(
        std::sync::Arc<crate::kms::vk::device::VkContext>,
        ash::vk::CommandPool,
    )> {
        if self.renderer_failed {
            return None;
        }
        let vk_arc = self.vk.as_ref().cloned()?;
        let pool_handle = self.ops_command_pool.as_ref()?.handle();
        Some((vk_arc, pool_handle))
    }
```

`&self` (not `&mut self`) — so it can be called from a context that already has `&mut self.windows` (the `&self` captures only the immutable subfields `self.vk`, `self.ops_command_pool`, `self.renderer_failed`, all disjoint from `self.windows` and `self.scheduler`).

- [ ] **Step 3: Reduce `KmsBackend::record_paint_op` / `record_paint_batch_op` to thin shims**

In `crates/yserver/src/kms/backend.rs`, replace the existing bodies (added in 3A T5) with:

```rust
    /// Shim: pull vk + ops pool via `paint_resources()`, delegate
    /// to the scheduler-level `record_paint_batch_op`. Useful when
    /// the caller doesn't hold a conflicting `&mut self.windows`
    /// / `.pixmaps` borrow. Recorders that DO hold such a borrow
    /// must use `paint_resources()` + `self.scheduler.record_paint_batch_op(...)`
    /// directly (field projection works because `&mut self.scheduler`
    /// is disjoint from `&mut self.windows`).
    pub fn record_paint_batch_op<F>(&mut self, record: F) -> Result<(), ash::vk::Result>
    where
        F: FnOnce(
            &crate::kms::vk::device::VkContext,
            &mut crate::kms::scheduler::paint_batch::PaintBatch,
            ash::vk::CommandBuffer,
        ) -> Result<(), ash::vk::Result>,
    {
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            // Either renderer_failed or vk/ops_pool unavailable. The
            // caller's existing fallback (typically log + return
            // false to fall back to pixman) handles either case.
            return Err(ash::vk::Result::ERROR_DEVICE_LOST);
        };
        self.scheduler.record_paint_batch_op(vk_arc, pool_handle, record)
    }

    /// Shim for recorders that don't need the batch handle.
    pub fn record_paint_op<F>(&mut self, record: F) -> Result<(), ash::vk::Result>
    where
        F: FnOnce(&crate::kms::vk::device::VkContext, ash::vk::CommandBuffer) -> Result<(), ash::vk::Result>,
    {
        self.record_paint_batch_op(|vk, _batch, cb| record(vk, cb))
    }
```

Both shims gate on `renderer_failed` via `paint_resources()` — if the flag is set, paint is rejected before any batch state machine work happens.

- [ ] **Step 4: Add `renderer_failed` field + gates on `composite_and_flip` and `flush_if_needed`**

In `crates/yserver/src/kms/backend.rs`, find `KmsBackend` struct definition and add the field (place near `ops_command_pool` for adjacency to other backend-state fields):

```rust
    /// Set by `composite_and_flip` / `flush_if_needed` when
    /// `PaintBatch::submit_and_wait` returns a Vk error. Once true,
    /// every paint entry point (`record_paint_op{,_batch_op}`,
    /// `flush_if_needed`, `composite_and_flip`,
    /// `try_vulkan_composite_flip`) is a no-op or early-Err.
    /// The renderer is unrecoverable in-process; an external
    /// supervisor restarts yserver to recover.
    ///
    /// See `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase3.md`
    /// "Renderer-disabled design" section.
    pub(crate) renderer_failed: bool,
```

Initialize to `false` in `KmsBackend::new` (or wherever the struct is constructed — likely 2-3 sites including `for_tests`). Use `git grep -n 'KmsBackend {' crates/yserver/src/kms/backend.rs` to find every constructor literal.

In `flush_if_needed`, both gate the body AND latch `renderer_failed` on Vk failure. The latch was previously only in `composite_and_flip`'s VisibleComposite path; doing it inside `flush_if_needed` covers ALL callers (composite, run_legacy_paint_op, the readback handlers) consistently:

```rust
    pub fn flush_if_needed(
        &mut self,
        reason: crate::kms::scheduler::paint_batch::BatchFlushReason,
    ) -> Result<(), ash::vk::Result> {
        use crate::kms::scheduler::paint_batch::{BatchError, BatchFlushReason};
        if self.renderer_failed {
            // Already failed: best-effort reasons swallow; strict
            // reasons surface ERROR_DEVICE_LOST so the caller's
            // synchronous-reply contract isn't silently broken.
            return match reason {
                BatchFlushReason::Readback
                | BatchFlushReason::ExternalSync
                | BatchFlushReason::ProtocolBarrier => Err(ash::vk::Result::ERROR_DEVICE_LOST),
                _ => Ok(()),
            };
        }
        log::trace!("flush_if_needed: reason={reason:?}");
        // ... existing close_and_submit + strict/best-effort error mapping ...
        // CHANGE: on Vk error (path-2 wait failure ⇒ abandoned
        // resources), latch renderer_failed BEFORE returning.
        // Every caller path then sees a consistent failed state on
        // the next invocation.
        match result {
            Ok(()) => Ok(()),
            Err(BatchError::Vk(r)) => {
                log::error!(
                    "flush_if_needed({reason:?}): submit_and_wait returned fatal {r:?}; \
                     latching renderer_failed — KMS renderer disabled until restart"
                );
                self.renderer_failed = true;
                Err(r)
            }
            Err(BatchError::Poisoned) if strict => {
                // ... existing arm ...
            }
            // ... existing arms ...
        }
    }
```

(The full existing body from 3A T5 stays; the only ADDED behaviour is `self.renderer_failed = true` in the `Err(BatchError::Vk(_))` arm. The existing strict-reason mapping for `Poisoned` / `InvalidState` is unchanged.)

In `composite_and_flip`, add the gate before the existing vk/pool extraction:

```rust
    pub fn composite_and_flip(&mut self) -> io::Result<()> {
        if self.renderer_failed {
            // Renderer is in fatal state; skip paint+composite. The
            // backend is alive enough to drain pageflip-completes
            // and process input — clients still see the X server,
            // they just see the last good frame on screen.
            return Ok(());
        }
        // ... existing body ...
```

The existing `flush_if_needed(VisibleComposite)` error path (3A T5 step 4) currently logs + returns `io::Error`. With `flush_if_needed` now latching `renderer_failed` itself, the composite call site simplifies:

```rust
        if let Err(e) = self.flush_if_needed(BatchFlushReason::VisibleComposite) {
            // flush_if_needed already latched renderer_failed and
            // logged the underlying Vk error. Propagate to the
            // event loop; future composite ticks early-return at
            // the top of this function via the renderer_failed gate.
            return Err(std::io::Error::other(format!(
                "PaintBatch::submit_and_wait failed: {e:?}"
            )));
        }
```

The `self.renderer_failed = true` line is gone here — it's done inside `flush_if_needed` already.

Also gate `try_vulkan_composite_flip` (it's called from inside the per-output loop in `composite_and_flip`, so the early-return above already covers it; but a defense-in-depth gate at its entry point catches future direct callers):

```rust
    fn try_vulkan_composite_flip(
        &mut self,
        layout_idx: usize,
        visible: &[u32],
    ) -> Option<(usize, usize)> {
        if self.renderer_failed {
            return None;
        }
        // ... existing body ...
```

The three readback-flush sites (`try_vk_get_image_pixels`, `hw_cursor_refresh`, `read_mirror_pixels`) already check `flush_if_needed`'s return — they get `ERROR_DEVICE_LOST` via the new gate and fall through their existing error paths. No code changes needed there.

- [ ] **Step 5: Add unit tests for the gate**

In `crates/yserver/src/kms/backend.rs` `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn renderer_failed_makes_record_paint_op_return_device_lost() {
        let mut backend = KmsBackend::for_tests();
        backend.renderer_failed = true;
        let result = backend.record_paint_op(|_, _| Ok(()));
        assert_eq!(result, Err(ash::vk::Result::ERROR_DEVICE_LOST));
    }

    #[test]
    fn renderer_failed_makes_visible_composite_flush_a_noop() {
        use crate::kms::scheduler::paint_batch::BatchFlushReason;
        let mut backend = KmsBackend::for_tests();
        backend.renderer_failed = true;
        // VisibleComposite is best-effort; gate returns Ok.
        assert!(backend
            .flush_if_needed(BatchFlushReason::VisibleComposite)
            .is_ok());
    }

    #[test]
    fn renderer_failed_makes_readback_flush_surface_device_lost() {
        use crate::kms::scheduler::paint_batch::BatchFlushReason;
        let mut backend = KmsBackend::for_tests();
        backend.renderer_failed = true;
        // Readback is strict; gate returns Err.
        assert_eq!(
            backend.flush_if_needed(BatchFlushReason::Readback),
            Err(ash::vk::Result::ERROR_DEVICE_LOST)
        );
    }

    #[test]
    fn renderer_failed_makes_composite_and_flip_a_noop() {
        let mut backend = KmsBackend::for_tests();
        backend.renderer_failed = true;
        // Even with dirty outputs, composite returns Ok early.
        assert!(backend.composite_and_flip().is_ok());
    }
```

A test for "`flush_if_needed` latches `renderer_failed` on Vk Err" needs a real or mocked `VkContext` that can be forced into a wait-failure state. If the test harness doesn't support that, leave the latch test as a hardware-smoke item in T4 step 5 and add a `#[ignore]`'d stub here documenting what it should do once a mock exists.

If `KmsBackend::for_tests()` doesn't exist verbatim, look at the existing test in `composite_and_flip_does_not_set_flip_pending_on_no_vk_path` (around line 11877) for the test harness pattern; reuse it.

- [ ] **Step 6: Verify**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Expected: 4 new tests pass. The existing `record_paint_op` test from 3A T5 (`record_paint_op_returns_init_failure_without_vk`, if it exists) may need its expected error code updated — `paint_resources()` returning `None` now collapses the no-vk case to `ERROR_DEVICE_LOST` from the shim. Update or delete that test as appropriate.

- [ ] **Step 7: Commit**

```bash
git add crates/yserver/src/kms/scheduler/mod.rs crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): RenderScheduler::record_paint_op + renderer_failed gate + paint_resources helper"
```

---

## Task 2: Migrate `fill` family

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (4 call sites by current grep count)

The simplest family — no scratch deps. Two recorders (`fill::record_fill_rectangles`, `fill::record_logic_fill`); 4 call sites in `backend.rs`.

**Per-call-site transformation template.** Original (post-T0, every fill site is inside `run_legacy_paint_op`):

```rust
        // ... compute color, rects, scissor, get mirror ...
        self.run_legacy_paint_op(|vk, cb| {
            fill::record_fill_rectangles(vk, cb, mirror, color, &rects, scissor)
        })
```

Migrated (uses `paint_resources()` so the `renderer_failed` gate runs; `&mut self.scheduler` is disjoint from `&mut self.windows` / `.pixmaps`):

```rust
        let (vk_arc, pool_handle) = self.paint_resources()?;
        // ... compute color, rects, scissor, get mirror ...
        self.scheduler.record_paint_op(vk_arc, pool_handle, |vk, cb| {
            fill::record_fill_rectangles(vk, cb, mirror, color, &rects, scissor)
        })
```

If the function's return type doesn't match `paint_resources()`'s `Option`, adapt:

```rust
        let Some((vk_arc, pool_handle)) = self.paint_resources() else {
            return false; // or whatever the existing no-vk path returns
        };
```

**For sites that T0 had to leave on the borrow-conflict fallback** (raw `run_one_shot_op` with explicit `flush_if_needed(ProtocolBarrier)` first): replace the whole construct with `paint_resources()` + scheduler-direct call as above. The explicit flush is no longer needed because `record_paint_op` appends to the batch instead of submitting independently. Remove the flush call.

The `run_one_shot_op` import becomes unused if no other op in the same function uses it; remove from the function-scope `use` if so. The `fill` import stays.

**Renderer-failed handling.** `paint_resources()` returns `None` if `renderer_failed` is set — caller's existing no-vk fallback (log-and-fall-back-to-pixman, or return false) handles the failed case the same way it handled "Vk unavailable" before.

**3A T4 audit confirmed**: both fill recorders have NO error paths from Rust's side (Vulkan calls are in `unsafe { }` blocks returning `()`). So `Poisoned` is unreachable from fill.

- [ ] **Step 1: Locate every fill call site**

```bash
rg -nB2 -A5 'fill::record_fill_rectangles|fill::record_logic_fill' crates/yserver/src/kms/backend.rs
```

Expected: 4 hits at approximately lines 2628, 3434, 3548, plus one more (audit reports the family count = 4).

- [ ] **Step 2: Migrate each fill call site**

For each site, apply the transformation template. Sites known from 3A's grep:

- **~2628** — `fill_solid_cursor_color` (or similar) calling `record_fill_rectangles`. The function returns `Result<(), vk::Result>`; `mirror` is `&mut DrawableImage` already.
- **~3434** — `try_vk_logic_fill` (or similar) calling `record_logic_fill`. Returns a result that includes a `Vec<vk::Rect2D>` and a pipeline. Borrow shape: `mirror`, `pipeline`, `pipeline_layout`, `fg_color`, `vk_rects`, `clip_scissor` are all locals already.
- **~3548** — `try_vk_fill_rectangles` (or similar) calling `record_fill_rectangles`. Same shape as 3434.
- The fourth site is in the same function as one of the above (e.g., a `match` arm) — find it via the grep.

For each: change `run_one_shot_op(&vk_arc, pool_handle, |vk, cb| ...)` to `self.scheduler.record_paint_op(vk_arc, pool_handle, |vk, cb| ...)`. `vk_arc` is moved (not borrowed) so don't dereference with `&`. Drop `run_one_shot_op` from the imports if it has no other callers in the same function.

If a site fails to compile due to a borrow conflict the transformation template doesn't anticipate, **STOP and report** rather than restructuring on your own. The likely cause would be `vk_arc` or `pool_handle` extracted in a way that requires `&self` to outlive the closure — fix by reordering the let-bindings so the immutable borrows on `self.vk` / `self.ops_command_pool` end before the `&mut self.windows` borrow is taken.

- [ ] **Step 3: Verify**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Expected: all green. No new warnings. The composite cycle log should show the batch transitioning to `Recording` for the first time (a fill request was migrated and its work is appended).

- [ ] **Step 4: Audit cutover**

`run_one_shot_op` and the recorder name are on different lines, so single-line greps miss them. Use `rg -U` (multiline) or grep for the recorder name with context:

```bash
# Show context around every fill recorder call site. Each one
# should now be inside `self.scheduler.record_paint_op(...)`,
# NOT inside `run_one_shot_op(...)`.
rg -nB2 -A2 'fill::record_fill_rectangles|fill::record_logic_fill' crates/yserver/src/kms/backend.rs

# Or multiline:
rg -nU 'run_one_shot_op[^)]*\)\s*,\s*\|[^|]*\|\s*\{[^}]*fill::record_' crates/yserver/src/kms/backend.rs
# Expected: zero matches.
```

Inspect each `fill::record_*` hit visually: confirm the call is wrapped in `self.scheduler.record_paint_op(...)`, not `run_one_shot_op(...)`.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "refactor(kms): migrate fill recorders to PaintBatch via record_paint_op"
```

---

## Task 3: Migrate `copy` distinct + same (NOT same_overlap)

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (4 call sites by current grep count)

Two recorders (`copy::record_copy_area_distinct`, `copy::record_copy_area_same`); 4 call sites. Same transformation template as T2 (fill).

**Critical scoping:** `copy::record_copy_area_same_overlap` (the third copy recorder) is NOT migrated here. It uses `CopyScratch`, a backend-shared scratch image; the second use within one batch would alias the first. Defers to 3D after 3C lands the per-batch upload arena strategy that informs how shared scratch images are handled.

3A T4 audit confirmed: `record_copy_area_distinct` and `record_copy_area_same` both delegate to infallible private helpers (`record_distinct_image_copy`, `record_same_image_copy`); no Rust-side error paths. `Poisoned` unreachable.

- [ ] **Step 1: Locate every distinct+same copy call site**

```bash
rg -nB2 -A5 'copy::record_copy_area_distinct|copy::record_copy_area_same\b|vk_copy::record_copy_area_distinct|vk_copy::record_copy_area_same\b' crates/yserver/src/kms/backend.rs
```

Expected: 4 hits at approximately lines 2670, 2725, 3140, 3299. Verify by running the grep — the line numbers may have shifted since 3A.

The `\b` word boundary in the grep is important: it excludes `copy::record_copy_area_same_overlap` which is deferred.

- [ ] **Step 2: Migrate each call site**

Same transformation as T2 (fill). Post-T0, each site is inside `run_legacy_paint_op`. Replace with `paint_resources()` + `self.scheduler.record_paint_op(...)`. Examples:

```rust
// Before (post-T0, at ~2670):
if let Err(e) = self.run_legacy_paint_op(|vk, cb| {
    vk_copy::record_copy_area_distinct(vk, cb, src, &mut cm, &regions)
}) { ... }

// After:
let (vk_arc, pool_handle) = self.paint_resources()?;
if let Err(e) = self.scheduler.record_paint_op(vk_arc, pool_handle, |vk, cb| {
    vk_copy::record_copy_area_distinct(vk, cb, src, &mut cm, &regions)
}) { ... }
```

```rust
// Before (post-T0, at ~3140):
return match self.run_legacy_paint_op(|vk, cb| {
    copy::record_copy_area_same(vk, cb, mirror, &regions)
}) { ... };

// After:
let Some((vk_arc, pool_handle)) = self.paint_resources() else {
    return false;
};
return match self.scheduler.record_paint_op(vk_arc, pool_handle, |vk, cb| {
    copy::record_copy_area_same(vk, cb, mirror, &regions)
}) { ... };
```

For borrow-conflict fallback sites (still on raw `run_one_shot_op` with explicit pre-flush from T0): same pattern as T2 — replace the whole construct with `paint_resources()` + scheduler-direct call, drop the explicit flush call.

Drop `run_one_shot_op` from function-scope imports where no other op uses it.

- [ ] **Step 3: Verify**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

- [ ] **Step 4: Audit cutover**

```bash
# Show context around every distinct + same recorder call. Each
# should now be inside `self.scheduler.record_paint_op(...)`.
rg -nB2 -A2 'copy::record_copy_area_distinct|copy::record_copy_area_same\b|vk_copy::record_copy_area_distinct|vk_copy::record_copy_area_same\b' crates/yserver/src/kms/backend.rs

# Note: the `\b` excludes `record_copy_area_same_overlap`.

# same_overlap should still be inside `run_legacy_paint_op`
# (T0 wrapped it):
rg -nB2 -A2 'record_copy_area_same_overlap' crates/yserver/src/kms/backend.rs
# Expected: still inside run_legacy_paint_op (or the borrow-conflict
# fallback). NOT inside record_paint_op.
```

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "refactor(kms): migrate copy distinct+same recorders to PaintBatch (same_overlap deferred to 3D)"
```

---

## Task 4: Validation + results doc

**Files:**
- Create: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3b-results.md`

End-to-end validation. The first time `flush_if_needed(VisibleComposite)` flushes a non-Idle batch with real recorded work — if anything's broken in 3A's state machine, this is where it surfaces.

- [ ] **Step 1: Pre-flight checks**

```bash
cargo +nightly fmt --check
cargo clippy
cargo test
```

Expected: all three exit 0.

- [ ] **Step 2: Cutover greps**

Use context-greps (recorder name + surrounding lines) since `run_one_shot_op` and the recorder are on different lines.

```bash
# Migrated fill sites: each `fill::record_*` should be inside
# `self.scheduler.record_paint_op(...)`. Inspect visually.
rg -nB2 -A2 'fill::record_fill_rectangles|fill::record_logic_fill' crates/yserver/src/kms/backend.rs

# Migrated copy sites: each `record_copy_area_distinct` / `record_copy_area_same`
# (with word boundary to exclude same_overlap) should be inside
# `self.scheduler.record_paint_op(...)`.
rg -nB2 -A2 'record_copy_area_distinct|record_copy_area_same\b' crates/yserver/src/kms/backend.rs

# same_overlap should be inside `run_legacy_paint_op(...)`
# (T0 wrapped) or its borrow-conflict fallback. NOT inside
# record_paint_op (3D will migrate).
rg -nB2 -A2 'record_copy_area_same_overlap' crates/yserver/src/kms/backend.rs

# Total raw `run_one_shot_op` calls should be down significantly
# from the phase-2 baseline (39 sites): 4 fill + 4 copy migrated
# to record_paint_op; ~25 sites wrapped via `run_legacy_paint_op`
# (the wrapper itself contains 1 raw call). Most remaining raw
# `run_one_shot_op` hits should be inside `run_legacy_paint_op`'s
# body or borrow-conflict fallbacks.
rg -c 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs

# renderer_failed wired:
rg -n 'renderer_failed' crates/yserver/src/kms/backend.rs
# Expected: field declaration + ~5 gate sites + 4 test references.

# paint_resources is the gateway for direct scheduler calls:
rg -n 'paint_resources\(' crates/yserver/src/kms/backend.rs
# Expected: ~8-12 hits (4 fill + 4 copy + a few others) plus the definition.
```

- [ ] **Step 3: XTS regression**

```bash
just xts-yserver
```

Expected: matches phase-2 / phase-3A xts baseline. If new failures, the most likely cause is a fill/copy call site whose borrow conflict was resolved incorrectly (e.g., the closure captures by `&mut` and the recorder runs after the implicit `&mut self.windows` was released too early — produces stale data in the mirror).

- [ ] **Step 4: rendercheck**

```bash
just rendercheck-yserver
```

Expected: matches phase-2 baseline.

- [ ] **Step 5: Hardware smoke**

User runs the desktop session (`just yserver-mate-hw-release` or local equivalent). Verify:

- Desktop comes up; window paint works.
- Solid-fill ops (panel backgrounds, button hover states) render correctly.
- CopyArea ops (window move/resize, scrolling-without-overlap) render correctly.
- Cursor mirror rendering still works (cursor-related fill + copy migrated).
- No `paint batch submit failed` warnings.
- No `renderer_failed` log messages — that flag should NEVER fire in normal operation.
- mate-control-center hover does not regress GPU saturation vs phase 2.

- [ ] **Step 6: Write results doc**

Create `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3b-results.md` modeled on phase-3A's. Sections:

- Scope landed (T0 + T1 + T2 + T3).
- Preflight checks.
- Cutover grep results (paste actual output).
- XTS / rendercheck status.
- Hardware smoke notes (any GPU-saturation observations under mate-control-center hover).
- Any plan bugs caught during execution.
- Done conditions checklist.
- Commit summary.
- Known deferred items.
- "What's next" — 3C planning (PutImage, mirror upload, mask scratch via BatchUploadArena).

- [ ] **Step 7: Commit**

```bash
git add docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3b-results.md
git commit -m "docs: phase-3B rendering re-architecture validation results"
```

---

## Done conditions

3B is complete when:

1. All 5 tasks committed; tree green (`cargo test`, `cargo clippy`, `cargo fmt --check`).
2. `KmsBackend::run_legacy_paint_op` exists and wraps `flush_if_needed(ProtocolBarrier)` + `run_one_shot_op`. After T0, every paint-side `run_one_shot_op` call site is either (a) inside `run_legacy_paint_op`'s dispatch body, (b) a borrow-conflict fallback that still does explicit pre-flush, or (c) one of the three `Readback` handlers. After T2 and T3, the fill and copy-distinct/same sites move further to `record_paint_op`; same_overlap, render, text, traps, etc. stay on `run_legacy_paint_op` until 3C/3D.
3. `RenderScheduler::record_paint_op` and `record_paint_batch_op` exist; `KmsBackend`'s versions are thin shims that gate via `paint_resources()`.
4. `KmsBackend::paint_resources()` returns `Option<(Arc<VkContext>, vk::CommandPool)>` and gates on `renderer_failed`. Every direct `self.scheduler.record_paint_op(...)` call site goes through it.
5. `KmsBackend::renderer_failed` field gates `record_paint_op{,_batch_op}` (via `paint_resources`), `flush_if_needed`, `composite_and_flip`, `try_vulkan_composite_flip`, `run_legacy_paint_op`.
6. `flush_if_needed` latches `renderer_failed = true` on any fatal Vk error from `submit_and_wait` (any reason, any caller — composite, legacy-paint, readback handlers all latch consistently). `composite_and_flip` propagates the resulting `io::Error` upward; no per-call-site latching duplication.
7. All 4 `fill` family call sites in `backend.rs` migrated to `self.scheduler.record_paint_op(...)`.
8. All 4 `copy::record_copy_area_distinct` + `record_copy_area_same` call sites migrated.
9. `copy::record_copy_area_same_overlap` is wrapped via `run_legacy_paint_op` (deferred from migration to 3D).
10. XTS / rendercheck / hardware smoke green; no regressions vs phase 3A.
11. The `flush_if_needed(VisibleComposite)` path in `composite_and_flip` is now load-bearing — it carries real recorded work into `submit_and_wait` for the first time.
12. Mixed batched + legacy paint ops on the same drawable produce correct rendering — the load-bearing test is XTS5 + rendercheck, both of which exercise interleaved fill/copy/render sequences.

## What's next

**Phase 3C** — migrate upload-backed paint (PutImage / mirror upload via `BatchUploadArena`; then `MaskScratch::record_upload_r8`; then glyph atlas upload; then gradient upload). Each migration converts a scratch's "internal `run_one_shot_op` with shared staging" into "record into the batch CB; upload bytes via `BatchUploadArena`."

Pre-3C audit work:
- Late-mutation audit of `image::record_put_image` (3A T4 deferred this).
- Decision on `MaskScratch` shared-image aliasing: serialize upload+draw within one CB closure, or per-batch mask images?
- Decision on gradient lifecycle: gradients are created once per `RenderCreateLinearGradient` (not per-frame), so they could either flush on create (synchronous upload) or defer the upload to the next batch open.

3C planning happens after 3B lands — the API ergonomics that 3B's borrow-split pattern reveals will inform the 3C migration shape.
