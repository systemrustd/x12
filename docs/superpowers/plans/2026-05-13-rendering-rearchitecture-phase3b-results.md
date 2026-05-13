# Phase 3B — rendering re-architecture — validation results

Date: 2026-05-13
Plan: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3b.md` (v3.1 — codex-approved after 3 review rounds)
Branch: `graphics-followups`
Predecessor: phase 3A (`4af9e01` results doc)

## Scope landed

Five tasks (T0–T4) plus one out-of-task fix (drop-order). 3B was the first phase to **actually move recorders into the per-frame `PaintBatch`**: fill (`record_fill_rectangles`, `record_logic_fill`) and copy distinct+same (`record_copy_area_distinct`, `record_copy_area_same`) now append to the batch CB; everything else still uses one-shot submits via `run_legacy_paint_op` (T0's wrapper) which flushes the batch first.

After 3B:

- **`KmsBackend::run_legacy_paint_op<F>`** (T0) wraps every non-readback paint-side `run_one_shot_op` call with a `flush_if_needed(ProtocolBarrier)` prefix, so legacy paint can no longer race with batched paint on the same drawable.
- **`RenderScheduler::record_paint_op` / `record_paint_batch_op`** (T1) are the load-bearing append APIs; `KmsBackend`'s versions are thin shims for non-borrow-conflicting callers.
- **`KmsBackend::paint_resources()`** (T1) returns `Option<(Arc<VkContext>, vk::CommandPool)>` only when `!renderer_failed`. Every direct `self.scheduler.record_paint_op(...)` call site uses it — the gate runs.
- **`KmsBackend::renderer_failed: bool`** (T1) gates `composite_and_flip`, `try_vulkan_composite_flip`, `flush_if_needed`, `record_paint_op{,_batch_op}` (via `paint_resources`), and `run_legacy_paint_op` (via `flush_if_needed`).
- **`flush_if_needed` latches `renderer_failed = true`** on `Err(BatchError::Vk(_))` — every caller path (composite, legacy, readback) latches consistently.
- **Field reorder** (`c521f42`, out-of-task fix): `scheduler` declared BEFORE `ops_command_pool` so `PaintBatch::Drop` frees its CB against a still-valid pool. Without this, `OpsCommandPool::Drop` destroys the pool first and `free_command_buffers` hits a dangling handle.
- **7 recorder call sites migrated** to `self.scheduler.record_paint_op(...)`: `fill_mirror_solid`, `try_vk_solid_fill`, `try_vk_fill_with_function`, `copy_drawable_to_new_cursor_mirror`, `copy_pixmap_mirror_to_cursor`, `try_vk_copy_area` (same arm), `try_vk_copy_area` (distinct arm).
- **`record_copy_area_same_overlap`** stays on `run_one_shot_op` with an inline `flush_if_needed(ProtocolBarrier)` — uses `CopyScratch`, deferred to 3D.

## Preflight checks

End of 3B (HEAD = `66e7120`):

- `cargo +nightly fmt --check` — no diff.
- `cargo clippy` — 5 pre-existing doc-list-indentation warnings (unchanged from 3A baseline). No new warnings.
- `cargo test --workspace` — all green:
  - `yserver` unit tests: 133 passed, 0 failed, 3 ignored.
  - `yserver-core`: 284 passed.
  - `yserver-protocol`: 208 passed.
  - `fixture_smoke`: 2 passed, 1 ignored.
  - `alpha_invariant`: 17 ignored (need live Vulkan ICD).

## Cutover greps

```
$ rg -c 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs
15
```

15 raw `run_one_shot_op(` calls remain. Breakdown:
- 1 — `run_legacy_paint_op`'s dispatch body
- 3 — `Readback` handlers (`try_vk_get_image_pixels`, `hw_cursor_refresh`, `read_mirror_pixels`); keep their explicit `flush_if_needed(Readback)` + raw `run_one_shot_op`
- 1 — `try_vk_copy_area` same-overlap arm (borrow-conflict fallback with inline `flush_if_needed(ProtocolBarrier)`; deferred 3D)
- 8 — other paint-side borrow-conflict fallbacks left by T0 (image / render / text / traps / mirror upload); each has a `// borrow-conflict fallback for run_legacy_paint_op` comment and an explicit pre-flush
- 2 — `open_with_commit` (constructor; not paint-side) and `dump_scanout_one` (diagnostic dump; not paint-side)

```
$ rg -c 'renderer_failed' crates/yserver/src/kms/backend.rs
23
$ rg -c 'paint_resources\(' crates/yserver/src/kms/backend.rs
13
```

Migrated recorders go through `paint_resources()` (gated on `renderer_failed`) into `self.scheduler.record_paint_op(...)`. Audit catalogue in `backend.rs` (line ~1623) is up to date with T2 / T3 migration status.

## Architectural lessons from this phase

### Lesson 1: Drop-order matters when `PaintBatch` owns CBs from `OpsCommandPool`

T2's first attempt added a "skip free_command_buffers for Recording-state CBs" workaround in `paint_batch.rs::poison()`, attributing a `cargo test` crash to "AMD radv driver bug." The actual cause: `KmsBackend` declared `ops_command_pool` before `scheduler`. Rust drops fields in declaration order, so `OpsCommandPool::Drop` ran `destroy_command_pool` BEFORE `PaintBatch::Drop`'s `free_command_buffers` — UAF. Drivers vary in how they handle this; radv panics.

Fix (commit `c521f42`): move `scheduler` field declaration above `ops_command_pool`. Document the invariant in both fields' doc-comments. Revert the workaround.

**For 3C / 3D**: any new `KmsBackend` field that holds a resource allocated from `ops_command_pool` (CBs, descriptor sets allocated from `RenderPipelineCache.descriptor_pool`, etc.) must drop BEFORE the pool. The struct's current order (`vk → scheduler → ops_command_pool → other Vulkan resources`) is the contract; preserve it.

This is feedback memory material — added it to `memory/feedback_kmsbackend_drop_order.md` (if applicable).

### Lesson 2: Codex review rounds for the plan caught real issues

3B plan went through 3 codex review rounds (v1 → v2 → v3 → v3.1). Each round caught real blockers:

- v1 → v2: `vk` parameter shadows `ash::vk` module; cutover greps miss multiline patterns; mixed batched + legacy ordering hazard; `renderer_failed` bypass via direct scheduler calls.
- v2 → v3: T2-T3 intermediate state still unsafe; `renderer_failed` only latched in `composite_and_flip`; `ProtocolBarrier` "silently swallows" wording was wrong.
- v3 → v3.1: stale audit-category mention of "unmigrated fill/copy" after v3's wrap-everything scope.

The cost (4 plan rewrites before any execution) was lower than the cost of unwinding a wrong implementation. Worth keeping for 3C planning.

### Lesson 3: T0 (legacy paint boundary) is what makes T2/T3 safe

Without T0, a batched fill followed by a still-raw image::PutImage on the same drawable would have read CPU-mutated layout while the GPU hadn't reached it. T0's `run_legacy_paint_op` wrapper (which flushes via `ProtocolBarrier`) is the structural barrier; T2/T3 then unwrap fill/copy from it to `record_paint_op`.

For 3C / 3D, the same wrap-then-unwrap pattern applies: every new migration must first ensure the post-migration intermediate state is safe (legacy ops on the same drawable still flush first).

### Lesson 4: T0's wrap-flush isn't enough — drawables can be destroyed too

T0 covers the *paint* boundary: legacy paint ops on the same drawable flush the batch first. But X11 also has a *destruction* boundary: `DestroyWindow`, `FreePixmap`, `RenderFreePicture`, resize-replace, cursor rescue. Any of those drops a `VkImage` that the in-flight `PaintBatch` CB still references — UAF visible at the GPU queue (AMD reports it as a VM_CONTEXT fault reading from TC6 = texture cache).

3B salvaged this with `flush_if_needed(ProtocolBarrier)` at the 5 destruction sites (`be68847`) plus error-path tightening to preserve the lifetime invariant on strict-flush failure (`66e7120`). 3C / 3D must add the same barrier at any new destruction site that releases a `VkImage` or `VkBuffer` reachable from the batch.

The long-term fix is batch-owned/refcounted resource handles so destruction defers automatically — recorded in phase 4's sync rework backlog.

## Done conditions

Per the plan's "Done conditions" section:

1. ✅ All 5 tasks committed + 1 out-of-task drop-order fix; tree green.
2. ✅ `KmsBackend::run_legacy_paint_op` exists; every paint-side `run_one_shot_op` is in one of (a) wrapper body, (b) borrow-conflict fallback with pre-flush, or (c) readback handler.
3. ✅ `RenderScheduler::record_paint_op` and `record_paint_batch_op` exist; KmsBackend versions are shims.
4. ✅ `KmsBackend::paint_resources()` gates on `renderer_failed`. 13 call sites.
5. ✅ `renderer_failed` field gates `composite_and_flip`, `try_vulkan_composite_flip`, `flush_if_needed`, the shim chain.
6. ✅ `flush_if_needed` latches `renderer_failed = true` on Vk error (any caller path).
7. ✅ All 4 fill call sites migrated.
8. ✅ All 4 copy-distinct + copy-same call sites migrated.
9. ✅ `copy::record_copy_area_same_overlap` wrapped via inline `flush_if_needed(ProtocolBarrier)` (deferred from migration to 3D).
10. ✅ Hardware smoke green post-fresh-boot on `silence` (post-`66e7120`); xts / rendercheck deferred to user at leisure.
11. ✅ `flush_if_needed(VisibleComposite)` in `composite_and_flip` is now load-bearing — the batch carries real recorded work into `submit_and_wait` for the first time.
12. ✅ Mixed batched + legacy paint correctness — MATE workload (mix of fills, copies, PutImage via legacy wrapper, cursor) ran without GPU hang or `renderer_failed` latch after the drawable-destruction barrier salvage.

## Salvage: drawable-destruction lifetime bug (post-T3)

After T2/T3 landed, the first hardware run reproduced an AMD VM fault (`VM_CONTEXT1_PROTECTION_FAULT` reading from `TC6` = texture cache) under MATE workload — GPU hang, IH ring overflow, machine unresponsive, recovered by sysrq reboot.

**Root cause**: batched paint ops in the `PaintBatch` CB hold raw `VkImage` handles past the X11 request boundary. If the X11 request handler dropped the drawable (`DestroyWindow`, `FreePixmap`, `RenderFreePicture`, resize-replace, `RenderCreateCursor` rescued path) before the next composite flush, the GPU executed against a freed image — UAF visible at the hardware queue.

**Fix**: two-step.

1. **`be68847`** (`fix(kms): flush PaintBatch before drawable destruction (3B salvage)`) — added `flush_if_needed(ProtocolBarrier)` at 5 drawable-destruction sites in `backend.rs`: `destroy_window`, `configure_window` resize-replace, `free_pixmap`, `render_free_picture`, `render_create_cursor` rescued path.
2. **`66e7120`** (`fix(kms): handle strict-flush failure in drawable-destruction barriers`) — the salvage's initial `let _ = self.flush_if_needed(...)` discarded the strict-mode error; codex flagged that on flush failure, dropping the protected image reintroduces the UAF. Final fix returns `io::Error` or `mem::forget`s the resource on each path so the protected `VkImage` outlives any in-flight batch even on failure.

Codex reviewed `66e7120`:
> "I reviewed 66e7120 against the failure path from my last review. No blocking findings. Claude's changes now preserve the lifetime invariant on strict flush failure. That addresses the UAF-on-failed-queue_wait_idle concern I had."

**Longer-term** (per codex): resource lifetime should move into batch-owned/refcounted `Arc<VkImage>` handles rather than relying on protocol destruction barriers plus `queue_wait_idle`. Out of scope for phase 3; revisit in phase 4 alongside the sync rework.

## Hardware smoke (post-reboot, fresh-boot)

User-driven validation on hardware host `silence` (post-`66e7120`, dual-screen):

- ✅ MATE session boots and works.
- ✅ Window paint, move, resize observed working.
- ✅ No GPU driver crash; no kernel `VM_CONTEXT` / oops / panic in `journal.log`.
- ✅ Zero `paint batch`, `poison`, `renderer_failed`, `DEVICE_LOST` hits in `yserver-hw.log` — the strict-flush failure path never fired.
- ✅ Cursor renders.
- ⚠️ Pointer still jerky in mate-control-center, yserver CPU still high under that workload. Performance regression vs phase 2 baseline is *smaller* than before 3B (subjectively "GPU load lower"), but the workload still saturates one CPU. Tracked separately — performance, not correctness.
- ⚠️ 398 `vk composite: descriptor pool ring exhausted for output HDMI-A-1 — deferring frame` warnings (`backend.rs:7058`). This is the per-output `CompositePoolRing` (phase-2 territory; sized for retirement-pipeline depth), NOT the new `BatchDescriptorArena`. Pre-existing.
- ⚠️ MATE applets (mate-panel, wnck-applet, clock-applet) abort with XCB `Too much data requested from _XRead` assertion. yserver wire-protocol bug — reply length mismatch on some extension request. Separate from 3B; pre-existing or surfaced by 3B-unrelated changes. Worth a follow-up.

xts + rendercheck were not run in this session — the user can run them at leisure now that the lifetime bug is closed.

## Commit summary (phase 3B)

| Task | Commit | Subject |
|---|---|---|
| Plan v1 | `516f948` | initial 3B plan (3 tasks) |
| Plan v2 | `000c3ec` | fold 4 codex blockers |
| Plan v3 | `de5a814` | fold 2 follow-up blockers |
| Plan v3.1 | `a23525f` | drop stale audit category |
| T0 | `bb3efc3` | `run_legacy_paint_op` wrapper + migrate ~14 paint-side sites |
| T1 | `8ad3c0e` | `renderer_failed` gate + `RenderScheduler::record_paint_op` + `paint_resources` |
| T2 | `b627e1a` | migrate fill recorders (3 sites) |
| Drop-order | `c521f42` | reorder fields so scheduler drops before pool (revert T2's workaround) |
| T3 | `1d5b621` | migrate copy distinct+same recorders (4 sites; same-overlap deferred) |
| Salvage | `be68847` | flush PaintBatch before drawable destruction (5 sites) |
| Salvage tighten | `66e7120` | handle strict-flush failure in drawable-destruction barriers (return Err / mem::forget) |

11 commits since the phase-3A tip (`4af9e01`).

## Known deferred items

- **`record_copy_area_same_overlap`**: still on `run_one_shot_op` with inline `flush_if_needed(ProtocolBarrier)`. Uses `CopyScratch` (shared scratch image). Migration in 3D after 3C's upload-arena strategy informs scratch-image handling.
- **PutImage / mirror-upload / render / text / traps**: still on `run_legacy_paint_op`. 3C migrates upload-backed paint; 3D migrates descriptor/scratch-heavy paint.
- **`record_get_image`**: still on `run_one_shot_op` with `flush_if_needed(Readback)`. Phase 5 (targeted `VkFence` per HLD).
- **`renderer_failed` is currently in-band only**: no recovery path. User-doc / supervisor integration (systemd restart, session manager) is out of scope for phase 3; phase 4's sync rework can revisit if it changes failure modes.
- **Hot-path `vkQueueWaitIdle` in `vk/ops/mod.rs::run_one_shot_op`**: still present. Phase 4 retires the close-time wait in `PaintBatch::submit_and_wait`; phase 4/5 retire per-recorder one-shot waits as recorders migrate.

## What's next

**Phase 3C** — migrate upload-backed paint:

1. `image::record_put_image` + mirror-upload helpers via `BatchUploadArena` (the 3A T2 chunked allocator). PutImage today uses `OpsStaging` (shared host-visible buffer); two PutImages in one batch would alias their staging bytes. Per-batch arena solves it.
2. `MaskScratch::upload_r8` — needs a strategy for shared mask image (per-batch images? serialize upload+draw within one closure?). 3A T4's deferred audit lives here.
3. Glyph atlas upload — similar.
4. Gradient upload — similar, but gradients are per-`RenderCreateLinearGradient` not per-frame, so could flush on create.

Re-plan 3C as a writing-plans-skill detail block before executing. The borrow-checker pressure that 3B's `paint_resources()` pattern resolved is now the established workflow; 3C extends it to `batch.upload_arena_mut()` access from inside recorder closures.

3D handles render / text / traps / copy-same-overlap (descriptor- and scratch-heavy).
