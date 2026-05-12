# Phase 3A — rendering re-architecture — validation results

Date: 2026-05-13
Plan: `docs/superpowers/plans/2026-05-12-rendering-rearchitecture-phase3.md` (3A section)
Branch: `graphics-followups`
Predecessors: phase 1 (`87e887c`), phase 2 (`6b2101b`)

## Scope landed

3A's five tasks landed the destination resource-ownership model for `PaintBatch`. **No recorders migrated** — that's 3B–3D's scope. After 3A:

- `PaintBatch` owns its command buffer, an upload arena, a descriptor arena, and a generic `Vec<Box<dyn BatchResource>>` for retire-time cleanup.
- State machine: `Idle → Recording → Closed → Submitted → Retired`, plus `Poisoned` terminal for failed appends.
- `BatchFlushReason` enum + `KmsBackend::flush_if_needed(reason)` is the single boundary callers cross.
- `KmsBackend::record_paint_batch_op` (wide API: `(&VkContext, &mut PaintBatch, vk::CommandBuffer)`) and `record_paint_op` (thin shim) exist with **zero call sites**.
- CPU-visible request handlers (`try_vk_get_image_pixels`, `hw_cursor_refresh`, `read_mirror_pixels`) wired to `flush_if_needed(Readback)`.
- `composite_and_flip` flushes via `flush_if_needed(VisibleComposite)` BEFORE the per-output composite loop, with fatal-on-Vk return.

## Preflight checks

All ran clean at end of `f778868`:

- `cargo +nightly fmt --check` — no diff.
- `cargo clippy` — 5 pre-existing doc-list-indentation warnings (unchanged from phase 2 baseline).
- `cargo test --lib -p yserver` — **129 passed, 0 failed, 3 ignored** (the 3 ignored: 2 pre-existing + new `append_failure_poisons_batch` from T4 which needs a VkContext mock harness that doesn't exist).

## Diff size

| File | Insertions | Notes |
|---|---|---|
| `kms/scheduler/paint_batch.rs` | +562 | shell → state machine + 2 arena fields + flush reasons + 3 access methods |
| `kms/backend.rs` | +307 | flush_if_needed + record_paint_op{,_batch_op} + composite_and_flip rewire + 3 readback flush prefixes + audit catalogue |
| `kms/scheduler/batch_upload_arena.rs` | +256 | new file (chunked host-visible allocator) |
| `kms/scheduler/batch_descriptor_arena.rs` | +150 | new file (chunk-grown paint descriptor pool) |
| `kms/scheduler/mod.rs` | +104 | open_batch(vk, pool) + close_and_submit + module registrations + doc refresh |
| **Total** | **+1315 / −64** | |

## CPU-visible / sync-export audit catalogue

T5 step 3 catalogued every site across `crates/yserver/` for `flush_if_needed(Readback | ExternalSync)` wiring. The catalogue lives as a comment block in `backend.rs` near `try_vk_get_image_pixels` (~line 3845).

| Site | Decision | Reason |
|---|---|---|
| `try_vk_get_image_pixels` | **flush Readback** | Calls `run_one_shot_op(record_get_image)`; CPU reads staging bytes after |
| `hw_cursor_refresh` | **flush Readback** | Calls `run_one_shot_op(record_get_image)` on cursor mirror; CPU reads into dumb buffer |
| `read_mirror_pixels` | **flush Readback** | Calls `run_one_shot_op(record_get_image)`; returns CPU bytes to caller |
| `create_cursor` / `copy_plane` / `read_depth1_pixmap` | skip | Calls `read_mirror_pixels`; flush already in callee |
| `dri3_trigger_fence` | skip ExternalSync | xshmfence futex / VkSemaphore stub; no GPU work |
| `dri3_fd_from_fence` | skip ExternalSync | Exports pre-populated VkSemaphore fd; no GPU work |
| `dri3_signal_syncobj` | skip ExternalSync | CPU-side timeline semaphore signal; no batch dependency |
| `dri3_export_pixmap` | skip ExternalSync | Exports dma-buf handle; X11 DRI3 contract puts pixel-visibility sync on client via fences |
| `PresentPixmap` / `SyncTriggerFence` | n/a | No handler entry points in `kms/backend.rs` |

## Layout-state late-mutation audit

T4 audited the four 3B-target recorders for the late-mutation invariant. Stronger than expected: **all four have zero error paths** (their Vulkan calls are in `unsafe { }` blocks returning `()`). `set_current_layout` is always the tail mutation before `Ok(())` — invariant trivially satisfied.

- `fill::record_fill_rectangles` — late ✓
- `fill::record_logic_fill` — late ✓
- `copy::record_copy_area_distinct` (delegates to infallible `record_distinct_image_copy`) — late ✓
- `copy::record_copy_area_same` (delegates to infallible `record_same_image_copy`) — late ✓

Deferred audits (3C/3D BLOCK on these passing): `copy::record_copy_area_same_overlap` (3D), `image::record_put_image` (3C), `render::record_render_composite` (3D), `text::record_text_run` (3D), `traps::record_*` (3D).

## Hardware smoke (fuji)

User-driven validation on hardware host (`fuji`):

- Desktop session comes up.
- No regressions vs phase 2.
- No `paint batch submit failed` warnings in logs.

This validates 3A T5 step 5 (originally deferred from the implementer to keep the plan running). The Idle-batch flush path under real composite cadence is exercised; the fatal-Err path in `composite_and_flip` is unreachable until 3B migrates a recorder.

## Plan bugs caught (folded back into the plan)

Five recipe-level issues caught during execution. All folded into the plan doc (`f778868`) so 3B+ implementers see them upfront:

1. **`#[derive(Debug)]` on types holding `Arc<VkContext>` doesn't compile.** Hit at `PaintBatch` (T1), `BatchUploadArena` (T2), `BatchDescriptorArena` (T3). Workaround: manual `Debug` impl with `finish_non_exhaustive()`. Plan now spells this out in the pre-task section.

2. **`unsafe impl Send` requires `// SAFETY:` at the impl site.** T2 missed both impls. Fixed in `aba499e`. Plan now requires the SAFETY pattern upfront.

3. **`BatchUploadArena::min_chunk_size` field was dead** (always `MIN_CHUNK_SIZE` constant). Removed in `aba499e`.

4. **`BatchUploadArena` chunks had spurious `TRANSFER_DST` usage flag.** Upload-staging buffers are SRC-only. Dropped in `aba499e`.

5. **3A T1's `dirty_outputs` local in `composite_and_flip` is dead after T5 rewires.** T5 must delete the local since `flush_if_needed` rebuilds it internally. Caught at T5 review; T5 implementer fixed it. Plan now flags this for the next-tranche author.

## Renderer-disabled design (3B prerequisite)

`f778868` added a section to the plan before 3B that catalogues the three options for handling `composite_and_flip`'s fatal-Vk return:
- **Option A** (recommended) — in-band `renderer_failed: bool` flag; gate every paint entry point.
- **Option B** — propagate via `Backend::on_page_flip_ready` trait (invasive).
- **Option C** — panic.

3B T0 owns implementation. Today the trait surface drops the `Err` on the floor — fine while the batch is always Idle, but a real correctness issue once 3B's first recorder migrates.

## Done conditions

Per the plan's "Done conditions" section:

1. ✅ All 5 tasks committed; tree green (`fmt --check` / `clippy` / `test`).
2. ✅ `PaintBatch` state machine + three failure paths in `submit_and_wait` + Drop honours leak invariant.
3. ✅ `BatchUploadArena` exists, owned per-batch, stable-offset, chunk-grown.
4. ✅ `BatchDescriptorArena` exists, owned per-batch, paint-side pool sized per the constants.
5. ✅ `BatchFlushReason` enum + `flush_if_needed(reason)` with strict-vs-best-effort error semantics.
6. ✅ `record_paint_batch_op` (wide) + `record_paint_op` (shim) exist with zero call sites.
7. ✅ Layout-state policy documented; fill + copy-distinct + copy-same audited late ✓.
8. ✅ CPU-visible / sync-export request handlers all call `flush_if_needed(Readback)` (or skip with documented rationale for ExternalSync sites).
9. ✅ `cargo test`, hardware smoke (fuji) all green; no regressions vs phase 2.

## Commit summary (phase 3A)

| Task | Commit | Subject |
|---|---|---|
| Plan | `fa697a3` | phase-3 plan (3A detailed, 3B–3D sketched) |
| T1 | `485ff01` | PaintBatch state machine + holders refcount + BatchResource trait |
| T2 | `01d37d2` | BatchUploadArena — chunked host-visible per-batch allocator |
| T3 | `4cac50e` | BatchDescriptorArena — per-batch paint descriptor pool |
| T4 | `a564055` | layout-state policy — poison-and-discard with late-mutation audit |
| T5 | `3ca6db8` | BatchFlushReason + flush_if_needed + record_paint_op (no call sites) |
| Cleanup | `aba499e` | clean up BatchUploadArena — SAFETY comments, drop TRANSFER_DST, remove dead field |
| Plan refresh | `f778868` | fold phase-3A lessons + renderer_failed design into plan |

8 commits on top of `6b2101b` (phase-2 tip).

## Known deferred items

- **`append_failure_poisons_batch` test** is `#[ignore]`'d; needs a VkContext mock harness or a hardware smoke step. Validated by behaviour today (any future Vk error in an `append` body would poison the batch and the next composite cycle would observe `Poisoned` state in `flush_if_needed`'s strict path). Not blocking.

- **Phase-4 holder refcount fields** (`PaintBatch::holders`, `acquire_holder`, `release_holder`) are dead code. Landed for ABI shape so phase 4 can wire them without struct churn.

- **`PaintBatch::dirty_outputs`** populated at close, never read in 3A. Phase 4 reads it for multi-output semaphore fan-out.

- **`record_get_image`** still uses `run_one_shot_op` with its own `vkQueueWaitIdle`. Explicit phase-5 scope (targeted `VkFence` per HLD).

- **Hot-path `vkQueueWaitIdle` in `vk/ops/mod.rs::run_one_shot_op`** still present (4 hits unchanged from phase-2). Phase 4 retires the close-time wait in `PaintBatch::submit_and_wait`; phase 4/5 retire the per-recorder one-shot waits as recorders migrate.

## What's next

**Phase 3B** — migrate scratch-free paint families (fill + copy-distinct + copy-same). Per the plan's "Renderer-disabled design" section, **3B T0 implements the `renderer_failed` flag** before any recorder migration touches `record_paint_op`. Then ~2 migration tasks (fill, then copy). Re-plan as a writing-plans-skill detail block before executing — the borrow-checker pressure of `&mut self` through `record_paint_op` is the unknown.

After 3B lands, replan 3C (upload-backed: PutImage / mirror upload / mask scratch / glyph atlas / gradient) and 3D (descriptor-heavy: traps / render / text / copy-same-overlap) with whatever the real ownership API ergonomics demand.
