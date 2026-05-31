# Findings: RENDER source-picture redirect resolution — partial fix, loop NOT fixed

**Date:** 2026-05-31
**Parked branch:** `fix/applet` (10 commits + 1 WIP-stash commit; pushed to origin).
**Plan executed:** `docs/superpowers/plans/2026-05-31-render-source-picture-redirect.md` (lives on the parked branch).

## TL;DR

We landed a sound, well-tested fix for a real asymmetry in the v2 KMS backend's RENDER source-picture path. On yoga (X1E / Turnip), it does NOT resolve either symptom:

- **Tray icons still invisible.**
- **Per-vsync DAMAGE loop still firing** (~5% reduction, well within noise).

The diagnosis was incomplete. The source-picture redirect path is genuinely asymmetric vs Xorg, but it isn't the load-bearing driver of either symptom on yoga. The Risk Register on the original plan predicted this outcome as one of two possibilities and named the likely follow-up (ClipByChildren on dst paint paths) — that hypothesis is now the most likely next direction.

bee was not retested. The earlier xtrace baselines were captured on bee. Yoga reproducer is hotter (~2x request rate) and was the post-fix test platform.

## What we tried

Across 10 commits on `fix/applet` (base `e7199e4`, tip `7bb5e90` before the WIP stash):

| Phase | Change | Test surface |
|---|---|---|
| 1 | New `KmsBackendV2::resolve_source_picture` helper — wraps `resolve_picture_for_render` and routes `PictureRecord::Drawable` sources through `resolve_paint_target`, returning `(ResolvedSource, …, offset)`. | 4 unit tests covering redirected/descendant/unredirected/non-Drawable branches. |
| 2 | `render_composite` source + mask paths use the new helper; offsets applied to `CompositeRect.src_x/src_y/mask_x/mask_y`. | src + mask coverage tests; client-clip translation audit (confirmed-correct without modification). |
| 3 | `render_traps_or_tris` engine signature gained `src_offset: (i32, i32)`. Trapezoids and triangles call sites both routed through the new helper. | 2 tests per op (resolves-to-backing + captures src_offset). |
| 4 | Tray-shape integration test exercising the systray applet pattern end-to-end via recorder-based assertions (Vk-null fixture has no pixel readback). | 1 test asserting `src_id == backing_id` post-resolve plus storage-share via `resolve_paint_target`. |

Each commit is atomic and individually compiles + tests pass (verified across 10 sequential builds, 402 → 413 monotonic).

## Why we thought this was the bug

The plan was driven by codex-assisted analysis of two xtrace captures (`mate.xtrace` from yserver/bee, `mate-xorg.xtrace` from a reference Xorg session):

1. The mate-panel notification-area-applet uses `RedirectWindow(proxy, Manual)` then reads `picture(proxy)` to composite icon content.
2. `crates/yserver/src/kms/v2/backend.rs:6174` `resolve_picture_for_render` for `PictureRecord::Drawable` returned `store.lookup(host_xid)` — the leaf's `DrawableId`. Under Manual redirect the leaf has empty content; the icon's pixels live in the backing.
3. Destination side of the same code (`resolve_dst_picture_for_render` at `:6240`) explicitly routes through `KmsBackendV2::resolve_paint_target` to walk the redirect. The source-side analog was missing.

Hypothesis: this asymmetry meant the applet was compositing from empty leaf storage → icons invisible, then `FillRectangles op=Clear` on the same picture damaged the backing → DAMAGE-Notify → applet wakes → 60 Hz loop.

The hypothesis explains both symptoms with a single defect, which is why it looked load-bearing. It also matches the Xorg model precisely (`xserver/composite/compwindow.c:121-152` `compSetPixmap` propagates the backing pixmap to non-redirected descendants).

## What the test on yoga showed

Pre-fix baseline (yoga, 22:28) vs post-fix (yoga, 23:38):

| Metric | Pre-fix | Post-fix | Δ |
|---|---|---|---|
| Trace span | 14.66 s | 12.24 s | — |
| Total systray requests / s | 1338 | 1270 | −5% |
| RENDER Composite / s | 260 | 246 | −5% |
| XFIXES CreateRegion / s | 175 | 164 | −6% |
| XFIXES DestroyRegion / s | 175 | 164 | −6% |
| DAMAGE Subtract / s | 175 | 164 | −6% |
| RENDER CreatePicture / s | 173 | 163 | −6% |
| SYNC / s | 87 | 82 | −6% |
| yserver damage_notify_queue / s | ~300 | 213 | −29% |

The 5-6% drop on the applet-side metrics is well within trace-duration variance. The damage_notify_queue drop is larger (-29%) — interesting but not load-bearing for the user-visible cycle. The per-vsync `DAMAGE Subtract → CreateRegion → Composite → SYNC` loop is unchanged in shape and rate.

**Tray icons remained invisible** post-fix on yoga.

## Why the fix didn't deliver

Two not-mutually-exclusive explanations:

1. **ClipByChildren on dst is the load-bearing missing piece.** The plan's Risk Register named this as a separate concern. The applet's `FillRectangles op=Clear` on `picture(proxy)` writes to the backing every cycle. Xorg's default `subwindow-mode = ClipByChildren` (`xserver/render/picture.c:719`) makes that Clear a no-op when children fully cover the proxy — the icon's pixels in the backing survive each frame. yserver's RENDER path doesn't apply that clip; the Clear wipes the icon pixels every cycle, the applet receives DAMAGE-Notify from the legitimate write, and the loop continues regardless of which side of the picture we fix.

2. **Something upstream of source resolution is driving the loop.** The yoga-side post-fix analysis observation: "GTK is still looping at the same rate, so whatever drives the loop is upstream of the source-picture path." If the loop trigger is NOT the applet's own Clear (option 1) but instead spurious DAMAGE-Notify emission from yserver — e.g. on ConfigureWindow restacks the applet does, or on Map/Unmap dance for XEMBED handshakes — neither the source fix nor ClipByChildren will quiet it.

The yoga-Claude analysis (verbatim from the test session):

> Looking at the commits that landed, these are all RENDER-side fixes — walking the redirect chain when resolving source/mask pictures during Composite/Triangles. So this patch fixed how Composite resolves source pictures, not what causes the applet to enter the per-vsync loop in the first place. If GTK was previously sampling from wrong/stale content and looping until it "saw" what it wanted, fixing source resolution might have shown improvement — but on yoga, GTK is still looping at the same rate, so whatever drives the loop is upstream of the source-picture path.

We don't have bee-side post-fix data to compare. If bee (RDNA2 / RADV) quiets and yoga doesn't, the cause is GPU-driver-specific — same shape as the implicit-dmabuf-sync split in the project memory.

## Why the icons are still invisible — even more concerning

The plan's load-bearing claim was that source resolution to the leaf produced empty composite output. The integration test (`tray_pattern_composite_from_redirected_proxy_reads_child_pixels` in commit `7bb5e90`) asserts `src_id == backing_id` post-fix and was revert-verified — it FAILS if the source-resolve swap is reverted. So the resolution IS correctly routing to the backing in tests.

But on yoga, icons remain invisible. Possibilities:

- **The icon clients aren't writing to the backing.** `resolve_paint_target` on the icon's host xid should walk up to the redirected proxy and return the backing's `DrawableId`. If the actual icon clients hit a different paint path (one that bypasses `resolve_paint_target` or routes through engine code we didn't audit), their pixels don't reach the backing. Worth re-running `RUST_LOG="yserver::kms::v2::paint=trace"` and grepping for `NO_REDIRECT_FOUND` on the icon clients' host xids.
- **The backing is being read correctly but its content is wrong.** The fix swapped which `DrawableId` the engine receives, but the engine's actual sampling could still be misconfigured on yoga (Turnip-specific pipeline state, BLIT vs SAMPLE path differences, etc.). The Vk-null test fixture can't catch this.
- **The backing's allocation/layout is wrong on yoga.** Image-layout transitions, format mismatches, or staging-buffer paths that don't fire on RADV but do on Turnip.

## What's the most likely next direction

In rough order of cheapness × signal:

1. **bee post-fix capture.** 5-minute task on bee. If bee quiets and yoga doesn't, we have a GPU-driver split and the remaining work focuses on yoga-specific paint paths. If both still loop, ClipByChildren is the next plan.
2. **Visual check on bee.** If icons render on bee post-fix but not yoga, this confirms the source fix delivered its job on at least one HW and pushes investigation to yoga's paint pipeline.
3. **`YSERVER_DAMAGE_BACKTRACE=1` capture on yoga.** The diagnostic block is still on the parked branch's WIP commit. Re-run and look at the post-fix backtraces — what's emitting the per-vsync damage now? If it's still `process_request.rs:1622` (the RENDER FillRectangles emit on the proxy), ClipByChildren is the seam. If it's something else, the diagnosis pivots again.
4. **ClipByChildren plan.** Picture-level subwindow-mode clipping in v2's RENDER paint paths (FillRectangles, Composite, etc.). Substantial — touches the same call sites as Phase 2 of this branch but on the dst side, gated by the source picture's `subwindow_mode` field.

## What to keep / what to drop

- **Keep the parked branch.** The source-picture redirect asymmetry IS a real bug per the Xorg spec; even if it's not load-bearing for the current symptom, a future workload could surface it. The 12 new tests + recorder infrastructure are reusable for a ClipByChildren follow-up.
- **Don't merge to master.** The user-facing outcome wasn't delivered. Merging code that doesn't solve the reported problem creates the impression of progress that isn't there and complicates future bisection.
- **Update `project_tray_damage_self_loop` memory record.** Note this fix attempt, its outcome, and the most likely next direction so a future session lands faster.

## Process notes

- Codex review (after the plan was drafted) flagged the ClipByChildren concern explicitly. We documented it in the Risk Register but didn't prioritize it because the diagnosis painted the source-picture fix as a single-defect-fixes-both-symptoms shape. That diagnosis was wrong in retrospect. **Process lesson:** when a diagnosis explains multiple symptoms with one fix, it's worth weighting *more* skeptically rather than less. Single-defect explanations are clean but rarely true for long-standing bugs.
- Subagent-driven execution worked well — 10 commits in one session with spec + code review on each. The bottleneck was diagnosis, not implementation.
- The integration test couldn't catch this regression because Vk-null fixtures don't readback pixels. A hardware-loop test framework (mate session under `just yserver-mate-hw-trace`, asserted against damage-event rate) would have caught the "fix doesn't actually help" outcome in CI; without it, hw smoke is the load-bearing check.
