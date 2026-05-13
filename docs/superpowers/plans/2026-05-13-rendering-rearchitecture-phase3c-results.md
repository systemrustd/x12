# Phase 3C — rendering re-architecture — validation results

Date: 2026-05-13
Plan: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3c.md` (v2 — codex's three non-blocking cleanups folded in before execution)
Branch: `graphics-followups`
Predecessor: phase 3B results (`82558a5`)

## Scope landed

Three tasks (T1–T3) plus the validation/results task (T4). 3C migrated the two upload-backed paint recorders that were still on shared `OpsStaging` — `try_vk_put_image` and `upload_bgra_to_mirror` — to `record_paint_batch_op` + the per-batch `BatchUploadArena` (3A T2 allocator). T3 added a cheap protocol-barrier flush at gradient creation and refreshed the `run_legacy_paint_op` audit catalogue.

Mask scratch upload, glyph-atlas upload, `text::record_text_run` (2 sites), `render::record_render_composite` (2 sites), and `copy::record_copy_area_same_overlap` are explicitly deferred to 3D — descriptor- / scratch-heavy paths need a separate strategy.

After 3C:

- **`try_vk_put_image`** (T1, `d4e7d3d`): no longer claims `ops_staging`. Instead, the recorder runs inside `self.scheduler.record_paint_batch_op(...)` and allocates a per-batch staging slice via `batch.upload_arena_mut()`, then calls `image::record_put_image`. Two PutImages within one batch can now coexist without aliasing.
- **`upload_bgra_to_mirror`** (T2, `9c3b6e2`): same migration shape — `mirror::record_upload_rect` runs inside `record_paint_batch_op` against an arena-backed staging slice.
- **Arena-OOM handling** (T1 fix `e53b8c9`, T2 fix `e1686b5`): both call sites use the "outer-flag pattern" so that an arena allocation failure inside the recorder closure leaves the batch untouched and falls back to the legacy pixman/`run_one_shot_op` path. The closure returns `Ok(())` and signals OOM via an outer `bool`, so `record_paint_batch_op` does not poison the batch.
- **Gradient-create protocol barrier** (T3, `f24eb72`): both gradient-create entry points (`render_create_linear_gradient`, `render_create_radial_gradient` — there is no conic handler in this codebase) call `flush_if_needed(ProtocolBarrier)` before `GradientPicture::new_*`. Gradients are created per-`RenderCreate*Gradient` (not per-frame), so the flush is cheap and rare. The `run_legacy_paint_op` audit catalogue (`backend.rs:~1697`) was updated to mark T1/T2 migrated and note the 3D-deferred recorders.
- **`OpsStaging` retained**: still used by the three readback handlers (`try_vk_get_image_pixels`, `hw_cursor_refresh`, `read_mirror_pixels`). No paint-side code references it anymore.

## Preflight checks

End of 3C (HEAD = `f24eb72`):

- `cargo +nightly fmt --check` — no diff.
- `cargo clippy -p yserver` — 5 pre-existing `doc_lazy_continuation` warnings on `yserver` lib (unchanged from 3B baseline). No new warnings. (`yserver-core` separately reports 2 pre-existing warnings, unchanged.)
- `cargo test --workspace` — all green:
  - `yserver` unit tests: 133 passed, 0 failed, 3 ignored.
  - `yserver-core`: 284 passed.
  - `yserver-protocol`: 208 passed.
  - `fixture_smoke`: 2 passed, 1 ignored.
  - `alpha_invariant`: 17 ignored (need live Vulkan ICD).
  - All other crates green; doc-tests green.

## Cutover greps

Semantic, not numeric — the count varies as 3D progresses; what matters is the SITES.

```
$ rg -n 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs
1744:        crate::kms::vk::ops::run_one_shot_op(&vk_arc, pool_handle, record)
2114:                        match crate::kms::vk::ops::run_one_shot_op(vkctx, pool_handle, |vk, cb| {
2407:        if let Err(e) = run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
2757:        if let Err(e) = run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
3325:                return match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
4127:    //     FLUSH Readback — calls run_one_shot_op(record_get_image). CPU reads
4134:    //     FLUSH Readback — calls run_one_shot_op(record_get_image) to read
4141:    //     FLUSH Readback — calls run_one_shot_op(record_get_image) and then
4336:            match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
4493:        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
5094:        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
5413:        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
6097:        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
7859:        run_one_shot_op(vk, pool_handle, |vk, cb| {
```

Site decisions:

- `1744` — `run_legacy_paint_op` wrapper body itself.
- `2114` — `hw_cursor_refresh` readback handler.
- `2407` — `read_mirror_pixels` readback handler.
- `2757` — `read_mirror_pixels` second branch.
- `3325` — `try_vk_copy_area` same-overlap arm (3D-deferred borrow-conflict fallback with inline `flush_if_needed(ProtocolBarrier)`).
- `4127/4134/4141` — audit-catalogue *comments* describing the three readback flushes (not call sites; documentation).
- `4336/4493/5094/5413/6097` — 3D-deferred borrow-conflict fallbacks (text-run, render-composite, traps; each carries a `// borrow-conflict fallback` comment and pre-flush).
- `7859` — `dump_scanout_one` diagnostic dump (not paint-side).

ZERO hits inside `try_vk_put_image` (lines 3812+) or `upload_bgra_to_mirror` (lines 2603+). Confirmed via per-function scan.

```
$ rg -n 'ops_staging' crates/yserver/src/kms/backend.rs
813:    pub(crate) ops_staging: Option<crate::kms::vk::ops::OpsStaging>,
1424:            ops_staging: None,
1496:        let ops_staging = crate::kms::vk::ops::OpsStaging::new(Arc::clone(&vk), 1024 * 1024)
1505:        backend.ops_staging = Some(ops_staging);
2002:        let ops_staging =
2208:            ops_staging,
2351:        // GPU readback into ops_staging. Same shape as
2360:        if self.ops_staging.is_none() {
2376:            .ops_staging
2399:        let staging_buffer = self.ops_staging.as_ref().expect("present").buffer();
2419:            let staging_ptr = self.ops_staging.as_ref().expect("present").mapped_ptr();
2688:        self.ops_staging.as_ref()?;
2722:            .ops_staging
2747:        let staging_buffer = self.ops_staging.as_ref().expect("present").buffer();
2770:            .ops_staging.as_ref().expect("present").mapped_ptr();
4225:        if self.ops_staging.is_none() {
4293:                .ops_staging
4335:            .ops_staging.as_ref().expect("checked above").buffer();
4356:        .ops_staging.as_ref().unwrap().mapped_ptr() as *const u8;
```

Site decisions:

- `813 / 1424 / 1496 / 1505 / 2002 / 2208` — struct field, initializer, two `KmsBackend::new` plumbing sites.
- `2351 / 2360 / 2376 / 2399 / 2419` — `hw_cursor_refresh` readback handler.
- `2688 / 2722 / 2747 / 2770` — `read_mirror_pixels` readback handler.
- `4225 / 4293 / 4335 / 4356` — `try_vk_get_image_pixels` readback handler.

ZERO hits inside `try_vk_put_image` or `upload_bgra_to_mirror`.

```
$ rg -n 'record_paint_batch_op' crates/yserver/src/kms/backend.rs
1612:    /// `self.scheduler.record_paint_op` / `record_paint_batch_op`,
1640:    /// to the scheduler-level `record_paint_batch_op`. Useful when
1643:    /// must use `paint_resources()` + `self.scheduler.record_paint_batch_op(...)`
1646:    pub fn record_paint_batch_op<F>(&mut self, record: F) -> Result<(), ash::vk::Result>
1661:            .record_paint_batch_op(vk_arc, pool_handle, record)
1672:        self.record_paint_batch_op(|vk, _batch, cb| record(vk, cb))
1700:    ///   upload_bgra_to_mirror:            mirror.record_upload_rect          — migrated 3C T2 (record_paint_batch_op + arena)
1709:    ///   try_vk_put_image:                 image::record_put_image             — migrated 3C T1 (record_paint_batch_op + arena)
2624:            .record_paint_batch_op(vk_arc, pool_handle, |_vk, batch, cb| {
3963:            .record_paint_batch_op(vk_arc, pool_handle, |vk, batch, cb| {
```

Call sites: `2624` is `upload_bgra_to_mirror` (T2), `3963` is `try_vk_put_image` (T1). Plus the shim and the doc-comment audit lines.

## Done conditions

Per the plan's done-conditions section:

1. ✅ `cargo +nightly fmt --check` clean.
2. ✅ `cargo clippy -p yserver` — 5 pre-existing `doc_lazy_continuation` warnings; no new warnings.
3. ✅ `cargo test --workspace` green (yserver 133 / yserver-core 284 / yserver-protocol 208; all other crates green).
4. ✅ `try_vk_put_image` runs its recorder inside `record_paint_batch_op` with `batch.upload_arena_mut()`; no inline `flush_if_needed(ProtocolBarrier)` fallback at the top of the recorder remains.
5. ✅ `upload_bgra_to_mirror` similarly migrated.
6. ✅ `OpsStaging` is used only by the three readback handlers (`try_vk_get_image_pixels`, `hw_cursor_refresh`, `read_mirror_pixels`) and the struct field/initializer plumbing.
7. ✅ Gradient-create entry points call `flush_if_needed(ProtocolBarrier)` before `gradient::build` (T3).
8. ✅ `run_legacy_paint_op` audit catalogue updated to reflect T1/T2 migrations and 3D-deferred recorders.
9. ⏸ Hardware smoke — see below.

## Hardware smoke

**Deferred to user.** The migrations are correctness-preserving by construction (same recorders called inside batch CB instead of one-shot). xts5 / rendercheck / MATE smoke can be run at user discretion. No GPU faults expected; phase-3B drawable-destruction barriers continue to cover PutImage targets (windows/pixmaps).

## Plan bugs caught (folded back into plan)

Two execution-time bugs caught at code-quality review and fixed before the next task started:

- **T1 arena-OOM batch poisoning** — first implementation returned `Err` from the recorder closure on arena allocation failure, which made `record_paint_batch_op` poison the entire `PaintBatch` for that frame, taking down everything previously recorded too. Caught at code-quality review; fix at commit `e53b8c9` introduced the "outer-flag pattern" — the closure returns `Ok(())` even on arena-OOM, signals failure via an outer `bool`, and the caller falls back to legacy / pixman. The pattern was then applied to T2 too for consistency (commit `e1686b5`).
- **T2 plan note claimed cursor-create paths are Idle in practice** — code-quality reviewer correctly noted that `create_glyph_cursor` rasterizes from FreeType CPU-side and does NOT pre-flush, so the batch could be in `Recording` state if a 3B fill/copy ran earlier in the same protocol read. The same outer-flag fix at `e1686b5` covers this — `upload_bgra_to_mirror` cannot assume Idle state.

Both lessons feed memory note material on "atomic-switch tasks need intermediate-state audits" — the closure-return contract of `record_paint_batch_op` is a sharp edge that needs an explicit not-our-fault path.

## Commit summary

| Task | Commit | Subject |
|---|---|---|
| Plan v1 | `a15c024` | initial 3C plan |
| Plan v2 | `d9fa287` | fold codex's three non-blocking cleanups |
| T1 | `d4e7d3d` | migrate try_vk_put_image |
| T1 fix | `e53b8c9` | don't poison batch on PutImage arena-OOM |
| T2 | `9c3b6e2` | migrate upload_bgra_to_mirror |
| T2 fix | `e1686b5` | don't poison batch on upload_bgra arena-OOM |
| T3 | `f24eb72` | gradient-create protocol barrier + audit update |

Total: 7 commits since the 3B-results tip (`82558a5`).

## Known deferred items (3D scope)

- **`text::record_text_run`** (2 sites: `try_vk_text_run`, `try_vk_render_composite_glyphs`) — descriptor-heavy + uses glyph atlas; needs glyph-atlas incremental-upload strategy.
- **`render::record_render_composite`** (2 sites: `try_vk_render_traps`, `try_vk_render_composite`) — descriptor-heavy + uses `MaskScratch`.
- **`copy::record_copy_area_same_overlap`** (`try_vk_copy_area` same-overlap arm) — uses `CopyScratch`; migration awaits 3D's scratch-image strategy.
- **`MaskScratch::upload_r8`** — shared mask image; needs per-batch image strategy or in-closure serialize.
- **Glyph atlas incremental upload** — similar.
- **`record_get_image`** — still `phase 5` scope (targeted `VkFence` per HLD); out of 3C/3D.

## What's next

**Phase 3D** — migrate descriptor- / scratch-heavy paint (text-run, render-composite, copy-same-overlap, mask/glyph upload). Re-plan via the writing-plans skill before executing; the `BatchUploadArena` access pattern established by 3C ports straight over, but scratch-image lifetime needs its own design pass.

Note that `BatchUploadArena` is now load-bearing under MATE-class workloads (PutImage and mirror upload both feed it). Phase 5 perf retirement of one-shot waits can revisit if the arena's chunk-growth pattern is right-sized for steady-state workloads — at minimum a per-batch peak/`p99` chunk-count counter would catch fragmentation regressions.
