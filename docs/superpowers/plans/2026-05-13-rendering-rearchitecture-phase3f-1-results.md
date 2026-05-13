# Phase 3F-1 — rendering re-architecture — render-composite migration — results

Date: 2026-05-13
Plan: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-1.md`
Branch: `graphics-followups`
Predecessor: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3e-results.md`

## Scope landed

Phase 3F-1 migrated the RENDER `Composite` recorder — one call site, `try_vk_render_composite` — off the legacy `flush_if_needed(ProtocolBarrier) + run_one_shot_op` borrow-conflict fallback onto `paint_resources() + scheduler.record_paint_batch_op(...)`, with a per-batch descriptor-arena allocation for the source/mask/dst-readback descriptor set. The unconditional pre-record `ProtocolBarrier` flush that fired on every RENDER `Composite` is gone; composite-heavy frames now pack into the open `PaintBatch` alongside fill / copy / put_image / text-run.

- **T1 (`afc18f6`)**: `DstReadback::needs_grow(format, w, h) -> bool` accessor. Returns `true` for an unallocated slot, `width > extent.width || height > extent.height` for an allocated slot of the requested format, and `false` for a slot whose format does not match the request. Mirrors `CopyScratch::needs_grow` from 3D, used by T3 as the guard for the resize-only pre-flush.
- **T2 (`3fe108b`)**: `RenderPipelineCache::allocate_descriptor_for_views_into(&self, arena: &mut BatchDescriptorArena, src, mask, dst) -> Result<vk::DescriptorSet, vk::Result>`. The arena-backed counterpart to the legacy shared-pool `allocate_descriptor_for_views`. The set lives until the owning batch retires, removing the cross-op trampling hazard that drove the per-`Composite` `ProtocolBarrier` flush.
- **T3 (`fade626`)**: `try_vk_render_composite` migrated to `record_paint_batch_op`. Five disjoint field borrows captured by the closure (`dst_mirror`, `solid_src_image`, `solid_mask_image`, `dst_readback`, `render_cache`); descriptor set allocated inside the closure via `render_cache.allocate_descriptor_for_views_into(batch.descriptor_arena_mut(), …)`. `dst_readback` grow is gated on a small pre-record `flush_if_needed(BatchFlushReason::ProtocolBarrier)` controlled by `DstReadback::needs_grow` — only fires on first sight of a dst format or on actual extent growth; steady-state path skips the flush. Audit catalogue updated in the same commit.
- **T3 comment fix-up (`c4a4965`)**: code-quality review feedback — the pre-flush comment originally said the gate fires "on resize"; the actual gate also fires on first-allocation (unmapped slot or format-change). Reworded to make both cases explicit and to note that the first-allocation case is harmless (no old image to dangle) while resize is the real hazard.

Deliberately retained for 3F-2:

- `try_vk_render_traps_or_tris` — still on the legacy `flush_if_needed(ProtocolBarrier) + run_one_shot_op` + shared-pool descriptor path. Migration requires per-batch `MaskScratch` + `MaskScratch::needs_grow` + arena-staging of `upload_r8`, plus deciding whether traps and tris share a recorder or each gets one.
- `RenderPipelineCache::allocate_descriptor_for_views` (the legacy shared-pool variant), `RenderPipelineCache::reset_descriptors`, and the `RenderPipelineCache.descriptor_pool` field — kept because `try_vk_render_traps_or_tris` still calls them. 3F-2 will delete all three.

## Preflight checks

End of 3F-1 (HEAD = `c4a4965`):

- `cargo +nightly fmt --check` — clean (no diff).
- `cargo clippy -p yserver` — 5 pre-existing `doc_lazy_continuation` warnings (`backend.rs:33`, `backend.rs:74`, `vk/pipeline.rs:104`, and 2 sibling sites). No new warnings.
- `cargo test --workspace`:
  - `yserver` lib: **138 passed, 0 failed, 3 ignored**.
  - `yserver` binary integration (`ynest`): 9 passed.
  - `yserver-core`: **284 passed**.
  - `yserver-protocol`: **208 passed**.
  - `fixture_smoke`: 2 passed, 1 ignored.
  - Other test binaries: green.

## Cutover greps

Captured semantically (line numbers are informational and will drift; the load-bearing claim is the SITE list).

```
$ rg -n 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs
1755:        crate::kms::vk::ops::run_one_shot_op(&vk_arc, pool_handle, record)
2125:                        match crate::kms::vk::ops::run_one_shot_op(vkctx, pool_handle, |vk, cb| {
2419:        if let Err(e) = run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
2769:        if let Err(e) = run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
4164:    //     FLUSH Readback — calls run_one_shot_op(record_get_image). CPU reads
4171:    //     FLUSH Readback — calls run_one_shot_op(record_get_image) to read
4178:    //     FLUSH Readback — calls run_one_shot_op(record_get_image) and then
4373:            match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
5125:        match run_one_shot_op(&vk_arc, pool_handle, |vk, cb| {
7929:        run_one_shot_op(vk, pool_handle, |vk, cb| {
```

ZERO hits inside `try_vk_render_composite` (5580..6176). Remaining production sites:
- `1755` — `run_legacy_paint_op` body itself (legacy entry point).
- `2125` — `open_with_commit` constructor (one-time `record_solid_color_clear` init).
- `2419` — `hw_cursor_refresh` (readback handler).
- `2769` — `read_mirror_pixels` (readback handler).
- `4373` — `try_vk_get_image_pixels` (readback handler).
- `5125` — `try_vk_render_traps_or_tris` (3F-2-deferred).
- `7929` — `dump_scanout_one` (one-shot readback).

(Lines `4164/4171/4178` are doc-comments, not call sites.) Compared to end-of-3E (8 production sites), the count drops by ONE — `try_vk_render_composite`'s site is gone.

```
$ rg -n 'flush_if_needed\(BatchFlushReason::ProtocolBarrier' crates/yserver/src/kms/backend.rs
1742:        if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
3313:                    if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
5102:            if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
6017:            if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
```

Sites:
- `1742` — `run_legacy_paint_op` body (legacy entry point).
- `3313` — `try_vk_copy_area` same-overlap resize-only pre-flush (3D-installed, gated on `CopyScratch::needs_grow`).
- `5102` — `try_vk_render_traps_or_tris` unconditional pre-record flush (3F-2-deferred).
- `6017` — `try_vk_render_composite` resize-only pre-flush (NEW in 3F-1, gated on `DstReadback::needs_grow`).

The OLD unconditional pre-record flush inside `try_vk_render_composite` (formerly at ~line 6049, captured in the 3E results doc as `6050`) is gone. The drawable-destruction and gradient-create call sites use a multi-line `flush_if_needed(...)` form that does not match the strict-prefix grep — they still live at `backend.rs:8971`, `9150`, `9440`, `10915`, `11348`, `11423`, `11508` (unchanged).

```
$ rg -n 'record_paint_batch_op|record_paint_op' crates/yserver/src/kms/backend.rs | wc -l
47
```

Raw count includes doc comments, definitions, and one test. Production scheduler-level call sites (i.e. `.record_paint_batch_op(…)` / `.record_paint_op(…)` with a leading dot, excluding the two wrapper definitions at `1672/1683`): **14**, well above the "≥ 10" floor. The new 3F-1 site sits at `backend.rs:6127` inside the migrated `try_vk_render_composite` closure. One more production call site than end-of-3E.

```
$ rg -n 'allocate_descriptor_for_views\b' crates/yserver/src/kms/backend.rs
5025:            .allocate_descriptor_for_views(src_view, mask_view, dst_readback_view)
```

ONE hit — inside `try_vk_render_traps_or_tris` (the legacy shared-pool path that 3F-2 will remove). ZERO inside `try_vk_render_composite`. (The plan's expected output says "ZERO hits inside backend.rs" but its parenthetical clarifies "traps still uses it"; the code matches the parenthetical — see "Plan bugs caught" below.)

```
$ rg -n 'allocate_descriptor_for_views_into' crates/yserver/src/kms/
crates/yserver/src/kms/backend.rs:6128:                let descriptor_set = render_cache.allocate_descriptor_for_views_into(
crates/yserver/src/kms/vk/render_pipeline.rs:518:    pub fn allocate_descriptor_for_views_into(
```

1 definition (`render_pipeline.rs:518`) + 1 call site (`backend.rs:6128` inside the migrated `try_vk_render_composite` closure). Matches the plan.

```
$ rg -n 'reset_descriptors\(' crates/yserver/src/kms/backend.rs
4885:            .reset_descriptors()
```

ONE hit inside `try_vk_render_traps_or_tris` (legacy path; 3F-2 will remove). ZERO inside `try_vk_render_composite`. Matches the plan.

```
$ rg -n 'needs_grow' crates/yserver/src/kms/
crates/yserver/src/kms/backend.rs:3310:                    .is_some_and(|s| s.needs_grow(u32::from(width), u32::from(height)));
crates/yserver/src/kms/backend.rs:6005:        // before the grow. needs_grow fires on first use of a
crates/yserver/src/kms/backend.rs:6014:                .is_some_and(|r| r.needs_grow(dst_format, dst_extent.width, dst_extent.height));
crates/yserver/src/kms/vk/dst_readback.rs:84:    pub fn needs_grow(&self, format: vk::Format, width: u32, height: u32) -> bool {
crates/yserver/src/kms/vk/copy_scratch.rs:70:    pub fn needs_grow(&self, width: u32, height: u32) -> bool {
```

2 definitions (`copy_scratch.rs:70` from 3D, `dst_readback.rs:84` from 3F-1 T1) + 2 production call sites in `backend.rs` (`3310` same-overlap from 3D, `6014` render-composite from 3F-1 T3). The third backend.rs hit at `6005` is the explanatory comment over the new pre-flush block. Matches the plan.

```
$ rg -n '3D-deferred: render-composite' crates/yserver/src/kms/backend.rs
(no hits)
```

ZERO. The stale comment that lived inside the removed pre-flush block is gone.

```
$ rg -n '3D-deferred: render-traps' crates/yserver/src/kms/backend.rs
5096:        // 3D-deferred: render-traps needs per-batch MaskScratch + descriptor
```

ONE hit inside `try_vk_render_traps_or_tris` — 3F-2 scope.

## Done conditions

Per the plan's 14 Done conditions (~lines 989–1006):

1. ✅ `cargo +nightly fmt --check` clean.
2. ✅ `cargo clippy -p yserver` produces 5 pre-existing `doc_lazy_continuation` warnings; no new warnings.
3. ✅ `cargo test --workspace` green; `yserver` lib **138 passed**.
4. ✅ `DstReadback::needs_grow(format, w, h)` exists at `crates/yserver/src/kms/vk/dst_readback.rs:84`. Returns `true` for unallocated slot, `width > extent.width || height > extent.height` for allocated slot, and returns `false` for unknown formats (i.e. formats other than `B8G8R8A8_UNORM` / `R8_UNORM`). (Verified by reading the implementation in T1 commit `afc18f6` and via T1's unit tests living next to the impl.)
5. ✅ `RenderPipelineCache::allocate_descriptor_for_views_into(&self, arena: &mut BatchDescriptorArena, src, mask, dst) -> Result<vk::DescriptorSet, vk::Result>` exists at `render_pipeline.rs:518` and is the ONLY descriptor-allocation path used by `try_vk_render_composite` (sole call site at `backend.rs:6128`, inside the closure).
6. ✅ Legacy `RenderPipelineCache::allocate_descriptor_for_views` and `reset_descriptors` are still present but have ZERO callers inside `try_vk_render_composite`. They still have one caller each inside `try_vk_render_traps_or_tris` (`backend.rs:5025` and `backend.rs:4885`). 3F-2 will remove both.
7. ✅ `try_vk_render_composite` uses `record_paint_batch_op` at `backend.rs:6127`:
   - The five disjoint field borrows (`dst_mirror`, `solid_src_image`, `solid_mask_image`, `dst_readback`, `render_cache`) are captured by direct field paths before the closure.
   - Descriptor set is allocated INSIDE the closure via `render_cache.allocate_descriptor_for_views_into(batch.descriptor_arena_mut(), …)` at `backend.rs:6128`.
8. ✅ The OLD `flush_if_needed(BatchFlushReason::ProtocolBarrier)` block (formerly at ~line 6049 / 6050) is GONE. The four remaining `flush_if_needed(BatchFlushReason::ProtocolBarrier` sites are catalogued under "Cutover greps" above.
9. ✅ The `3D-deferred: render-composite needs…` comment is GONE (grep returns no hits).
10. ✅ The pre-resize flush for `dst_readback` exists at `backend.rs:6017` and is gated on `DstReadback::needs_grow`. Steady-state (stable dst extent + same format) skips the flush. First-allocation also returns `true`, which is correct-but-harmless — there is no old image to dangle. T3 fix-up `c4a4965` reworded the comment to make the first-allocation case explicit.
11. ✅ Ordering invariant: the `dst_readback` pre-flush at `6017` runs BEFORE `scratch.ensure(…)` in the body, not merely before `record_paint_batch_op`. Verified by reading the migrated function.
12. ✅ The `run_legacy_paint_op` audit catalogue entry for `try_vk_render_composite` at `backend.rs:1724` reads `migrated 3F-1 (record_paint_batch_op + BatchDescriptorArena)`.
13. ✅ The `run_legacy_paint_op` audit catalogue entry for the traps row at `backend.rs:1722` reads `borrow-conflict fallback (3F-2)`. (Note: the label is `try_vk_render_traps (composite)`; the real function is `try_vk_render_traps_or_tris`. See "Plan bugs caught" below — this is plan-prescribed and not fixed in 3F-1.)
14. ⏳ **TBD — pending the user's hardware smoke**. See "Hardware smoke results" below.

## Hardware smoke results

Hardware smoke is user-owned (separate TTY on bare metal). The user runs `just yserver-mate-hw-release` or `just yserver-xfce-hw`, plus `just rendercheck-yserver`, and fills in the subsections below.

The rendercheck baseline is `docs/test-status.md` (2026-05-10 yserver/KMS, 600 s/test). composite + cacomposite are **INCOMPLETE at the 600 s budget** on yserver/KMS (rc=124) vs OK on ynest. The gate is "no regression in cases that did complete and partial-pass count is at least as high as the pre-3F-1 baseline" — NOT "must complete". If 3F-1's denser batching lets them finish that is a positive signal, but it is not a requirement.

### Host

TBD — fill in: hostname, GPU (e.g. AMD Polaris RX 580), session type (Wayland host: `dms + labwc`, or X host: lightdm/MATE), kernel version.

### Log file summary

TBD — fill in path to the captured `yserver-hw.log` and grep results for:
- `vk render_composite: record failed` — expect ZERO.
- `vk render_composite: pre-resize flush failed` — expect ZERO (rare path; if it fires, capture the context — it indicates a batch was Poisoned).
- `paint batch submit failed` — expect ZERO.
- `renderer_failed` / `DEVICE_LOST` — expect ZERO.
- `descriptor set` (from `VK_LAYER_KHRONOS_validation` if enabled in the smoke build) — expect ZERO.
- `journalctl -k --since "<yserver-start-time>"` for kernel GPU faults — expect ZERO.

### rendercheck delta vs `docs/test-status.md` baseline

TBD — fill in. Compare `target/rc-logs/rc-composite.log` and `target/rc-logs/rc-cacomposite.log` against the 2026-05-10 baseline. For composite/cacomposite specifically: report partial-pass count and whether the run still timed out at the 600 s budget (rc=124) or completed. For the regression-only categories (fill, dcoords, scoords, mcoords, tscoords, tmcoords, blend, gradients, repeat, triangles, bug7366) report pass counts and flag any delta vs the baseline as a stop-the-line.

### GTK smoke notes

TBD — fill in:
- mate-control-center hover (RenderComposite-heavy under hover gradients).
- gtk3-demo theme browser transitions.
- gedit / pluma text + selection drag (cairo selection-highlight overlay is a render-composite path).

### Anomalies

TBD — fill in any unexpected behaviour, including any caja regressions surfaced separately. (Pre-existing caja wheel-needs-view-switch issue at `39018c3` is not a 3F-1 regression; cross-reference if it appears.)

## Plan bugs caught (folded back into plan / fixed in-tree)

### 1. T3 grep #2 expected-output self-contradiction (plan-prescribed; not fixed in 3F-1)

The plan's "Cutover greps (post-3F-1)" section for `allocate_descriptor_for_views\b` says "ZERO" in the header but its parenthetical clarifies "traps still uses it". The actual code has ONE hit at `backend.rs:5025` inside `try_vk_render_traps_or_tris`. The code matches the parenthetical, which is the correct interpretation. The codex spec reviewer flagged this self-contradiction during plan review; this results doc resolves it the way the parenthetical reads. 3F-2 will remove the legacy variant entirely and the next results doc will report ZERO.

### 2. T3 pre-flush comment glossed over first-allocation case (fixed in `c4a4965`)

Code-quality review of `fade626` noted the original comment over the `needs_readback_grow` gate said the flush fires "before the grow", which is accurate for resize but silent about the first-allocation case (where `needs_grow` also returns `true` because there is no old extent to compare). The fix-up at `c4a4965` reworded the comment to make both cases explicit and to note that first-allocation is harmless (no old image to dangle) while resize is the real hazard.

### 3. Audit catalogue label mismatch for the traps row (pre-existing; deferred to 3F-2)

The `run_legacy_paint_op` audit catalogue at `backend.rs:1722` labels the traps row as `try_vk_render_traps (composite)`, but the real function name in the same file is `try_vk_render_traps_or_tris`. This is pre-existing — predates 3F-1 — and is plan-prescribed (Done condition 13 quotes the plan's expected text verbatim). Fixing it in 3F-1 would be scope creep; 3F-2 will rewrite that catalogue row when it migrates the function, and should correct the label at the same time. Surfaced here for the next plan rev to fold into a 3F-2 task description.

## Commit summary (phase 3F-1)

| Task | Commit | Subject |
|---|---|---|
| Plan update | `5fc5e47` | docs(plans): fold codex review feedback into phase-3F-1 |
| T1 | `afc18f6` | refactor(kms): add DstReadback::needs_grow accessor |
| T2 | `3fe108b` | refactor(kms): add RenderPipelineCache::allocate_descriptor_for_views_into |
| T3 | `fade626` | refactor(kms): migrate try_vk_render_composite to record_paint_batch_op |
| T3 fix-up | `c4a4965` | refactor(kms): clarify needs_readback_grow pre-flush comment |
| T4 (results doc) | this commit | docs(plans): phase-3F-1 validation results |

5 commits since the 3F plan-fold tip; 6 with this results doc.

## Known deferred items

- **Phase 3F-2** — `try_vk_render_traps_or_tris` (the trapezoids + triangles render-composite path) migration to `record_paint_batch_op`. Scope:
  - Per-batch `MaskScratch` plumbing + `MaskScratch::needs_grow(format, w, h) -> bool` (analogue of `DstReadback::needs_grow` from T1).
  - `MaskScratch::upload_r8` migrated from its current self-contained one-shot CB to staging through `BatchUploadArena` inside the closure.
  - Removal of the legacy `RenderPipelineCache::allocate_descriptor_for_views`, `RenderPipelineCache::reset_descriptors`, and `RenderPipelineCache.descriptor_pool` field — they have no remaining callers once traps migrates.
  - Audit catalogue label fix for the traps row (`try_vk_render_traps (composite)` → `try_vk_render_traps_or_tris`) — see "Plan bugs caught" item 3.
  - Removal of the `3D-deferred: render-traps` comment at `backend.rs:5096`.
  - The unconditional `flush_if_needed(BatchFlushReason::ProtocolBarrier)` at `backend.rs:5102` either disappears or, if `MaskScratch::needs_grow` mirrors the dst_readback pattern, becomes a resize-gated pre-flush like 3D's `try_vk_copy_area` and 3F-1's `try_vk_render_composite`.
- **`GlyphAtlas::intern` per-glyph `queue_wait_idle`** — phase-5 sync rework. Unchanged from 3E.
- **`record_get_image`** — still on `run_one_shot_op` with `flush_if_needed(Readback)`. Phase 5 (targeted VkFence per HLD), unchanged from 3D.
- **`MaskScratch` / `CopyScratch` / `dst_readback` `ensure_size` grow paths** — phase 4. After 3F-2 migrates `MaskScratch`'s consumer, all three scratches can defer their grow through the batch retire queue instead of `queue_wait_idle`, subsuming the 3D-installed and 3F-1-installed `needs_grow` pre-flush patterns.

## What's next

Three reasonable next moves; user's call:

1. **Phase 3F-2 planning** — the traps/triangles family + `MaskScratch` arena-staging. Plan via writing-plans + codex review loop, same shape as 3F-1. Smaller than 3F-1 because the legacy descriptor allocator deletion is "remove last caller, drop the methods" rather than "add a new arena-backed variant".
2. **Phase 4** — sync rework. After 3F-2 has migrated all five render-side recorders, the BatchResource lifecycle work that defers `MaskScratch` / `CopyScratch` / `dst_readback` grows through the batch retire queue can land — subsuming the three resize-pre-flush patterns 3D and 3F-1 installed. See `docs/status.md` Phase 4 entry.
3. **Phase 5 / 6** — see `docs/status.md` for the full rendering-rework roadmap (Phase 5 = targeted VkFence per HLD + glyph-atlas-in-batch; Phase 6 = legacy-fallback removal).
