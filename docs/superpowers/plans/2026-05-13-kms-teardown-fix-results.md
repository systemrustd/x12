# KMS teardown fix — validation results

- **Date:** 2026-05-13
- **Plan:** [`2026-05-13-kms-teardown-fix.md`](2026-05-13-kms-teardown-fix.md) (v4)
- **Branch:** `graphics-followups`
- **Predecessor:** phase 3D results doc (`47f213f docs(plans): phase-3D validation results + sharpen P0 teardown entry`)

## Scope landed

The KMS teardown plan replaced a single-shot shutdown that destroyed
framebuffers while KMS still held them with an explicit 6-step
sequence plus a failure-path disarm so a partially-failed teardown
no longer cascades into kernel `remove_fb` warnings (and the host
Wayland session can recover).

Concretely, the following pieces landed:

- `KmsBackend::shutting_down` gate — prevents new composites from
  being submitted once shutdown starts.
- `ScanoutBoPool::has_pending_pageflip` predicate — exposes
  per-output pageflip state to the drain helper without lying BOs
  back to `Free`.
- `KmsBackend::drain_pending_pageflips_for_shutdown` — non-blocking
  poll loop that retires inflight pageflips per output before the
  atomic disable.
- `KmsBackend::disable_output` 6-step rewrite — (1) stop submissions,
  (2) flush/retire paint batch, (3) wait for Vulkan work,
  (4) drain pageflips, (5) atomic-disable outputs, (6) drop scanout
  resources (only after KMS is quiesced). On atomic-disable failure
  the path branches to disarm rather than force-reset.
- `ScanoutBo::disarm` + `Drop` early-return — flagged scanout BOs
  leak their FB/GEM/Vk handles to DRM-fd close instead of being
  destroyed while KMS may still hold them.
- `drm::buffer::Buffer::disarm` + `Drop` early-return — same
  treatment on the non-Vulkan dumb-buffer scanout path; added in
  `b41ac38` after codex caught that disarming only `ScanoutBo` left
  the dumb-swapchain path exposed.
- `Swapchain::disarm` — fan-out to per-buffer `Buffer::disarm`.
- `KmsBackend::disarm_scanout_pool` helper — marks all scanout BOs
  belonging to an output as disarmed when its atomic disable fails.

## Preflight checks

```text
$ cargo +nightly fmt --check
(no diff)

$ cargo clippy -p yserver 2>&1 | tail
warning: `yserver-core` (lib) generated 2 warnings
warning: `yserver` (lib) generated 5 warnings
```

The 5 yserver-lib warnings are all pre-existing `doc_lazy_continuation`;
T1–T3 introduced none. The 2 yserver-core warnings are also pre-existing.

```text
$ cargo test --workspace
test result: ok. 138 passed; 0 failed; 3 ignored        (yserver lib)
test result: ok. 9 passed; 0 failed; 0 ignored          (yserver-core)
test result: ok. 284 passed; 0 failed; 0 ignored        (yserver-protocol)
test result: ok. 208 passed; 0 failed; 0 ignored        (integration)
test result: ok. 2 passed; 0 failed; 1 ignored          (other suites)
```

No new unit tests were added — per the plan's codex-flagged caution
against performative tests for this kind of plumbing change. Hardware
smoke is the meaningful validation here.

## Hardware smoke results

**User workflow:** dms+labwc Wayland session running on F1; switch
to F3 (`Ctrl+Alt+F3`); run yserver on F3; quit yserver; switch back
to F1 (`Ctrl+Alt+F1`). (Note: yserver itself does not handle VT
switching, so the user can't switch away from F3 while it's
running.)

**Outcome:** dms+labwc session recovered cleanly. User report:
> "well, at least my DMS session came back"

**Kernel WARN is gone.** `grep -i remove_fb /home/jos/Projects/yserver/journal.log`
returns zero hits across the F3 yserver session. Before the fix this
was the smoking gun (`atomic remove_fb failed with -22` from
`drm_framebuffer_remove → drm_mode_rmfb_work_fn`) that left the
host compositor's outputs in an unrecoverable state.

**Disarm path activated.** `yserver-hw.log` records:

- 2× `disable_output failed for DP-1/HDMI-A-1: ... Invalid argument`
- 1× top-level `yserver: disable_output failed`
- 4× `drm Buffer disarmed (atomic disable_output failed); leaking
   FB/dumb to be reaped by DRM-fd close`
- 6× `ScanoutBo disarmed (atomic disable_output failed); leaking
   FB/GEM/Vk to be reaped by DRM-fd close`

Total = 10 disarmed handles across 2 outputs. These are the
framebuffers that would previously have been destroyed mid-teardown
and triggered the kernel WARN; they now leak cleanly until process
exit closes the DRM fd.

**Residual:** atomic `disable_output` itself still returns `EINVAL`
on both outputs. This is a separate bug in the modeset payload at
`crates/yserver/src/drm/modeset.rs:387` — filed as its own
known-issues entry alongside the P0 closure. Harmless given the
disarm path, but a follow-up will split the single atomic commit
into per-stage commits the kernel accepts.

## Done conditions

From the plan:

1. ✅ `shutting_down` gate prevents new composite submissions.
2. ✅ Paint batch flushed/retired before pageflip drain.
3. ✅ Vulkan work waited on before pageflip drain.
4. ✅ `drain_pending_pageflips_for_shutdown` retires inflight pageflips
   without force-resetting BO state.
5. ✅ Atomic disable issued after KMS is quiesced.
6. ✅ Scanout resources dropped only after disable (or disarmed on
   disable failure).
7. ✅ `ScanoutBo::disarm` + `Drop` early-return implemented.
8. ✅ `Buffer::disarm` + `Drop` early-return implemented for the
   dumb-buffer scanout path (gap-close in `b41ac38`).
9. ✅ Workspace builds clean, fmt clean, clippy clean of new
   warnings, all tests pass.
10. ✅ with caveat — hardware smoke recovers the host Wayland
    session and eliminates the kernel WARN. Caveat: atomic
    `disable_output` still EINVALs, but the disarm path absorbs
    that failure; dms+labwc session reclaims the outputs cleanly
    after VT-switch back from F3.

## Plan bugs caught (folded back into plan)

- **v1 → v2** (`d2d7786`): codex flagged the poll-loop race
  (blocking read of DRM events vs. async retirement) and the
  unconditional step-6 force-reset on failed disables. Both folded.
- **v2 → v3** (`1e2db1f`): codex flagged that simply "skip
  `drain_all_pending` on failed disable" wasn't enough — RAII
  `Drop` on `ScanoutBo` would still destroy FBs on failed outputs.
  Folded the `disarmed` flag + `disarm_scanout_pool` helper into v3.
- **v3 → v4** (`a811709`): codex flagged stale unit-test references
  (deleted during earlier folds) and `Drop`-comment wording. Folded.
- **After T2 landed** (`b41ac38`): codex caught the dumb-buffer
  `Buffer::Drop` gap — same destroy-while-KMS-holds bug class on
  the non-Vulkan scanout path. Fixed with mirror `disarm` +
  `Swapchain::disarm` fan-out.

## Commit summary

| Step | Commit | Subject |
|---|---|---|
| Plan v1+v2 | `d2d7786` | docs(plans): KMS teardown fix plan — fold codex's two blocking findings |
| Plan v3 | `1e2db1f` | docs(plans): KMS teardown fix plan v3 — fold codex's v2 followup findings |
| Plan v4 | `a811709` | docs(plans): KMS teardown fix plan v4 — codex v3 doc-consistency fixes |
| T1 | `6ef0a66` | infra: `shutting_down`, `ScanoutBo::disarm`, `has_pending_pageflip`, drain helper, `disarm_scanout_pool` |
| T1 doc fix | `95ff02a` | reattach hijacked doc-comments |
| T2 | `308ba2f` | 6-step `disable_output` rewrite |
| T2 cleanup | `837bfdb` | drop stale `#[allow(dead_code)]` |
| Dumb-buffer gap | `b41ac38` | also disarm dumb-swapchain `Buffer`s on failed disable |
| T3 (this) | _this commit_ | docs(plans): KMS teardown fix validation results |

## Known deferred items

- **Atomic `disable_output` EINVAL on AMD Polaris** — filed as a
  separate known-issues entry (see `docs/known-issues.md`). Fix
  recipe: split the single atomic commit into per-stage commits
  (plane clear, CRTC deactivate + MODE_ID blob unset, connector
  clear), each with `ALLOW_MODESET`.
- **Phase 3E hardware testing** (text + render-composite + traps +
  MaskScratch + glyph atlas) is unblocked again now that yserver
  exits no longer poison the host session.

## What's next

Phase 3E. The user can iterate on hardware again without rebooting
between runs.
