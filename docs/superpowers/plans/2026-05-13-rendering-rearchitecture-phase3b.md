# Rendering re-architecture — phase 3B implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

Date: 2026-05-13
Status: ready for execution
Branch: `graphics-followups`
Predecessor: phase 3A (`4af9e01` results doc; tip `4af9e01`)

**Goal:** Migrate the four scratch-free paint recorders (`fill::record_fill_rectangles`, `fill::record_logic_fill`, `copy::record_copy_area_distinct`, `copy::record_copy_area_same`) from `run_one_shot_op` (per-op submit + `vkQueueWaitIdle`) to appending into the per-frame `PaintBatch` via the 3A APIs. After 3B, every call site for these four recorders goes through `record_paint_op`; the batch carries real recorded work into `flush_if_needed(VisibleComposite)` for the first time. **`copy::record_copy_area_same_overlap` is deferred to 3D** (uses `CopyScratch` shared scratch image; needs the upload-arena infrastructure 3C lays).

**Architecture:** 3A's `record_paint_op` / `record_paint_batch_op` live on `KmsBackend` and are `&mut self` — that conflicts with the `&mut self.windows[id].vk_mirror` borrows recorder call sites already hold. **3B T0 moves the implementation onto `RenderScheduler`** so call sites can split borrows: `&mut self.scheduler` is disjoint from `&mut self.windows`/`&mut self.pixmaps`. The KmsBackend methods become thin shims for callers that don't hold a conflicting borrow. T0 also lands the **`renderer_failed: bool` gate** that prevents cascade-of-abandoned-CBs once T1's first recorder can produce a fatal `Err`.

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

## Task 0: `renderer_failed` gate + move record_paint_op onto RenderScheduler

**Files:**
- Modify: `crates/yserver/src/kms/scheduler/paint_batch.rs` (no change to PaintBatch; just touched if needed)
- Modify: `crates/yserver/src/kms/scheduler/mod.rs` (add `record_paint_op` + `record_paint_batch_op` to `RenderScheduler`)
- Modify: `crates/yserver/src/kms/backend.rs` (add `renderer_failed` field + gates; reduce existing `record_paint_op{,_batch_op}` to thin shims; gate `composite_and_flip` and `flush_if_needed`)

This task lands the infrastructure that T1/T2 build on. **Two orthogonal pieces** — could split into T0a/T0b, but they're tightly coupled (the gate runs at the same entry points the move touches), so single task.

### Why move to `RenderScheduler`?

3A put `record_paint_op` on `KmsBackend` (taking `&mut self`). T1's first call site looks like:

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

The KmsBackend `record_paint_op` / `record_paint_batch_op` become shims for callers that don't hold a conflicting borrow. T1 will reveal whether they're actually used; if not, T2 can delete them.

### `renderer_failed` flag — Option A from the design appendix

`composite_and_flip` returns fatal Vk errors via `io::Error::other(...)`, but the trait-surface `Backend::on_page_flip_ready` returns `()` and drops the error on the floor. Once T1 migrates fill, a fatal `submit_and_wait` failure produces an abandoned `Submitted`-state batch; the next composite cycle would try to use it and produce more abandoned CBs each tick.

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
        vk: Arc<VkContext>,
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
        let _ = self.open_batch(vk.clone(), pool);
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
        match record(&vk, batch, cb) {
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
        vk: Arc<VkContext>,
        pool: vk::CommandPool,
        record: F,
    ) -> Result<(), vk::Result>
    where
        F: FnOnce(&VkContext, vk::CommandBuffer) -> Result<(), vk::Result>,
    {
        self.record_paint_batch_op(vk, pool, |vk, _batch, cb| record(vk, cb))
    }
```

The `Arc<VkContext>` and `vk::CommandPool` types are already in scope (file imports them at the top from 3A T1 step 2). If not, add the imports.

- [ ] **Step 2: Reduce `KmsBackend::record_paint_op` / `record_paint_batch_op` to thin shims**

In `crates/yserver/src/kms/backend.rs`, replace the existing bodies (added in 3A T5) with:

```rust
    /// Shim: pull vk + ops pool, delegate to the scheduler-level
    /// `record_paint_batch_op`. Useful when the caller doesn't hold
    /// a conflicting `&mut self.windows` / `.pixmaps` borrow.
    /// Recorders that DO hold such a borrow must call
    /// `self.scheduler.record_paint_batch_op(vk_arc, pool_handle, ...)`
    /// directly (use field projection on self at the call site).
    pub fn record_paint_batch_op<F>(&mut self, record: F) -> Result<(), ash::vk::Result>
    where
        F: FnOnce(
            &crate::kms::vk::device::VkContext,
            &mut crate::kms::scheduler::paint_batch::PaintBatch,
            ash::vk::CommandBuffer,
        ) -> Result<(), ash::vk::Result>,
    {
        if self.renderer_failed {
            return Err(ash::vk::Result::ERROR_DEVICE_LOST);
        }
        let Some(vk_arc) = self.vk.as_ref().cloned() else {
            return Err(ash::vk::Result::ERROR_INITIALIZATION_FAILED);
        };
        let Some(pool_handle) = self.ops_command_pool.as_ref().map(|p| p.handle()) else {
            return Err(ash::vk::Result::ERROR_INITIALIZATION_FAILED);
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

Both shims now also gate on `renderer_failed` — if the flag is set, paint is rejected before any batch state machine work happens.

- [ ] **Step 3: Add `renderer_failed` field + gates on `composite_and_flip` and `flush_if_needed`**

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

In `flush_if_needed`, gate the body:

```rust
    pub fn flush_if_needed(
        &mut self,
        reason: crate::kms::scheduler::paint_batch::BatchFlushReason,
    ) -> Result<(), ash::vk::Result> {
        use crate::kms::scheduler::paint_batch::{BatchError, BatchFlushReason};
        if self.renderer_failed {
            // Best-effort reasons swallow; strict reasons surface.
            return match reason {
                BatchFlushReason::Readback
                | BatchFlushReason::ExternalSync
                | BatchFlushReason::ProtocolBarrier => Err(ash::vk::Result::ERROR_DEVICE_LOST),
                _ => Ok(()),
            };
        }
        log::trace!("flush_if_needed: reason={reason:?}");
        // ... existing body unchanged ...
    }
```

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

In the existing `flush_if_needed(VisibleComposite)` error path (T5 step 4), set the flag:

```rust
        if let Err(e) = self.flush_if_needed(BatchFlushReason::VisibleComposite) {
            log::error!(
                "composite cycle: paint batch flush returned fatal {e:?}; \
                 marking renderer_failed — KMS renderer disabled until restart"
            );
            self.renderer_failed = true;
            return Err(std::io::Error::other(format!(
                "PaintBatch::submit_and_wait failed: {e:?}"
            )));
        }
```

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

- [ ] **Step 4: Add unit tests for the gate**

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

If `KmsBackend::for_tests()` doesn't exist verbatim, look at the existing test in `composite_and_flip_does_not_set_flip_pending_on_no_vk_path` (around line 11877) for the test harness pattern; reuse it.

- [ ] **Step 5: Verify**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Expected: 4 new tests pass. The existing `record_paint_op` test from 3A T5 (`record_paint_op_returns_init_failure_without_vk`, if it exists) still passes — the no-vk path is hit before the renderer_failed check.

Verify the shim still works as before for the no-borrow-conflict case:
```bash
cargo test -p yserver kms
```

- [ ] **Step 6: Commit**

```bash
git add crates/yserver/src/kms/scheduler/mod.rs crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): RenderScheduler::record_paint_op + renderer_failed gate"
```

---

## Task 1: Migrate `fill` family

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (4 call sites by current grep count)

The simplest family — no scratch deps. Two recorders (`fill::record_fill_rectangles`, `fill::record_logic_fill`); 4 call sites in `backend.rs`.

**Per-call-site transformation template.** Original:

```rust
        let vk_arc = self.vk.as_ref().cloned()?;
        let pool_handle = self.ops_command_pool.as_ref()?.handle();
        // ... compute color, rects, scissor, get mirror ...
        run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
            fill::record_fill_rectangles(vk, cb, mirror, color, &rects, scissor)
        })
```

Migrated (`&mut self.scheduler` is disjoint from `&mut self.windows` / `.pixmaps`):

```rust
        let vk_arc = self.vk.as_ref().cloned()?;
        let pool_handle = self.ops_command_pool.as_ref()?.handle();
        // ... compute color, rects, scissor, get mirror ...
        self.scheduler.record_paint_op(vk_arc, pool_handle, |vk, cb| {
            fill::record_fill_rectangles(vk, cb, mirror, color, &rects, scissor)
        })
```

The `run_one_shot_op` import becomes unused if no other op in the same function uses it; remove from the function-scope `use` if so. The `fill` import stays.

**Renderer-failed handling.** `record_paint_op` already gates on `self.renderer_failed` via the shim chain. If the backend is failed, the recorder returns `Err(ERROR_DEVICE_LOST)` — caller's existing error path (typically log-and-fall-back-to-pixman) runs.

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

```bash
# Should be zero hits — all fill call sites migrated:
rg -n 'run_one_shot_op.*fill::' crates/yserver/src/kms/backend.rs
# Or with the imports pattern:
rg -n 'run_one_shot_op\(.*\n.*fill::record_' crates/yserver/src/kms/backend.rs
```

If non-zero, you missed a site. Migrate it.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "refactor(kms): migrate fill recorders to PaintBatch via record_paint_op"
```

---

## Task 2: Migrate `copy` distinct + same (NOT same_overlap)

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (4 call sites by current grep count)

Two recorders (`copy::record_copy_area_distinct`, `copy::record_copy_area_same`); 4 call sites. Same transformation template as T1.

**Critical scoping:** `copy::record_copy_area_same_overlap` (the third copy recorder) is NOT migrated here. It uses `CopyScratch`, a backend-shared scratch image; the second use within one batch would alias the first. Defers to 3D after 3C lands the per-batch upload arena strategy that informs how shared scratch images are handled.

3A T4 audit confirmed: `record_copy_area_distinct` and `record_copy_area_same` both delegate to infallible private helpers (`record_distinct_image_copy`, `record_same_image_copy`); no Rust-side error paths. `Poisoned` unreachable.

- [ ] **Step 1: Locate every distinct+same copy call site**

```bash
rg -nB2 -A5 'copy::record_copy_area_distinct|copy::record_copy_area_same\b|vk_copy::record_copy_area_distinct|vk_copy::record_copy_area_same\b' crates/yserver/src/kms/backend.rs
```

Expected: 4 hits at approximately lines 2670, 2725, 3140, 3299. Verify by running the grep — the line numbers may have shifted since 3A.

The `\b` word boundary in the grep is important: it excludes `copy::record_copy_area_same_overlap` which is deferred.

- [ ] **Step 2: Migrate each call site**

Same transformation as T1. Examples:

```rust
// Before (~2670):
if let Err(e) = run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
    vk_copy::record_copy_area_distinct(vk, cb, src, &mut cm, &regions)
}) { ... }

// After:
if let Err(e) = self.scheduler.record_paint_op(vk_arc, pool_handle, |vk, cb| {
    vk_copy::record_copy_area_distinct(vk, cb, src, &mut cm, &regions)
}) { ... }
```

```rust
// Before (~3140):
return match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
    copy::record_copy_area_same(vk, cb, mirror, &regions)
}) { ... };

// After:
return match self.scheduler.record_paint_op(vk_arc, pool_handle, |vk, cb| {
    copy::record_copy_area_same(vk, cb, mirror, &regions)
}) { ... };
```

Drop `run_one_shot_op` from function-scope imports where no other op uses it.

- [ ] **Step 3: Verify**

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

- [ ] **Step 4: Audit cutover**

```bash
# Distinct + same should be zero:
rg -n 'run_one_shot_op.*copy::record_copy_area_distinct\|run_one_shot_op.*copy::record_copy_area_same\b' crates/yserver/src/kms/backend.rs

# same_overlap should still have its hit (deferred to 3D):
rg -n 'run_one_shot_op.*record_copy_area_same_overlap' crates/yserver/src/kms/backend.rs
# Expected: 1 hit
```

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "refactor(kms): migrate copy distinct+same recorders to PaintBatch (same_overlap deferred to 3D)"
```

---

## Task 3: Validation + results doc

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

```bash
# Migrated families: zero hits.
rg -n 'run_one_shot_op.*fill::' crates/yserver/src/kms/backend.rs
rg -n 'run_one_shot_op.*copy::record_copy_area_distinct\|run_one_shot_op.*copy::record_copy_area_same\b' crates/yserver/src/kms/backend.rs

# same_overlap still on run_one_shot_op (3D scope):
rg -n 'run_one_shot_op.*record_copy_area_same_overlap' crates/yserver/src/kms/backend.rs

# Other families still on run_one_shot_op (3C/3D scope):
rg -c 'run_one_shot_op' crates/yserver/src/kms/backend.rs
# Expected: down by ~8 from phase-2 baseline (4 fill + 4 copy = 8 sites moved); other families unchanged.

# renderer_failed wired:
rg -n 'renderer_failed' crates/yserver/src/kms/backend.rs
# Expected: field declaration + 4-5 gate sites + 4 test references.
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

- Scope landed (T0 + T1 + T2).
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

1. All 3 tasks committed; tree green (`cargo test`, `cargo clippy`, `cargo fmt --check`).
2. `RenderScheduler::record_paint_op` and `record_paint_batch_op` exist; `KmsBackend`'s versions are thin shims.
3. `KmsBackend::renderer_failed` field gates `record_paint_op{,_batch_op}`, `flush_if_needed`, `composite_and_flip`, `try_vulkan_composite_flip`.
4. `composite_and_flip` sets `renderer_failed = true` on fatal Vk error from `flush_if_needed(VisibleComposite)`.
5. All 4 `fill` family call sites in `backend.rs` migrated to `self.scheduler.record_paint_op(...)`.
6. All 4 `copy::record_copy_area_distinct` + `record_copy_area_same` call sites migrated.
7. `copy::record_copy_area_same_overlap` still uses `run_one_shot_op` (deferred to 3D).
8. XTS / rendercheck / hardware smoke green; no regressions vs phase 3A.
9. The `flush_if_needed(VisibleComposite)` path in `composite_and_flip` is now load-bearing — it carries real recorded work into `submit_and_wait` for the first time.

## What's next

**Phase 3C** — migrate upload-backed paint (PutImage / mirror upload via `BatchUploadArena`; then `MaskScratch::record_upload_r8`; then glyph atlas upload; then gradient upload). Each migration converts a scratch's "internal `run_one_shot_op` with shared staging" into "record into the batch CB; upload bytes via `BatchUploadArena`."

Pre-3C audit work:
- Late-mutation audit of `image::record_put_image` (3A T4 deferred this).
- Decision on `MaskScratch` shared-image aliasing: serialize upload+draw within one CB closure, or per-batch mask images?
- Decision on gradient lifecycle: gradients are created once per `RenderCreateLinearGradient` (not per-frame), so they could either flush on create (synchronous upload) or defer the upload to the next batch open.

3C planning happens after 3B lands — the API ergonomics that 3B's borrow-split pattern reveals will inform the 3C migration shape.
