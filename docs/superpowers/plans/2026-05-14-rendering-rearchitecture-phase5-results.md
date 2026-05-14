# Phase 5 — readback fence + scratch grow defer-release — results

Date: 2026-05-14
Plan: `docs/superpowers/plans/2026-05-14-rendering-rearchitecture-phase5.md`
Branch: `graphics-followups`
Predecessor: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase4-results.md`

## Scope landed

Phase 5 finished the `queue_wait_idle` retirement program for the paint-side of the renderer. After Phase 4 narrowed the close-time wait in `PaintBatch::submit_and_wait` to a per-batch `VkFence`, Phase 5 carried the same fence-narrowing into `run_one_shot_op` (every readback / scanout-dump / legacy-paint caller benefits automatically), built the scheduler-side defer-release machinery for scratch-image grow paths, migrated the three scratch types (`CopyScratch` / `DstReadback` / `MaskScratch`) over to it, and retired the redundant grow-time `queue_wait_idle`s in `OpsStaging::ensure` and `GlyphAtlas::grow_staging`. The three pre-flush gates introduced in 3D / 3F-1 / 3F-2 (the `if scratch.needs_grow() { flush_if_needed(ProtocolBarrier) }` pattern) are gone — defer-release subsumes them.

The Phase 5 target documented in `docs/status.md` is closed: the paint hot path no longer has any `queue_wait_idle` outside `Drop` impls and the scanout `drain_all_pending` modeset-teardown path. `run_one_shot_op` now uses a per-op `VkFence` + `wait_for_fences` (5-T1), narrowing the wait from "all queue" to "this submission only." The five in-scope `run_one_shot_op` call sites (`hw_cursor_refresh`, `read_mirror_pixels`, `try_vk_get_image_pixels`, `dump_scanout_one`, `run_legacy_paint_op`) latch `self.renderer_failed = true` on the new path-2 fatal contract — same pattern as Phase 4's `flush_if_needed`. The atlas / gradient / white-mask-init call sites are intentionally not migrated and stay log-and-degrade per pre-existing behavior; they are tracked as known deferred items.

- **T1 (`604f009`)**: `run_one_shot_op` swapped `queue_wait_idle(graphics_queue)` for `create_fence` + `queue_submit2(..., fence)` + `wait_for_fences(&[fence], true, u64::MAX)`. Documented 5-path failure taxonomy (extends Phase 4's 4-path model with the additional pre-submit failure window of `begin_command_buffer` / `record(...)` / `end_command_buffer`). `cb_safe_to_free` flag gates the outer CB free: only path 2 (post-submit wait failure) sets it false, leaking both CB and fence per the documented contract. All five in-scope callers latch `self.renderer_failed = true` on `Err` (upgraded from `log::warn!` to `log::error!`).
- **T2 (`c6dfecc`)**: `RenderScheduler::defer_resource_release` added — adopts a `Box<dyn BatchResource>` into the currently-open paint batch (lazy-opening one in Idle state if none exists) when any batch (open or in flight) might reference the resource, releases synchronously otherwise. Pure-function decision helper `defer_resource_release_decision_for(has_submitted, current_state)` exposes the decision tree to tests without constructing a real `PaintBatch`. **Poisoned-batch handling** (codex round-2 P1): the production path discards a `Poisoned` current batch before deciding so an adopted resource can't leak through Poison's no-op Drop; if submitted predecessors exist, a fresh Idle batch is opened to host the adoption. 10-test matrix covers every `(has_submitted, current_state)` combination.
- **T3 (`eea0316`)**: `CopyScratch::ensure_size_returning_old` replaces `ensure_size`'s `queue_wait_idle` + destroy with allocation-first + old-image-returned-as-`Box<RetiredCopyScratchImage>`. The single caller (`try_vk_copy_area` same-overlap arm in `backend.rs:~3343`) now defer-releases the old image through `scheduler.defer_resource_release`, and the 3D pre-flush gate (`if scratch.needs_grow() { flush_if_needed(ProtocolBarrier) }`) is removed.
- **T4 (`067b6c3`)**: `DstReadback::ensure_returning_old` analogous to T3 (handles per-format Boxed retirement). Two callers (`try_vk_render_composite` at `backend.rs:~5058` and `try_vk_render_traps_or_tris` at `backend.rs:~6123`) defer-release through the scheduler; both 3F-1 / 3F-2 pre-flush gates for DstReadback are removed.
- **T5 (`43dd62c`)**: `MaskScratch::ensure_image_size_returning_old` finishes the trio. One caller (`try_vk_render_traps_or_tris` at `backend.rs:~5016`) defer-releases; the 3F-2 pre-flush gate for MaskScratch is removed.
- **T6 (`11321b6`)**: `OpsStaging::ensure` and `GlyphAtlas::grow_staging` `queue_wait_idle`s deleted. Post-T1 every caller of `OpsStaging` and the only caller of `grow_staging` (`GlyphAtlas::intern`) goes through `run_one_shot_op`, which now waits on a per-op fence before returning. The OLD buffer / staging's last referencing CB therefore has already retired when grow runs — the synchronous wait was redundant. Audit comments left at both sites so future-violation is detectable.

## Preflight checks

End of Phase 5 (HEAD = `11321b6`, plus this T7 commit):

- `cargo +nightly fmt --check` — clean (no diff, exit 0).
- `cargo clippy -p yserver` — 5 pre-existing `doc_lazy_continuation` warnings (`backend.rs:33`, `backend.rs:73`, `backend.rs:74`, `vk/pipeline.rs:104`, and one sibling site). No new warnings.
- `cargo test --workspace`:
  - `yserver` lib: **148 passed, 0 failed, 3 ignored** (+10 vs Phase 4 from T2's pure-function decision matrix).
  - `yserver` binary integration (`ynest`): 9 passed.
  - `yserver-core`: **284 passed**.
  - `yserver-protocol`: **208 passed**.
  - `fixture_smoke`: 2 passed, 1 ignored.
  - Other test binaries: green (1 with 17 ignored, 1 with 1 ignored — same shape as Phase 4).

## Cutover greps

Captured semantically. Line numbers are informational and will drift; the load-bearing claim is the SITE list.

```
$ rg -n 'queue_wait_idle' crates/yserver/src/kms/scheduler/paint_batch.rs
356:    /// narrows the wait from `queue_wait_idle` (all queue) to
419:        // queue_wait_idle. This narrows the wait to OUR submission
```

Both hits are doc / comment references to the Phase-4 retirement. ZERO real call sites in `paint_batch.rs` — the Phase 4 target remains closed.

```
$ rg -n 'queue_wait_idle' crates/yserver/src/kms/vk/ops/mod.rs
59:            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
255:        // 5-T6: no queue_wait_idle. All callers of `OpsStaging`
281:            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
```

Real call sites at `59` (`OpsCommandPool::drop`) and `281` (`OpsStaging::drop`) — both `Drop` impls, conservative cleanup at teardown. The `255` hit is the 5-T6 audit comment. The `run_one_shot_op` body hit (Phase 5 T1 target) is gone, and the `OpsStaging::ensure` body hit (5-T6 target) is gone.

```
$ rg -n 'wait_for_fences' crates/yserver/src/kms/vk/ops/mod.rs
125:    //   2.  wait_for_fences fails: CB IS in flight or device is
130:    //   3.  wait_for_fences Ok: destroy fence, free CB, Ok(()).
166:        match unsafe { vk.device.wait_for_fences(&fences, true, u64::MAX) } {
179:                    "run_one_shot_op: wait_for_fences failed ({e:?}); \
```

One real call site at `166` inside `run_one_shot_op` (the 5-T1 fence-narrowed wait). Matches Done condition 5.

```
$ rg -n 'ensure_returning_old|ensure_size_returning_old|ensure_image_size_returning_old' crates/yserver/src/kms/
crates/yserver/src/kms/backend.rs:3343:                    match scratch.ensure_size_returning_old(u32::from(width), u32::from(height)) {
crates/yserver/src/kms/backend.rs:5016:            match scratch.ensure_image_size_returning_old(bbox_w, bbox_h) {
crates/yserver/src/kms/backend.rs:5058:                match scratch.ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
crates/yserver/src/kms/backend.rs:6112:        // is gone: DstReadback now uses `ensure_returning_old`, which
crates/yserver/src/kms/backend.rs:6123:                match scratch.ensure_returning_old(dst_format, dst_extent.width, dst_extent.height)
crates/yserver/src/kms/vk/mask_scratch.rs:11://! [`MaskScratch::ensure_image_size_returning_old`] allocates a new
crates/yserver/src/kms/vk/mask_scratch.rs:115:    pub fn ensure_image_size_returning_old(
crates/yserver/src/kms/vk/mask_scratch.rs:149:    ///   1. Calling `ensure_image_size_returning_old(width, height)?`
crates/yserver/src/kms/vk/mask_scratch.rs:174:            "MaskScratch::record_upload_r8: caller must ensure_image_size_returning_old first",
crates/yserver/src/kms/vk/dst_readback.rs:107:    pub fn ensure_returning_old(
crates/yserver/src/kms/vk/dst_readback.rs:195:    /// Caller must have called `ensure_returning_old` for the dst
crates/yserver/src/kms/vk/dst_readback.rs:210:                .expect("ensure_returning_old() not called"),
crates/yserver/src/kms/vk/dst_readback.rs:211:            vk::Format::R8_UNORM => self.r8.as_mut().expect("ensure_returning_old() not called"),
crates/yserver/src/kms/vk/copy_scratch.rs:92:    pub fn ensure_size_returning_old(
```

All three scratch migrations landed: `CopyScratch::ensure_size_returning_old` (T3, `copy_scratch.rs:92`), `DstReadback::ensure_returning_old` (T4, `dst_readback.rs:107`), `MaskScratch::ensure_image_size_returning_old` (T5, `mask_scratch.rs:115`). Four caller sites in `backend.rs` (`3343`, `5016`, `5058`, `6123`). Matches Done condition 4.

```
$ rg -n 'defer_resource_release|RetiredCopyScratchImage|RetiredDstReadbackImage|RetiredMaskScratchImage' crates/yserver/src/kms/
crates/yserver/src/kms/backend.rs:3333:                // `self.scheduler.defer_resource_release` borrows
crates/yserver/src/kms/backend.rs:3353:                        .defer_resource_release(vk_arc.clone(), pool_handle, old);
crates/yserver/src/kms/backend.rs:5010:        // `self.scheduler.defer_resource_release` borrows `&mut self`.
crates/yserver/src/kms/backend.rs:5026:                .defer_resource_release(vk_arc.clone(), pool_handle, old);
crates/yserver/src/kms/backend.rs:5052:            // `self.scheduler.defer_resource_release` borrows
crates/yserver/src/kms/backend.rs:5069:                    .defer_resource_release(vk_arc.clone(), pool_handle, old);
crates/yserver/src/kms/backend.rs:6117:            // `self.scheduler.defer_resource_release` borrows
crates/yserver/src/kms/backend.rs:6134:                    .defer_resource_release(vk_arc.clone(), pool_handle, old);
crates/yserver/src/kms/vk/mask_scratch.rs:51:struct RetiredMaskScratchImage {
crates/yserver/src/kms/vk/mask_scratch.rs:57:impl BatchResource for RetiredMaskScratchImage {
crates/yserver/src/kms/vk/mask_scratch.rs:134:        Ok(Some(Box::new(RetiredMaskScratchImage {
crates/yserver/src/kms/scheduler/mod.rs:36:/// Outcome of `RenderScheduler::defer_resource_release_decision`. Pure
crates/yserver/src/kms/scheduler/mod.rs:39:/// `defer_resource_release` which does the action.
crates/yserver/src/kms/scheduler/mod.rs:53:/// `defer_resource_release` discards the Poisoned batch before
crates/yserver/src/kms/scheduler/mod.rs:327:    /// `defer_resource_release` discards the Poisoned batch and
crates/yserver/src/kms/scheduler/mod.rs:331:    pub fn defer_resource_release_decision_for(
crates/yserver/src/kms/scheduler/mod.rs:350:    pub fn defer_resource_release_decision(&self) -> DeferDecision {
crates/yserver/src/kms/scheduler/mod.rs:390:    pub fn defer_resource_release(
crates/yserver/src/kms/scheduler/mod.rs:408:    match self.defer_resource_release_decision() {
crates/yserver/src/kms/vk/dst_readback.rs:50:struct RetiredDstReadbackImage {
crates/yserver/src/kms/vk/dst_readback.rs:57:impl BatchResource for RetiredDstReadbackImage {
crates/yserver/src/kms/vk/dst_readback.rs:137:            Box::new(RetiredDstReadbackImage {
crates/yserver/src/kms/vk/copy_scratch.rs:35:struct RetiredCopyScratchImage {
crates/yserver/src/kms/vk/copy_scratch.rs:40:impl BatchResource for RetiredCopyScratchImage {
crates/yserver/src/kms/vk/copy_scratch.rs:110:        Ok(Some(Box::new(RetiredCopyScratchImage {
```

Production helper defined at `scheduler/mod.rs:390`, decision helper at `scheduler/mod.rs:331`/`350`. Four call sites in `backend.rs`. All three `Retired*Image` BatchResource impls present. Matches Done conditions 3 + 4.

```
$ rg -n 'renderer_failed = true' crates/yserver/src/kms/backend.rs | head -10
1612:                self.renderer_failed = true;
1772:            self.renderer_failed = true;
2446:            self.renderer_failed = true;
2800:            self.renderer_failed = true;
4415:                    self.renderer_failed = true;
6938:            self.renderer_failed = true;
8117:            self.renderer_failed = true;
13009:        backend.renderer_failed = true;
13018:        backend.renderer_failed = true;
13031:        backend.renderer_failed = true;
```

The 5 new T1 latch sites:
- `1772` — `run_legacy_paint_op` wrapper.
- `2446` — `hw_cursor_refresh`.
- `2800` — `read_mirror_pixels`.
- `4415` — `try_vk_get_image_pixels`.
- `8117` — `dump_scanout_one`.

(`1612` is the pre-existing Phase 4 `flush_if_needed` latch. `6938` is the pre-existing `record_and_present_composite` error-path latch. `13009–13042` are test-only.)

```
$ rg -n 'queue_wait_idle' crates/yserver/src/kms/
crates/yserver/src/kms/backend.rs:7412:                // rare; a `queue_wait_idle` here is acceptable. The hot
crates/yserver/src/kms/backend.rs:7415:                    let _ = vkctx.device.queue_wait_idle(vkctx.graphics_queue);
crates/yserver/src/kms/backend.rs:9101:        // batch and queue_wait_idles, draining BOTH before we drop
crates/yserver/src/kms/backend.rs:9575:        // VkImage. flush_if_needed submits the batch + queue_wait_idle
crates/yserver/src/kms/vk/glyph.rs:443:        // 5-T6: no queue_wait_idle. `grow_staging`'s only caller is
crates/yserver/src/kms/vk/glyph.rs:445:        // and waits on each (today via `queue_wait_idle` — still in
crates/yserver/src/kms/vk/glyph.rs:473:            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
crates/yserver/src/kms/vk/gradient.rs:250:            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
crates/yserver/src/kms/vk/mask_scratch.rs:235:            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
crates/yserver/src/kms/vk/dst_readback.rs:297:            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
crates/yserver/src/kms/vk/copy_scratch.rs:166:            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
crates/yserver/src/kms/vk/render_pipeline.rs:474:            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
crates/yserver/src/kms/vk/render_pipeline.rs:613:            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
crates/yserver/src/kms/vk/target.rs:735:                device.queue_wait_idle(self.vk.graphics_queue)?;
crates/yserver/src/kms/scheduler/paint_batch.rs:356:    /// narrows the wait from `queue_wait_idle` (all queue) to
crates/yserver/src/kms/scheduler/paint_batch.rs:419:        // queue_wait_idle. This narrows the wait to OUR submission
crates/yserver/src/kms/vk/logic_fill_pipeline.rs:137:            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
crates/yserver/src/kms/vk/pipeline.rs:247:            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
crates/yserver/src/kms/vk/text_pipeline.rs:329:            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
crates/yserver/src/kms/vk/ops/mod.rs:59:            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
crates/yserver/src/kms/vk/ops/mod.rs:255:        // 5-T6: no queue_wait_idle. All callers of `OpsStaging`
crates/yserver/src/kms/vk/ops/mod.rs:281:            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
```

The full post-Phase-5 `queue_wait_idle` landscape:

- **Drop impls** (conservative cleanup at owning-struct teardown): `glyph.rs:473`, `gradient.rs:250`, `mask_scratch.rs:235`, `dst_readback.rs:297`, `copy_scratch.rs:166`, `render_pipeline.rs:474`, `render_pipeline.rs:613`, `logic_fill_pipeline.rs:137`, `pipeline.rs:247`, `text_pipeline.rs:329`, `ops/mod.rs:59` (`OpsCommandPool::drop`), `ops/mod.rs:281` (`OpsStaging::drop`).
- **Modeset teardown** (`ScanoutBoPool::drain_all_pending`): `vk/scanout.rs:589` — NB this is `device_wait_idle` not `queue_wait_idle`, captured in the Phase-level Done condition discussion below.
- **Init-only one-shot** (`DrawableImage::initialize` via `target.rs:initialize_clear`): `vk/target.rs:735`. Out of scope per the plan's Phase-level Done condition #1.
- **Pre-existing conservative error drain** (`record_and_present_composite` atomic-commit failure path): `backend.rs:7415`. From commit `dde83d03` (2026-05-12), pre-Phase-5; NOT regressed by Phase 5.

Comment / log-string hits: `paint_batch.rs:356/419`, `glyph.rs:443/445`, `ops/mod.rs:255`, `backend.rs:7412` / `9101` / `9575`.

## Done conditions

Per the plan's 8 Phase-level Done conditions in section "## Phase-level Done conditions":

1. ⚠️ **PARTIAL / honest correction**. The plan said "at most one site: `vk/target.rs::initialize_clear` if still present." After Phase 5, there are actually **two** non-`Drop` non-modeset `queue_wait_idle` sites: `vk/target.rs:735` (`initialize_clear`, out-of-scope per the plan) **and** `backend.rs:7415` (`record_and_present_composite` atomic-commit failure path). The `backend.rs:7415` site is **not regressed by Phase 5** — it landed in `dde83d03` (2026-05-12), pre-Phase-5, as a conservative drain on atomic-commit failure. The plan's "at most one site" framing missed it. Both are followup candidates for the same fence-narrowing treatment in a future micro-pass; both are off the per-frame paint hot path.
2. ✅ `queue_wait_idle` is NOT inside `run_one_shot_op`'s body. The body now uses `create_fence` + `queue_submit2(..., fence)` + `wait_for_fences`.
3. ✅ `RenderScheduler::defer_resource_release` exists at `scheduler/mod.rs:390` with both `Synchronous` and `AdoptOpen` branches. Companion `defer_resource_release_decision_for` pure helper exposes the decision tree for unit tests.
4. ✅ Each of `CopyScratch`, `DstReadback`, `MaskScratch` has an `ensure_*_returning_old` method (`copy_scratch.rs:92`, `dst_readback.rs:107`, `mask_scratch.rs:115`) returning `Option<Box<dyn BatchResource>>`. The corresponding `RetiredCopyScratchImage` / `RetiredDstReadbackImage` / `RetiredMaskScratchImage` BatchResource impls exist.
5. ✅ The three pre-flush gates in `backend.rs` (3D `CopyScratch` site, 3F-1 `DstReadback` site, 3F-2 `MaskScratch+DstReadback` site) are entirely gone. Defer-release replaced them.
6. ✅ `cargo +nightly fmt --check` clean; `cargo clippy -p yserver` 5 pre-existing warnings; `cargo test --workspace` green (148 lib + 284 + 208 + ...).
7. ⏳ **TBD — pending user's hardware smoke** for `just rendercheck-yserver`. See "Hardware smoke results" below.
8. ✅ `docs/status.md` reflects Phase 5 done (this T7 commit); `docs/superpowers/plans/2026-05-14-rendering-rearchitecture-phase5-results.md` exists (this file).

## Hardware smoke results

Hardware smoke is user-owned (separate TTY on bare metal). The user runs `just yserver-mate-hw-release` or `just yserver-xfce-hw`, plus `just rendercheck-yserver`, and fills in the subsections below. Reference: Phase 5 plan's "Smoke plan (T7 hardware section)".

**Phase 5 expectation**: at steady state the user should not feel any delta vs post-Phase-4 — the 5-T1 fence-narrowing improves readback latency under contention (no longer serialising on unrelated submissions), but readbacks aren't the per-frame bottleneck on the typical workloads where Phase 4 already shipped the big delta. The defer-release migrations remove the pre-flush gates that only fired on scratch grow (window resize, large composite glyphs); their absence should be invisible at steady state and net-positive on resize bursts. The bee/RDNA2 adapta-nokto + mate-cc reproducer is **not** expected to materially close per the post-3F-2 profile — pixmap-pool (the next item per `docs/status.md`) is the right next move for that.

### Host

TBD.

### General smoke

TBD. Was steady-state MATE indistinguishable from post-Phase-4? Window resize bursts improved or neutral? Any new amdgpu / intel-i915 errors in `dmesg` / journal?

### Readback wait scope (synthetic)

TBD. `xterm` + `import -window root window.ppm` should be fast (`GetImage` goes through `read_mirror_pixels` / `try_vk_get_image_pixels`, both 5-T1-fenced). SIGUSR1 scanout dump should produce a valid PPM (`dump_scanout_one`, also 5-T1-fenced).

### Rendercheck no regressions

TBD. Compare `target/rc-logs/rc-<test>.log` pass/fail counts vs the Phase 4 baseline. Identical shape expected.

### adapta-nokto + mate-cc on bee

TBD. **Phase 5 is not expected to materially close the bee/RDNA2 lag** per the post-3F-2 profile. Capture a fresh perf snapshot anyway for the results doc — the narrative the result reinforces is "wait-idle is gone from the paint hot path; remaining lag is downstream and pixmap-pool is the next move."

### fuji regression check

TBD. Intel was fine pre-Phase 5; should stay fine. Phase 5's 5-T1 narrows the per-op wait (still blocking; data is back on return), so this is a no-regression check.

### Anomalies

TBD.

## Plan bugs caught (folded back into plan / fixed in-tree)

### Round 1 (pre-T1) — codex review against the initial plan

- **P1: `defer_resource_release` Poisoned handling**. The first draft adopted into a Poisoned current batch unconditionally; since `PaintBatch::Drop` on Poisoned is a no-op (the leak-on-error contract), an adopted resource would silently leak. Folded into the plan as the discard-Poisoned-current-batch-before-deciding pattern; the production `defer_resource_release` discards the Poisoned batch and opens a fresh Idle one when submitted predecessors exist.
- **P1: T1 leak contract (5-path failure taxonomy)**. The first draft framed `run_one_shot_op`'s failure as Phase 4's 4-path model verbatim. Codex flagged that `run_one_shot_op` has an additional pre-submit failure window — `begin_command_buffer`, `record(...)` callback, `end_command_buffer` can all fail before the fence is even created. The taxonomy was widened to 5 paths (0a/0b/0c pre-submit, 1a/1b submit-time, 2 wait-failure, 3 success) and the `cb_safe_to_free` flag was introduced to gate the outer CB free.
- **P1: T3/T4/T5 borrow scoping**. The first draft of the scratch-grow migrations had `paint_resources()` borrowing `&mut self` across the `defer_resource_release` call, which conflicts with the scheduler's own `&mut self` borrow. Folded into the plan as the explicit borrow-narrowing pattern: take the old `Box`, drop the scratch borrow, then call `scheduler.defer_resource_release` separately.
- **P3: GlyphAtlas wording**. The first draft's Phase-level Done condition listed `GlyphAtlas::intern` as "still has `queue_wait_idle`" — but post-T1 it no longer does (the per-glyph wait now lives inside `run_one_shot_op` as `wait_for_fences`). Folded into the plan as a clarifying note in Phase-level Done condition #1.

### Round 2 (pre-T1) — second codex pass

- **P1: `renderer_failed` latching at callers**. The first T1 draft only narrowed the wait; it didn't propagate the new fatal-on-path-2 contract to callers. Phase 4's `flush_if_needed` latches `self.renderer_failed = true` on `BatchError::Vk`; T1 must do the same at the 5 in-scope readback / scanout / legacy-paint call sites. Folded into T1 as Step 2.
- **P2: Poisoned-discard test coverage refactor**. The first draft of T2's unit test attempted to construct a real `PaintBatch` in `Poisoned` state to test the discard path. That requires an `Arc<VkContext>` and a `vk::CommandPool`, which the test environment doesn't have. Refactored into a pure-function decision helper `defer_resource_release_decision_for(has_submitted, current_state)` that takes the state args explicitly — the 10-case test matrix covers every combination including the load-bearing Poisoned cases without constructing a `PaintBatch`.

### Round 3 (pre-T1) — third codex pass

- **P2: `white_mask_image` setup deferred**. Codex flagged that `open_with_commit`'s `run_one_shot_op` for `white_mask_image` would, by the T1 contract, need to latch `renderer_failed` on Err. But `open_with_commit` runs during backend construction where `&mut self` doesn't yet exist as a coherent latch target, and today's behavior already log-and-degrades (leaves `white_mask_image = None`; downstream paths detect and fall back). Documented as an init-only best-effort exception in T1 Step 2's "Out-of-scope call sites" list; preserved verbatim.

### Round 4 (pre-T6) — fourth codex pass

- **P3: stale `GlyphAtlas::intern queue_wait_idle` wording in T6 grep + Phase-level Done**. The first T6 draft's catalogue grep expected to see `queue_wait_idle` retired from `GlyphAtlas::intern`. But `intern` doesn't have a direct `queue_wait_idle` — it has a `run_one_shot_op` call, which post-T1 uses `wait_for_fences` internally. The grep expectations were corrected; T6's actual target is `grow_staging` (the back-pressure-allocator grow path), not `intern`.

### Per-task code reviews (post-commit codex passes)

- **T1**: one P3 stale comment caught in code review (`backend.rs:~8043` in `dump_scanout_one` — "run_one_shot_op submits + waits idle"). Folded into this T7 commit (Part C of the spec) as the per-op-VkFence-aware rephrasing.
- **T2 / T3 / T4 / T5 / T6**: all clean — no findings beyond the round-1–round-4 plan reviews above.
- **T6 deviation**: during T6 implementation, the agent noted that `OpsStaging::ensure`'s function-level doc comment ("Idle-waits the graphics queue before tearing the old buffer down — eager-submit-per-op means there's no in-flight work referencing it, but cheap to be safe") was stale post-T6 — the body comment was updated but the function-level doc still implied a synchronous wait. Folded into this T7 commit (Part C of the spec) as a rephrase matching the inline 5-T6 audit comment.

## Commit summary (phase 5)

| Task | Commit | Subject |
|---|---|---|
| Plan | `e8ee222` | docs(plans): phase 5 implementation plan — readback fence + scratch grow defer-release |
| T1 | `604f009` | refactor(kms): fence run_one_shot_op's per-op wait (5-T1) |
| T2 | `c6dfecc` | refactor(kms): add RenderScheduler::defer_resource_release (5-T2) |
| T3 | `eea0316` | refactor(kms): defer-release CopyScratch grow (5-T3) |
| T4 | `067b6c3` | refactor(kms): defer-release DstReadback grow (5-T4) |
| T5 | `43dd62c` | refactor(kms): defer-release MaskScratch grow (5-T5) |
| T6 | `11321b6` | refactor(kms): delete redundant queue_wait_idle in OpsStaging::ensure and GlyphAtlas::grow_staging (5-T6) |
| T7 (results doc) | this commit | docs(plans): phase 5 validation results |

7 commits from plan to T6; 8 with this results doc.

## Known deferred items

- **`GlyphAtlas::intern`'s per-glyph one-shot submit-and-wait pattern**. Post-T1 it no longer has a direct `queue_wait_idle` — its per-glyph wait now lives inside `run_one_shot_op` as `wait_for_fences` (narrower wait scope). But the per-glyph submit-and-wait shape itself remains: each interned glyph triggers a fresh `run_one_shot_op`. Batching glyph upload into the open `PaintBatch` is the eventual move; that's a separate phase (atlas batching, not the `queue_wait_idle` retirement program).
- **`GradientPicture` creation `run_one_shot_op`** (`vk/gradient.rs:~522`). Same shape as the atlas — one-shot submit-and-wait per gradient. Phase 5 narrowed the wait via T1; batching into `PaintBatch` is a future move.
- **`white_mask_image` setup in `open_with_commit`** (`backend.rs:~2137`). Init-only one-shot during backend construction; log-and-degrade on failure (downstream paths detect `None` and fall back). Intentional best-effort exception per the T1 Step 2 spec — this site does not need `renderer_failed` latching because backend construction failure has a different model than steady-state fatal-on-path-2.
- **`vk/target.rs::initialize_clear`** (`target.rs:735`). One-shot `queue_wait_idle` on `DrawableImage::initialize`. Off the per-frame paint hot path (one-shot at drawable-image creation). Out of scope per the Phase 5 plan; fence-narrow in a future micro-pass.
- **`backend.rs:~7415` `record_and_present_composite` error-path `queue_wait_idle`**. Pre-existing from `dde83d03` (2026-05-12), pre-Phase-5. Conservative drain on atomic-commit failure. Candidate for per-CB fence in the same spirit as 5-T1.
- **`renderer_failed` latching propagation through atlas / gradient `run_one_shot_op` callers**. Pre-existing gap; not regressed by Phase 5. T1 latched the 5 in-scope readback / scanout / legacy-paint sites; atlas (`vk/glyph.rs:~376`) and gradient (`vk/gradient.rs:~522`) still warn-and-continue per pre-existing behavior. Folding that here widens T1 beyond its goal — propagation through `intern`'s `Result` into `try_vk_text_run` / `try_vk_render_composite_glyphs` callers is the right Phase-5-followup shape.

## What's next

Per `docs/status.md`, the immediate next-phase priority is **pixmap-allocation pool — burst-absorbing `VkImage` recycling**. The cross-vendor reproducer (adapta-nokto + mate-cc visible: catastrophic on bee/RDNA2 + fuji/Intel under recent Arch kernels) is not closed by Phase 5; the post-3F-2 profile pointed at the per-pixmap `VkImage`/memory/VA alloc-free path under burst rate as the bottleneck, and `VkImagePool` keyed by `(extent, format, usage)` is the right fix.

**Phase 6 — batch-owned refcounted handles** subsumes the Phase 5 `Retired*Image` pattern. Phase 5 introduced three local BatchResource impls (`RetiredCopyScratchImage`, `RetiredDstReadbackImage`, `RetiredMaskScratchImage`) wrapping the OLD scratch handles for defer-release; Phase 6's refcounted-handle work generalises this — `BatchResource` becomes the universal shape for adopting destroyed Vulkan handles into the open paint batch, retiring the need for both the destruction-barrier collection (3B) AND the per-scratch `Retired*Image` wrappers Phase 5 introduced. Phase 6 stays the structural cleanup follow-up, second in priority behind pixmap-pool.
