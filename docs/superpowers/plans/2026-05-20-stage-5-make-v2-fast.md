# Stage 5 - make v2 fast

Drafted 2026-05-20 after HW cursor reintroduction moved out of the
critical path. This replaces "Stage 5 = HW cursor" as the active v2
Stage 5 plan. The HW cursor plan remains as historical design context in
`2026-05-19-stage-5-hw-cursor.md`.

## Goal

Make `KmsBackendV2` fast enough to be the default rendering model on
non-ancient hardware.

The target is not "some optimizations landed." The target is measured
interactive headroom:

- 60+ fps sustained for normal desktop interactions on fuji-class Intel
  and bee-class AMD hardware.
- No catastrophic-lag mode under MATE / GTK redraw, window drag,
  terminal scroll, or compositor-present workloads.
- v2 within a small measured budget of v1 where v1 is correct, and
  measurably better where v1's model forces waste or incorrect output.
- No steady-state `vkQueueWaitIdle`; no unbounded queue-submit,
  allocation, descriptor, or repaint-pixel rates.

## Non-goals

- Do not rewrite the v2 model. Stage 5 uses `PlatformBackend`,
  `DrawableStore`, `RenderEngine`, and `SceneCompositor` as-is.
- Do not hide performance bugs behind unconditional full-screen redraw.
  Full redraw remains a strategy choice when cheaper than fragmented
  clipping, not the default escape hatch.
- Do not tune by feel. Every task either adds telemetry or changes a
  named counter under a named workload.
- Do not make direct scanout / hardware planes a prerequisite for a
  responsive desktop. They are later headroom work after the composed
  path is healthy.

## Prerequisites

- Stage 4 compositor correctness issues are closed enough that perf
  captures represent the intended output:
  - COW-authoritative mode active when an external compositor owns COW.
  - Redirect-on-reparent semantics match Xorg for embedded tray applets.
  - Manual-redirected backings do not compete with COW in the scanout
    scene.
- HW cursor is implemented and validated. Pointer motion latency is no
  longer allowed to mask compositor or paint submit-rate problems.
- `YSERVER_LOOP_TELEMETRY=1` is available and stable enough to compare
  runs.

## Success gates

Capture each workload on both v1 and v2, same machine, same session
recipe, same theme, same output mode. Stage 5 closes only when v2 meets
the gates below and the results are written to `docs/status.md`.

| Workload | Hardware | Gate |
|---|---|---|
| MATE desktop idle, compositor off | fuji, bee | `frame_present/sec` near 0 when no damage; no repaint churn from idle applets |
| MATE desktop idle, compositor on | fuji, bee | COW presents drive repaint; no extra per-window scene overdraw above COW |
| xterm drag under non-composited WM | fuji, bee | sustained >=59 `frame_present/sec`; no flip-pending backlog growth |
| terminal scroll / text expose | fuji | `paint_submits/sec` and `queue_submit2/sec` <= 2x v1 baseline |
| mate-control-center GTK redraw | bee | no catastrophic lag; `queue_submit2/sec`, descriptor allocs, and repaint pixels bounded |
| caja desktop icons visible | fuji, bee | no post-icon idle repaint storm; tray/app icons remain visible |
| compositor shadow / panel scenario | yoga, fuji, bee | COW path stays correct; perf counters do not regress relative to compositor-off baseline by more than the expected COW present cost |

Required counters for every capture:

- `frame_present/sec`
- `paint_submit/sec`
- `composite_submit/sec`
- `queue_submit2/sec`
- `vk_queue_wait_idle/sec`
- `cpu_fence_wait_ns/sec`
- `compose_cb_record_ns/frame`
- `gpu_render_ns/frame`
- `damage_pixels/sec`
- `damage_fraction`
- `full_redraw_fallback/sec`
- `scene_entries/frame`
- `descriptor_allocations/sec`
- `image_view_create/sec`
- `storage_allocation/sec`
- pixmap-pool hit/miss totals
- flip-pending / commit-retry counters

## Task 0 - make telemetry answer the question

Before optimizing, make the performance cliff observable in one log.

- Add missing v2 telemetry counters for `queue_submit2/sec`,
  descriptor allocation rate, compose descriptor-set reuse/miss, repaint
  region count, repaint pixels, and flip-pending skips.
- Split paint submits by operation family: fill, copy, put-image,
  RENDER composite, glyph, trap/tri, readback.
- Split compose submits by output and by repaint mode:
  `Full`, `Clipped`, `SkippedNoDamage`, `SkippedFlipPending`,
  `FailedSubmitRecovery`.
- Emit pixmap-pool stats on the v2 telemetry line, not only through the
  older global hook.
- Add a stable capture recipe under `docs/` with exact commands,
  environment, machine name, theme, compositor state, and duration.

Exit gate: a single 60 s MATE run explains whether v2 is CPU-bound in
scene build, GPU-bound in compose, submit-bound, allocation-bound, or
waking/repainting without useful damage.

## Task 1 - bound frame production

The compositor must not manufacture work faster than KMS can consume it.

- Audit every `wake_for_damage` and `maybe_composite` path for repeated
  wakes while a flip is already pending.
- Convert "rapid damage while flip pending" into one coalesced pending
  repaint per output.
- Make the hardcoded commit retry backoff observable and tunable.
- Add tests that repeated damage while flip-pending produces one pending
  frame, not N queued compose attempts.

Exit gate: under rapid pointer/window-drag damage, v2's attempted
compose rate is bounded by page-flip retirement plus one coalesced
pending repaint.

## Task 2 - stop drawing what COW already drew

When a compositor is active, v2 should not run a second compositor on
top of the compositor's output.

- Treat COW as authoritative for redirected top-level content.
- In compositor-active mode, strip normal redirected top-level scene
  entries from scanout. The scanout scene is root background, COW,
  server-owned overlays that must remain outside COW, and cursor.
- Keep non-composited WMs on the normal per-window scene path.
- Pin this with a scene-build test: caja desktop, mate-panel,
  tray applets, and a compositor COW must not produce duplicated
  per-window layers above COW.

Exit gate: compositor-on scene entry count is small and stable; COW
pixels are not occluded by caja/mate-panel/manual backings.

## Task 3 - aggregate paint submissions

The composed path cannot fly if X11 paint traffic maps to one Vulkan
submit per small operation.

- Introduce a v2 paint aggregation boundary in `RenderEngine`: collect
  compatible operations for the same target until a protocol barrier,
  readback, layout hazard, target switch requiring strict ordering, or
  event-loop flush.
- Keep the existing per-op path for readback and rare strict barriers.
- Extend current stroke aggregation beyond solid fills:
  - consecutive `CopyArea` into the same target,
  - PutImage tile / span bursts,
  - RENDER composite batches sharing pipeline, target, source class,
    mask class, and operator,
  - glyph uploads / glyph draws where atlas lifetime allows it.
- Preserve X11 ordering. If an operation can be observed by a later
  readback or external client, flush before the observation.

Exit gate: high-volume paint workloads reduce `queue_submit2/sec` and
`paint_submit/sec` by an order of magnitude or hit the v1-comparable
budget in the success table.

## Task 4 - make compose cheap

Scene composition must scale with changed pixels and visible entries,
not with all historical state.

- Add damage-strategy selection per frame:
  - clipped repaint for small/medium compact regions,
  - full redraw when clipping is more expensive than redraw,
  - structure-damage full fallback only when the retained BO history is
    invalid.
- Add occlusion-driven scene-entry skip for fully covered opaque
  entries. This is especially important for desktop windows and panel
  rectangles.
- Cache descriptor sets / sampled-image bindings for stable
  `DrawableId`s. Reuse across frames until storage generation changes.
- Avoid per-frame image-view creation; stable storage should have stable
  views.

Exit gate: `compose_cb_record_ns/frame`, descriptor allocations, and
image view creation stay flat under idle and bounded under window drag.

## Task 5 - remove allocation churn

Allocation spikes produce the "fast until a desktop thing appears"
failure mode.

- Make the v2 pixmap pool visible in telemetry and tune bucket sizes
  from real MATE/caja traces.
- Pool or cache transient compose resources that still allocate per
  frame.
- Add lifetime counters for drawable storage create/destroy by kind:
  root, window, redirected backing, pixmap, COW, cursor.
- Investigate any workload with sustained storage allocation after the
  first 10 s warm-up.

Exit gate: after warm-up, idle MATE compositor-on/off produces near-zero
storage allocation, image-view creation, and descriptor allocation.

## Task 6 - async submit only where profiling justifies it

CPU fence waits should not be on the hot path, but replacing them with
syncobj plumbing before the submit rate is fixed just hides the real
problem.

- Re-profile after Tasks 1-5.
- If CPU fence wait remains material, add DRM in-fence / syncobj
  submission for compose without changing `SceneCompositor` semantics.
- Keep `VkFence` retirement as fallback for drivers without the needed
  syncobj path.

Exit gate: `cpu_fence_wait_ns/sec` is either negligible or removed from
steady-state compose by syncobj/in-fence submission.

## Task 7 - optional headroom: direct scanout and planes

Only after the composed desktop is responsive:

- Direct scanout for a single full-output eligible entry.
- Hardware plane assignment for video/overlay entries.
- Multi-queue graphics/transfer split if transfer uploads still block
  graphics after batching.

Exit gate: these improve specific workloads without changing the
observable scene result and without becoming required for basic desktop
responsiveness.

## Close protocol

For each task:

- Land one measurable change at a time.
- Run `cargo +nightly fmt`.
- Run `cargo clippy -p yserver`.
- Run relevant unit / acceptance tests.
- Capture at least one before/after telemetry run on the workload the
  task claims to improve.
- Update `docs/status.md` with the measured result, not just the patch
  description.

Stage 5 is closed when the success gates pass on fuji and bee, with yoga
used as a low-power sanity check for regressions.
