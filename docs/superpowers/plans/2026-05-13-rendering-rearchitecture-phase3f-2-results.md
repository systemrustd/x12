# Phase 3F-2 — rendering re-architecture — render-traps/triangles migration + MaskScratch arena — results

Date: 2026-05-13
Plan: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-2.md`
Branch: `graphics-followups`
Predecessor: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-1-results.md`

## Scope landed

Phase 3F-2 migrated the second (and last) RENDER family — `try_vk_render_traps_or_tris`, the trapezoid + triangle path — off the legacy `flush_if_needed(ProtocolBarrier) + run_one_shot_op` + shared-descriptor-pool fallback onto `record_paint_batch_op` with mask coverage staged through `BatchUploadArena` and descriptor sets allocated through `BatchDescriptorArena` (the 3F-1 arena allocator). The unconditional pre-record `ProtocolBarrier` flush that fired on every trapezoid call is gone; trap-heavy frames (rounded GTK widgets, thin-theme borders, anti-aliased text rendering paths that fall to trapezoids) now pack into the open `PaintBatch` alongside fill / copy / put_image / text-run / composite. With 3F-2 landed, **all paint-side recorders are now on `record_paint_batch_op` / `record_paint_op`** — the render family migration is complete, and the remaining `run_one_shot_op` sites in `backend.rs` are confined to readback/cursor/scanout-dump handlers + the `run_legacy_paint_op` fallback shim itself.

- **T1 (`bc4f98e`)**: `MaskScratch::needs_image_grow(w, h) -> bool` accessor. Returns `true` for an unallocated slot and for `w > extent.width || h > extent.height`, `false` for an already-large-enough slot. Mirrors `CopyScratch::needs_grow` from 3D and `DstReadback::needs_grow` from 3F-1 T1; used by T3 as the mask half of the combined resize-only pre-flush.
- **T1 doc fix (`1860afc`)**: the T1 accessor doc comment was lifted from `DstReadback::needs_grow` (which IS per-format because `DstReadback` reallocates on format change as well as size change) but `MaskScratch` is single-format (R8_UNORM only), so the "per-format" qualifier was misleading. Reworded to drop it and just say "first allocation or a real grow".
- **T2 (`e427127`, additive-only after the `83eb883` plan restructure)**: arena-aware MaskScratch primitives added alongside the legacy `upload_r8`. `MaskScratch::ensure_image_size` promoted to `pub` so the caller can size the image outside the closure (it takes `&mut self.mask_scratch` and runs in the post-pre-flush window). `MaskScratch::record_upload_r8(&mut self, vk, cb, src_buffer, src_offset, w, h)` recorded the barrier–copy–barrier sequence from a caller-provided staging buffer (the `BatchUploadArena`'s persistent buffer in T3). Legacy `upload_r8` / `ensure_staging` / `allocate_staging` and the four `staging_*` fields were deliberately left in place so the tree compiled at T2's tip — they were removed in T3 atomically with the call-site migration.
- **T3 (`f242945`)**: `try_vk_render_traps_or_tris` migrated to `record_paint_batch_op`. Six disjoint borrows captured (`dst_mirror`, `solid_src_image`, `mask_scratch`, `dst_readback`, `render_cache` plus the `batch` arg). Mask coverage uploaded inside the closure via `batch.upload_arena_mut().alloc(needed, 4)` + `copy_nonoverlapping` + `mask_scratch.record_upload_r8(vk, cb, alloc.buffer, alloc.offset, w, h)`. Descriptor set allocated inside the closure via `render_cache.allocate_descriptor_for_views_into(batch.descriptor_arena_mut(), …)`. Arena-OOM follows the 3C T2 outer-flag pattern (`arena_oom = true; return Ok(())`) — failure happens before any CB recording, so a poisoned-batch return would discard unrelated work already recorded in this batch. The unconditional pre-record `ProtocolBarrier` flush became a `needs_mask_grow || needs_readback_grow`-gated combined resize-only pre-flush, fired only when `ensure_image_size` or `dst_readback.ensure` would actually reallocate. The legacy `MaskScratch::upload_r8` / `ensure_staging` / `allocate_staging` + the four `staging_*` fields, the `3D-deferred: render-traps` comment, and the audit-catalogue label fix (3F-1 surfaced the label mismatch `try_vk_render_traps (composite)` → `try_vk_render_traps_or_tris`) all landed in this commit.
- **T3 SAFETY fix-up (`c9f18f8`)**: the T3 SAFETY comment on the arena memcpy originally claimed an upstream `debug_assert!` enforced `mask_len == needed`. There was no such assert; the invariant actually holds by construction (`rasterize_trapezoids` and `rasterize_triangles` both return `vec![0u8; (w * h) as usize]`, and `mask_w * mask_h == bbox_w * bbox_h == coverage_mask.len()`). Reworded to say so plainly, and added a local `debug_assert_eq!(mask_len, needed as usize)` to catch any future drift.
- **T4 (`df5dbba`)**: legacy `RenderPipelineCache` shared-pool API removed in full. After T3 cleared the last caller, `RenderPipelineCache::allocate_descriptor_for_views`, `RenderPipelineCache::reset_descriptors`, the `RenderPipelineCache.descriptor_pool` field, the `MAX_DESCRIPTOR_SETS_PER_FRAME` constant in `render_pipeline.rs`, and the pool-construction + pool-destruction calls in `new` / `Drop` are all gone. The `BatchDescriptorArena` is now the sole descriptor-set provider for the render pipeline; lifetime is per-batch, retired through the batch retire queue.
- **T4 doc fix (`196362f`)**: the `RenderPipelineCache` struct doc summary still said "Cache + sampler + pool …" after the pool was deleted. Dropped the "pool" word.

## Preflight checks

End of 3F-2 (HEAD = `196362f`):

- `cargo +nightly fmt --check` — clean (no diff).
- `cargo clippy -p yserver` — 5 pre-existing `doc_lazy_continuation` warnings (`backend.rs:33`, `backend.rs:73`, `backend.rs:74`, `vk/pipeline.rs:104`, and one sibling site). No new warnings.
- `cargo test --workspace`:
  - `yserver` lib: **138 passed, 0 failed, 3 ignored**.
  - `yserver` binary integration (`ynest`): 9 passed.
  - `yserver-core`: **284 passed**.
  - `yserver-protocol`: **208 passed**.
  - `fixture_smoke`: 2 passed, 1 ignored.
  - Other test binaries: green (3 with 0 passed, 1 with 17 ignored, 2 with 1 ignored — same shape as 3F-1).

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
7998:        run_one_shot_op(vk, pool_handle, |vk, cb| {
```

ZERO hits inside `try_vk_render_traps_or_tris` (4731..5242) or `try_vk_render_composite` (5649..7972). Remaining production sites:
- `1755` — `run_legacy_paint_op` body itself (legacy entry point).
- `2125` — `open_with_commit` constructor (one-time `record_solid_color_clear` init).
- `2419` — `hw_cursor_refresh` (readback handler).
- `2769` — `read_mirror_pixels` (readback handler).
- `4373` — `try_vk_get_image_pixels` (readback handler).
- `7998` — `dump_scanout_one` (scanout dump).

(Lines `4164/4171/4178` are doc-comments, not call sites.) Compared to end-of-3F-1 (7 production sites), the count drops by one — the `try_vk_render_traps_or_tris` site at the old line `5125` is gone. **No `try_vk_render_*` function calls `run_one_shot_op` any more.**

```
$ rg -n 'flush_if_needed[(]BatchFlushReason::ProtocolBarrier' crates/yserver/src/kms/backend.rs
1742:        if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
3313:                    if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
4990:            if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
6086:            if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
```

Sites:
- `1742` — `run_legacy_paint_op` body (legacy entry point).
- `3313` — `try_vk_copy_area` same-overlap resize-only pre-flush (3D-installed, gated on `CopyScratch::needs_grow`).
- `4990` — `try_vk_render_traps_or_tris` **combined-resize-only** pre-flush (NEW in 3F-2, gated on `MaskScratch::needs_image_grow || DstReadback::needs_grow`).
- `6086` — `try_vk_render_composite` resize-only pre-flush (3F-1, gated on `DstReadback::needs_grow`).

The OLD unconditional pre-record flush inside `try_vk_render_traps_or_tris` (formerly at ~line 5102, captured in the 3F-1 results doc) is gone. Drawable-destruction and gradient-create call sites use a multi-line `flush_if_needed(...)` form that does not match the strict-prefix grep — they still live elsewhere in `backend.rs` (unchanged from 3F-1).

```
$ rg -n 'record_paint_batch_op|record_paint_op' crates/yserver/src/kms/backend.rs | wc -l
50
```

Raw count includes doc comments, definitions, and one test. Up by 3 from 3F-1's 47 — accounted for by the new T3 call site at `backend.rs:5148` plus a couple of new doc-comment references inside the migrated function. Production scheduler-level call sites continue to exceed the plan's "≥ 11" floor.

```
$ rg -n '\.allocate_descriptor_for_views\(' crates/yserver/src/kms/
(no hits)
```

ZERO. The legacy shared-pool variant is fully gone after T4 (it had one caller — `try_vk_render_traps_or_tris` at `backend.rs:5025` — which T3 migrated, then T4 removed the method).

```
$ rg -n 'allocate_descriptor_for_views_into' crates/yserver/src/kms/
crates/yserver/src/kms/backend.rs:5181:                let descriptor_set = render_cache.allocate_descriptor_for_views_into(
crates/yserver/src/kms/backend.rs:6197:                let descriptor_set = render_cache.allocate_descriptor_for_views_into(
crates/yserver/src/kms/vk/render_pipeline.rs:429:    pub fn allocate_descriptor_for_views_into(
```

1 definition (`render_pipeline.rs:429`) + 2 call sites in `backend.rs` (`5181` inside `try_vk_render_traps_or_tris` from 3F-2 T3, `6197` inside `try_vk_render_composite` from 3F-1 T3). Matches the plan.

```
$ rg -n 'reset_descriptors\(' crates/yserver/src/kms/
(no hits)
```

ZERO. The method is gone after T4. Matches the plan.

```
$ rg -n 'fn needs_grow\b|fn needs_image_grow\b|\.needs_grow\(|\.needs_image_grow\(' crates/yserver/src/kms/
crates/yserver/src/kms/vk/mask_scratch.rs:87:    pub fn needs_image_grow(&self, width: u32, height: u32) -> bool {
crates/yserver/src/kms/vk/dst_readback.rs:84:    pub fn needs_grow(&self, format: vk::Format, width: u32, height: u32) -> bool {
crates/yserver/src/kms/backend.rs:3310:                    .is_some_and(|s| s.needs_grow(u32::from(width), u32::from(height)));
crates/yserver/src/kms/backend.rs:4982:            .is_some_and(|m| m.needs_image_grow(bbox_w, bbox_h));
crates/yserver/src/kms/backend.rs:4987:                .is_some_and(|r| r.needs_grow(dst_format, dst_extent.width, dst_extent.height));
crates/yserver/src/kms/backend.rs:6083:                .is_some_and(|r| r.needs_grow(dst_format, dst_extent.width, dst_extent.height));
crates/yserver/src/kms/vk/copy_scratch.rs:70:    pub fn needs_grow(&self, width: u32, height: u32) -> bool {
```

3 definitions (`copy_scratch.rs:70` from 3D, `dst_readback.rs:84` from 3F-1, `mask_scratch.rs:87` from 3F-2 T1) + **4** production call sites in `backend.rs`:
- `3310` — `try_vk_copy_area` same-overlap (3D).
- `4982` — `try_vk_render_traps_or_tris` mask half of the combined pre-flush (3F-2).
- `4987` — `try_vk_render_traps_or_tris` dst_readback half of the combined pre-flush (3F-2).
- `6083` — `try_vk_render_composite` (3F-1).

The plan's expected output says "3 call sites in `backend.rs`" but T3 deliberately uses **two** `needs_*` checks at the migrated site (one for `mask_scratch`, one for `dst_readback`) combined with `||` into a single pre-flush. Both are real `needs_*` accessor calls and both are load-bearing — this is the combined-pre-flush mechanic the plan described in T3 Step 7 and in Done condition 10. The "3 call sites" expectation in the cutover-greps section was a counting slip — there is one MORE call site than the plan listed, in the same function the plan named. Tagged under "Plan bugs caught" item 5.

```
$ rg -n '3D-deferred: render-traps' crates/yserver/src/kms/backend.rs
(no hits)
```

ZERO. The stale comment that lived inside the removed pre-flush block is gone. Matches the plan.

```
$ rg -n 'MaskScratch::upload_r8|fn upload_r8' crates/yserver/src/kms/
(no hits)
```

ZERO. The legacy entry point is gone after T3 — replaced by `needs_image_grow + ensure_image_size + record_upload_r8`. Matches the plan.

```
$ rg -n 'descriptor_pool' crates/yserver/src/kms/vk/render_pipeline.rs
(no hits)
```

ZERO. Field, creator, and destroyer all gone after T4. Matches the plan.

## Done conditions

Per the plan's 17 Done conditions:

1. ✅ `cargo +nightly fmt --check` clean.
2. ✅ `cargo clippy -p yserver` produces 5 pre-existing `doc_lazy_continuation` warnings; no new warnings.
3. ✅ `cargo test --workspace` green; `yserver` lib **138 passed**.
4. ✅ `MaskScratch::needs_image_grow(w, h) -> bool` exists at `crates/yserver/src/kms/vk/mask_scratch.rs:87`. Returns `true` for unallocated slot and for `w > extent.width || h > extent.height`, `false` otherwise. (Verified by reading the impl in T1 commit `bc4f98e`.)
5. ✅ `MaskScratch::ensure_image_size` is `pub` (`mask_scratch.rs:101`). `MaskScratch::record_upload_r8(&mut self, vk, cb, src_buffer, src_offset, w, h)` exists (`mask_scratch.rs:142`).
6. ✅ `MaskScratch::upload_r8(pool, w, h, bytes)` does NOT exist (grep `MaskScratch::upload_r8|fn upload_r8` returns ZERO). `MaskScratch::ensure_staging` and `MaskScratch::allocate_staging` do NOT exist (grep returns ZERO inside `mask_scratch.rs`). `MaskScratch.staging_buffer / staging_memory / staging_mapped / staging_size` fields do NOT exist (grep returns ZERO inside `mask_scratch.rs`). All deletions landed atomically with the call-site migration in T3 `f242945`.
7. ✅ `try_vk_render_traps_or_tris` uses `record_paint_batch_op` at `backend.rs:5147`:
   - Six disjoint captures: `dst_mirror` (`5119`), `solid_src_image` (`5122`), `mask_scratch` (`5123`), `dst_readback` (`5124..5128`), `render_cache` (`5141`), plus the `batch` argument bound by the closure signature.
   - Mask upload INSIDE the closure: `batch.upload_arena_mut().alloc(needed, 4)` at `5151`, `std::ptr::copy_nonoverlapping(mask_bytes.as_ptr(), alloc.mapped_ptr.as_ptr(), mask_len)` at `5172`, then `mask_scratch.record_upload_r8(vk, cb, alloc.buffer, alloc.offset, mask_w, mask_h)` at `5179`.
   - Descriptor set allocated INSIDE the closure via `render_cache.allocate_descriptor_for_views_into(batch.descriptor_arena_mut(), …)` at `5181`.
   - Outer-flag `arena_oom` pattern used for arena alloc failure at `5142/5158` — closure returns `Ok(())` on OOM so the unrelated 3B/3C/3E/3F-1 work already recorded in this batch is not discarded.
8. ✅ The OLD `flush_if_needed(BatchFlushReason::ProtocolBarrier)` block inside `try_vk_render_traps_or_tris` (formerly at ~line 5102, unconditional) is GONE. The four remaining `flush_if_needed(BatchFlushReason::ProtocolBarrier` sites are catalogued under "Cutover greps" above.
9. ✅ The `3D-deferred: render-traps needs…` comment is GONE (grep returns no hits).
10. ✅ The combined-grow pre-flush at `backend.rs:4988` (`if needs_mask_grow || needs_readback_grow`) gates the flush on `MaskScratch::needs_image_grow` (`4982`) and `DstReadback::needs_grow` (`4987`). Steady-state (stable mask bbox + stable dst extent + same format) skips the flush.
11. ✅ Ordering invariant holds: the pre-flush at `4988..4994` runs BEFORE `mask_scratch.ensure_image_size` at `5001..5009` AND BEFORE `dst_readback.ensure` at `5034` inside the body. Verified by reading the migrated function.
12. ✅ `RenderPipelineCache::reset_descriptors` does NOT exist (grep `reset_descriptors\(` inside `crates/yserver/src/kms/` returns ZERO).
13. ✅ `RenderPipelineCache::allocate_descriptor_for_views` (legacy variant) does NOT exist (grep `\.allocate_descriptor_for_views\(` inside `crates/yserver/src/kms/` returns ZERO).
14. ✅ `RenderPipelineCache.descriptor_pool` field does NOT exist (grep `descriptor_pool` inside `crates/yserver/src/kms/vk/render_pipeline.rs` returns ZERO).
15. ✅ `MAX_DESCRIPTOR_SETS_PER_FRAME` in `render_pipeline.rs` does NOT exist (manually verified — `rg -n MAX_DESCRIPTOR_SETS_PER_FRAME crates/yserver/src/kms/vk/render_pipeline.rs` returns no hits). The same name in `pipeline.rs` is unrelated and stays.
16. ✅ The `run_legacy_paint_op` audit-catalogue entry at `backend.rs:1722` reads `try_vk_render_traps_or_tris:      render::record_render_composite     — migrated 3F-2 (record_paint_batch_op + arena upload + arena descriptors)`. The 3F-1-flagged label mismatch (`try_vk_render_traps (composite)` → `try_vk_render_traps_or_tris`) is now corrected.
17. ⏳ **TBD — pending the user's hardware smoke**. See "Hardware smoke results" below.

## Hardware smoke results

Hardware smoke is user-owned (separate TTY on bare metal). The user runs `just yserver-mate-hw-release` or `just yserver-xfce-hw`, plus `just rendercheck-yserver`, and fills in the subsections below.

**Headline reproducer**: `docs/known-issues.md` lines 409+ documents the **mate-control-center under adapta-nokto: catastrophic mouse lag** issue (filed 2026-05-13 on `bee`, predates 3F-1). The leading hypothesis attributed the lag to per-call `vkQueueWaitIdle` in two paths: `try_vk_render_traps_or_tris` (Trapezoids driving rounded GTK widgets + thin theme borders + anti-aliased edges) and `GlyphAtlas::intern` (per-glyph wait-idle on first sighting of each glyph). **3F-2 retires the traps half** (the traps function no longer goes through `run_one_shot_op` so it no longer ends in a `vkQueueWaitIdle`). The expected outcome is **noticeably reduced lag with adapta-nokto + mate-cc**. If the lag is materially better, update the known-issue with "3F-2 reduced this; remaining lag attributed to Phase 5 atlas rewrite". If unchanged, the per-glyph `GlyphAtlas::intern` wait-idle is the dominant remaining cost and Phase 5 will close the gap — capture a `perf` profile under the lag to attribute the remaining cost.

### Host

TBD.

### Log file summary

TBD. (`yserver-hw.log` — grep for `vk render_traps: record failed`, `paint batch submit failed`, `renderer_failed`, `DEVICE_LOST`, `vk render_traps: pre-resize flush failed`, `vk render_traps: mask ensure_image_size failed`, `vk render_traps: arena alloc`, `descriptor set` validation messages.)

### rendercheck delta vs pre-3F-2 baseline

TBD. (Compare against `docs/test-status.md` 2026-05-10 yserver/KMS baseline; specifically check `triangles` 456/456 still passes and that `composite`/`cacomposite` partial-pass counts do not regress.)

### adapta-nokto + mate-cc lag delta vs known-issue

TBD. Cross-reference `docs/known-issues.md` lines 409+. Expected: materially reduced (3F-2 removed the per-call `queue_wait_idle` from the traps path that the known-issue named as half the root cause). If unchanged, capture `perf` profile and attribute the remaining cost to `GlyphAtlas::intern` (Phase 5 scope).

### Anomalies

TBD.

## Plan bugs caught (folded back into plan / fixed in-tree)

### 1. T2/T3 split as originally written was uncompilable (restructured at `83eb883` before T2 code landed)

The biggest plan-bug catch of the phase. As originally written, T2 was "decompose `MaskScratch::upload_r8` into `pub ensure_image_size + record_upload_r8`, delete the private staging buffer + the legacy `upload_r8`". T3 was "migrate `try_vk_render_traps_or_tris` to use the new primitives". The trouble: T3's caller still called `MaskScratch::upload_r8` *before* T3 ran. If T2 deleted the function, the tree did not compile at T2's tip — a hard violation of the plan's own "every commit builds" rule.

The first T2 implementer correctly stopped at the boundary, noticed the dangling caller, and surfaced the contradiction. The plan was restructured at `83eb883` (`docs(plans): make 3F-2 T2 additive (legacy deletion moves to T3)`): T2 became additive-only (`ensure_image_size` promoted to `pub`, `record_upload_r8` added) while the legacy `upload_r8` / `ensure_staging` / `allocate_staging` and the four `staging_*` fields stayed in place. T3 absorbed the legacy deletion, doing the migration and the legacy-removal in a single atomic commit. This is the shape we want for future "decompose + migrate" plans: the additive step lands first, the call-site migration + legacy removal lands second.

### 2. T1 accessor doc comment said "per-format" lifted from `DstReadback::needs_grow` (fixed in `1860afc`)

The T1 doc comment on `MaskScratch::needs_image_grow` was templated against `DstReadback::needs_grow`, which IS per-format because `DstReadback` reallocates on format change as well as size change. `MaskScratch` is single-format (R8_UNORM only — coverage masks are always 8-bit), so the "per-format" qualifier was actively misleading: it implied `MaskScratch` might one day grow a multi-format check, which it will not. Fixed by dropping the "per-format" word and re-anchoring the doc on "first allocation or a real grow".

### 3. T3 SAFETY comment claimed an upstream `debug_assert!` that did not exist (fixed in `c9f18f8`)

The T3 SAFETY comment on the arena memcpy was lifted from a similar pattern elsewhere and claimed an upstream `debug_assert!` enforced `mask_len == needed`. There was no such assert. The invariant actually holds *by construction*: `rasterize_trapezoids` and `rasterize_triangles` both return `vec![0u8; (w * h) as usize]`, and `mask_w * mask_h == bbox_w * bbox_h == coverage_mask.len()`. Fixed by rewording the SAFETY to say so plainly, and adding a local `debug_assert_eq!(mask_len, needed as usize)` at the memcpy site to catch any future drift in the rasterizer return shape.

### 4. T4 `RenderPipelineCache` struct doc summary not updated when pool deleted (fixed in `196362f`)

T4's commit `df5dbba` deleted the descriptor pool but left the struct's doc summary saying "Cache + sampler + pool …". A trivial drift; fixed in `196362f` by dropping the "pool" word so the doc reflects the post-3F-2 shape.

### 5. Cutover-greps count slip in the plan: "3 call sites in `backend.rs`" should be 4

The plan's "Cutover greps (post-3F-2 — semantic, not numeric)" entry for the `needs_*` accessor expected "3 call sites in `backend.rs` (`try_vk_copy_area` 3D, `try_vk_render_composite` 3F-1, `try_vk_render_traps_or_tris` 3F-2)". In practice T3's combined pre-flush uses **two** `needs_*` calls (mask half + dst_readback half) `||`-combined into the single pre-flush, so the actual hit count inside `try_vk_render_traps_or_tris` is two, and the total `backend.rs` call-site count is **four**. Both are load-bearing and both match Done condition 10. This was a counting slip in the plan's expected output — the underlying mechanic in T3 Step 7 / Done condition 10 was specified correctly. Fold into a future plan rev as a clarification on how the combined pre-flush expands across the cutover grep.

## Commit summary (phase 3F-2)

| Task | Commit | Subject |
|---|---|---|
| T1 | `bc4f98e` | refactor(kms): add MaskScratch::needs_image_grow accessor |
| T1 doc fix | `1860afc` | docs(kms): drop "per-format" from needs_image_grow doc |
| T2/T3 plan restructure | `83eb883` | docs(plans): make 3F-2 T2 additive (legacy deletion moves to T3) |
| T2 | `e427127` | refactor(kms): add arena-aware MaskScratch primitives (additive) |
| T3 | `f242945` | refactor(kms): migrate try_vk_render_traps_or_tris to record_paint_batch_op |
| T3 SAFETY fix | `c9f18f8` | docs(kms): correct SAFETY comment on traps arena memcpy |
| T4 | `df5dbba` | refactor(kms): remove legacy RenderPipelineCache shared-pool API |
| T4 doc fix | `196362f` | docs(kms): drop stale "pool" from RenderPipelineCache struct doc |
| T5 (results doc) | this commit | docs(plans): phase-3F-2 validation results |

8 commits since the 3F-2 plan tip; 9 with this results doc.

## Known deferred items

- **Phase 4 — sync rework**. With 3F-2 landed and the entire paint hot path batched, the remaining `vkQueueWaitIdle`s are:
  - `PaintBatch::submit_and_wait` — fires once per batch close. Phase 4's primary target.
  - `run_one_shot_op` — wraps every readback handler (`hw_cursor_refresh`, `read_mirror_pixels`, `try_vk_get_image_pixels`) plus `open_with_commit` and `dump_scanout_one`. Phase 4 / Phase 5 split: the readback handlers want a targeted VkFence (Phase 5 HLD); the constructor + scanout-dump sites are one-shots whose cost amortizes to nothing.
- **Phase 5 — targeted VkFence for readback + `GlyphAtlas::intern` per-glyph wait-idle**. The readback handler triplet (`hw_cursor_refresh`, `read_mirror_pixels`, `try_vk_get_image_pixels`) is the natural targeted-VkFence rewrite per the rendering rework HLD. `GlyphAtlas::intern`'s per-glyph wait-idle is the remaining big-ticket sync cost on text-heavy / theme-switch workloads, and the second half of the `docs/known-issues.md` adapta-nokto + mate-cc root cause.
- **Phase 6 — batch-owned refcounted handles**. The structural fix for record-time CPU layout tracking under poisoning. After Phase 5 retires per-call wait-idles, Phase 6 reshapes BatchResource so the three resize-pre-flush patterns (3D's `CopyScratch`, 3F-1's `DstReadback`, 3F-2's `MaskScratch`) can defer their grow through the batch retire queue and drop the `needs_*_grow` pre-flushes entirely.

## What's next

**Phase 4 planning** is the natural next move. 3F-2 has retired the last paint-side `run_one_shot_op` (every `try_vk_render_*` and `try_vk_copy_*` and `try_vk_fill_*` and `try_vk_put_*` and `try_vk_text_*` recorder packs into the open `PaintBatch`). Phase 4's `vkQueueWaitIdle` retirement from `PaintBatch::submit_and_wait` is the next clear win and what would close the remaining gap on workloads still bottlenecked by GPU-drain serialization — most visibly the `docs/known-issues.md` adapta-nokto + mate-cc reproducer, *if* the 3F-2 smoke shows the traps half of the lag is gone and the glyph-atlas + close-time wait are the new dominant costs.

Three reasonable shapes Phase 4 could take, in priority order:

1. **`PaintBatch::submit_and_wait` → submit + per-batch-fence handoff.** Most disruptive but biggest single win — every protocol-barrier flush stops blocking. Requires the batch retire queue to drain fenced work asynchronously.
2. **`record_get_image` (the three readback handlers) → targeted VkFence per HLD.** Phase 5 scope as written but small enough to pull forward if Phase 4 turns out to be longer. Removes the readback half of `run_one_shot_op`'s remaining cost.
3. **`GlyphAtlas::intern` per-glyph wait-idle → batch-owned atlas growth.** Phase 5 scope; the second half of the adapta-nokto + mate-cc story. If 3F-2's smoke shows the lag is materially better with traps gone but text-rendering frames still pay a tax, this is the next clear lever.

The 3F-2 hardware smoke result will determine which of these three is the highest-yield Phase 4 target.
