# Phase 3D — rendering re-architecture — validation results

Date: 2026-05-13
Plan: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3d.md` (v3 — codex-approved after 2 review rounds)
Branch: `graphics-followups`
Predecessor: phase 3C (`ac270d9` results doc + `9a166b1` review-nit fixes)

## Scope landed

Phase 3D was deliberately narrow: one recorder migration to complete the COPY family. After 3D:

- **`copy::record_copy_area_same_overlap`** is migrated to `record_paint_batch_op` via the `try_vk_copy_area` same-overlap arm. The unconditional pre-record `flush_if_needed(ProtocolBarrier)` that fired on every overlapping CopyArea (xterm scrollback, text-selection drag) is gone.
- **`CopyScratch::needs_grow(width, height) -> bool`** is the new public accessor that lets the migration site detect a pending image-replacement BEFORE entering the batch closure. Used in the same-overlap arm as a guard for the resize-only pre-flush.
- **Resize-only pre-flush** in `try_vk_copy_area` same-overlap arm: when `needs_grow` returns true, `flush_if_needed(BatchFlushReason::ProtocolBarrier)` fires BEFORE `scratch.ensure_size(...)`. This protects against the dangling-image hazard where `ensure_size`'s `queue_wait_idle + destroy_old_image` step doesn't wait for un-submitted batch CB commands. Rare path — fires only on the first same-overlap op whose size exceeds the current scratch.
- **`run_legacy_paint_op` audit catalogue** entry for `try_vk_copy_area (same-overlap)` updated to `migrated 3D (record_paint_batch_op, shared CopyScratch)`.

Out of scope, deferred to phase 3E:
- `text::record_text_run` (2 sites: `try_vk_text_run`, `try_vk_render_composite_glyphs`)
- `render::record_render_composite` (2 sites: `try_vk_render_traps_or_tris`, `try_vk_render_composite`)
- `MaskScratch::upload_r8` migration
- Glyph atlas incremental upload via batch CB

## Preflight checks

End of 3D (HEAD = `ccf60b1`):

- `cargo +nightly fmt --check` — no diff.
- `cargo clippy -p yserver` — 5 pre-existing `doc_lazy_continuation` warnings. No new warnings.
- `cargo test --workspace`:
  - `yserver` lib: **138 passed, 0 failed, 3 ignored**.
  - `yserver-core`: 284 passed.
  - `yserver-protocol`: 208 passed.
  - All other crates green.

## Cutover greps

```
$ rg -n 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs
```
ZERO hits inside `try_vk_copy_area`. Remaining sites: `run_legacy_paint_op` body, 3 readback handlers (`try_vk_get_image_pixels`, `hw_cursor_refresh`, `read_mirror_pixels`), 3E-deferred borrow-conflict fallbacks (text-run × 2, render-composite × 2), `open_with_commit`, `dump_scanout_one`.

```
$ rg -n 'flush_if_needed[(]BatchFlushReason::ProtocolBarrier' crates/yserver/src/kms/backend.rs
```
The OLD unconditional borrow-conflict fallback inside `try_vk_copy_area` same-overlap is gone. **ONE intentional resize-only pre-flush** remains in the same-overlap arm, gated on `CopyScratch::needs_grow(...)`. Other remaining sites: 4 text/render legacy fallbacks (3E-deferred), `run_legacy_paint_op` body, 5 drawable-destruction sites, 2 gradient-create sites.

```
$ rg -n 'record_paint_batch_op' crates/yserver/src/kms/backend.rs
```
≥ 3 call sites (3C T1 = `try_vk_put_image`, 3C T2 = `upload_bgra_to_mirror`, 3D = `try_vk_copy_area` same-overlap).

## Hardware smoke

Run on `silence` (AMD Polaris, dual-screen). The session itself was confounded by two unrelated bugs (see "Plan bugs caught" below), but the load-bearing 3D signals were clean:

- ✅ **No `vk copy same-overlap: pre-resize flush failed`** in `yserver-hw.log`. The CopyScratch resize-flush mitigation worked under real workloads.
- ✅ **No GPU driver fault, no `VM_CONTEXT` / `amdgpu` fault, no kernel oops.** Codex confirmed via journal scan: "The latest failure in journal.log is not the old AMD VM fault signature."
- ✅ **No `paint batch submit failed` / `renderer_failed` / `DEVICE_LOST`** in `yserver-hw.log`. The strict-flush failure path never fired.
- ✅ **xterm scrollback worked smoothly** (user report: "scrolling was totally fine"). This is the primary same-overlap workload.
- ⚠️ Caja icon-view scrolling was **choppy + high CPU load**. NOT a 3D regression — caused by (a) caja's per-icon damage rendering pattern, separate from same-overlap, and (b) GTK apps using XInput2 valuator-based smooth scroll which yserver doesn't yet emit. Filed as known issues, deferred.

## Plan bugs caught (folded back into plan / fixed in-tree)

### 1. Pre-existing P0: KMS teardown leaves Wayland host sessions broken

**Discovered via 3D smoke.** Not a 3D regression — predates the whole rendering re-architecture and was previously flagged at session-end as "disable_output atomic commit rejected with Invalid argument."

The user's normal session is `dms + labwc` (a Wayland compositor stack via QuickShell). When yserver exits on hardware, the kernel emits `atomic remove_fb failed with -22` from `drm_framebuffer_remove → drm_mode_rmfb_work_fn`, and the host compositor sees `qt.qpa.wayland: There are no outputs` / `WlrOutputService: Received empty outputs list`. The compositor can't recover. Reboot required.

Codex pinpoint of the bug: `KmsBackend::disable_output` at `backend.rs:7996` calls `ScanoutBoPool::drain_all_pending()`, which does `vkDeviceWaitIdle` and force-resets BO state to Free (`vk/scanout.rs:548`) — but does NOT wait for or drain kernel page-flip completion events. KMS still has FBs bound when yserver decides BOs are reusable. Then the atomic commit fails with EINVAL, and RAII later destroys framebuffers KMS still knows about.

Filed with full 6-step fix recipe in `docs/known-issues.md` ("P0: KMS teardown..."). This is the next high-priority piece of work — likely larger than a "3D-followup", probably its own short phase or folded into phase-4's general scratch lifetime rework.

X-based host sessions (Xorg + lightdm/MATE) survived because Xorg's startup runs a more aggressive DRM reset before grabbing outputs — the bug was there all along but Wayland hosts surface it.

### 2. Scroll-wheel libinput API misuse (from the inter-3C/3D scroll-wheel commit)

**Discovered via 3D smoke.** Not a 3D regression — introduced by the scroll-wheel support commit `b7d17a1` between 3C and 3D.

`yserver-hw.log` showed **flooding** of `libinput error: client bug: value requested for unset axis`. Root cause: my scroll handler in `crates/yserver/src/input/context.rs` unconditionally called `scroll_value_v120(Horizontal)` and `scroll_value_v120(Vertical)` for every wheel event. libinput requires checking `has_axis(axis)` first — a vertical-only wheel event has Horizontal unset, and querying it triggers the "client bug" error.

Functional behavior was unchanged (libinput returns 0 for unset axes anyway), but the log spam was substantial CPU + log-size waste.

**Fix**: `crates/yserver/src/input/context.rs` — gate each axis query on `ev.has_axis(axis)`; fall through to 0 when unset. Same fix applied to both `ScrollWheel` (uses `scroll_value_v120`) and `ScrollFinger`/`ScrollContinuous` (uses `scroll_value`) paths.

Landed at `56f93d9`.

### 3. Side observations from smoke (filed, not fixed in 3D)

- **Caja icon-view + gedit don't respond to mouse wheel scroll** (but caja list-view does). Pre-existing GTK behavior — those apps use XInput2 valuator-based smooth scroll (XI_Motion with valuators), not core X11 button-4/5 events. yserver only emits core button events from the scroll-wheel commit. Adding XI2 scroll-axis support is a larger separate piece of work; filed.
- **R↔B color swap on JPEG backgrounds** — already filed pre-3D in `docs/known-issues.md`.

## Done conditions

Per the plan v3's done conditions:

1. ✅ `cargo +nightly fmt --check` clean.
2. ✅ `cargo clippy -p yserver` 5 pre-existing warnings; no new ones.
3. ✅ `cargo test --workspace` green.
4. ✅ `try_vk_copy_area` same-overlap arm uses `record_paint_batch_op` with three disjoint field borrows (`scheduler` + mirror + scratch).
5. ✅ OLD unconditional `flush_if_needed(ProtocolBarrier)` borrow-conflict fallback at the top of the same-overlap arm is gone.
6. ✅ `CopyScratch::needs_grow` exists; same-overlap arm calls it BEFORE the mirror/scratch borrows and conditionally pre-flushes only on actual resize events.
7. ✅ Ordering invariant: pre-flush happens BEFORE `scratch.ensure_size(...)`.
8. ✅ `CopyScratch::ensure_size` is called BEFORE `record_paint_batch_op`.
9. ✅ Audit catalogue entry updated.
10. ✅ Hardware smoke: the 3D-specific signals are all clean. Confounders identified and isolated (libinput-spam fixed at `56f93d9`; KMS-teardown filed as P0 separate work).

## Commit summary (phase 3D)

| Task | Commit | Subject |
|---|---|---|
| Plan v1 | `e44f55f` | initial 3D plan (copy-same-overlap) |
| Plan v2 | `687b5d4` | fold codex's three tightening edits (extent fields, cutover greps, results doc wording) |
| Plan v3 | `8f31a1c` | fold codex's three followup tightenings (3C prerequisite, hardware-smoke required, ordering invariant) |
| T1 | `b8a8179` | migrate copy-same-overlap to record_paint_batch_op |
| Scroll-wheel libinput fix | `56f93d9` | check has_axis before scroll_value calls (inter-3D, surfaced by 3D smoke) |
| P0 disable_output known-issue | `ccf60b1` | file disable_output EINVAL as high-priority |
| T2 | this commit | docs(plans): phase-3D validation results |

7 commits since the 3C tip (`9a166b1`).

## Known deferred items

- **Text family** (3E): `text::record_text_run` × 2 sites, glyph-atlas incremental upload via batch CB.
- **Render-composite family** (3E): `render::record_render_composite` × 2 sites, `MaskScratch::upload_r8` migration.
- **`record_get_image`**: still on `run_one_shot_op` with `flush_if_needed(Readback)`. Phase 5 (targeted VkFence per HLD).
- **P0 KMS teardown** (next phase / blocker before 3E hardware testing): codex-pinpointed fix recipe in `docs/known-issues.md`. Must land before further hardware smoke runs to avoid breaking the user's daily-driver Wayland session.
- **XInput2 valuator-based scroll** (separate from 3D, surfaced by smoke): GTK apps in some modes don't respond to core button-4/5. Filed as known issue.
- **disable_output as a longer-term batch-adopt model**: the current CopyScratch resize-pre-flush is the 3D-only mitigation. Phase 4's sync rework should adopt the old scratch image into the open PaintBatch via `BatchResource` for deferred retirement instead — same model as the destruction barriers from 3B salvage.

## What's next

The P0 KMS teardown bug is the right next piece of work before phase 3E. Without it, hardware testing for 3E (which touches text + render-composite, both used heavily in any real session) will keep breaking the user's session. After P0 is fixed:

**Phase 3E** — text + render-composite family. Bigger than 3C/3D combined. Probably splits into two sub-phases (3E-text and 3E-render-composite) given each consumer has its own shared resource (glyph atlas + mask scratch / mask scratch + dst_readback). Re-plan via writing-plans skill + codex review loop, same as 3D.
