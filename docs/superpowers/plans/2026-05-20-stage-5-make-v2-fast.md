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

## Bee 2026-05-22 perf-branch findings

Captured on `perf` branch HEAD `85d5ce7` (DescriptorPoolRing landed
through Stage 5 Task 4 layer 1 commits `fb058a6..e12a559`). Host:
`bee` (Ryzen 9 6900HX / RDNA2 / RADV). Workload: MATE desktop, drag
of a wezterm + caja window. Artifacts:
`yserver-mate.perf.data` (perf record), `yserver-hw-mate.log`
(v2_telemetry lines).

### What the ring fix delivered

`descriptor_pool_creates/s = 0` and `descriptor_pool_resets/s = 5-6`
throughout the drag, confirming the ring recycles as designed. The
old `vkCreateDescriptorPool → msm_ioctl_vm_bind` hot path that
dominated yserver CPU on yoga/Turnip is fully gone. On bee, the
equivalent path was never material in the first place — RADV doesn't
shmem-pin pool memory the way Turnip does.

### Where the bee drag CPU actually goes

Telemetry snapshot at drag peak (2026-05-22 06:05:59):

```
paint_submits/s       = 2048
queue_submit2/s       = 2119
composite_submits/s   = 59       (one per frame at 60 Hz — fine)
cpu_fence_wait_ns/s   = 11.97 ms (37 waits/s)
storage_allocations/s = 467
image_view_creates/s  = 467
descriptor_pool_creates/s = 0    ← ring fix working
descriptor_pool_resets/s  = 6    ← ring recycling
avg_compose_cb_record_ns  = 478,374
```

`perf report` against the same drag confirms yserver's user-space
hot path is entirely flat (no Rust symbol above 0.05%); the cost
sits in `libc.so:ioctl → libvulkan_radeon → amdgpu` — i.e. the
syscall round-trip of ~2k `vkQueueSubmit2/s`. Aggregate yserver CPU
is 4.26% of 16 logical cores ≈ ~70% of one core averaged, pegging
one core during burst.

### Image-view caching is NOT the next fix

Investigated 2026-05-22 (this session). Findings:

- `record_image_view_create` is co-located with
  `record_storage_allocation` at all 5 backend sites
  (`backend.rs:979, 2386, 4689, 4805, 8114`). The 1:1 ratio is by
  counter design, not redundant view creation.
- The 5 sites are all X11-protocol-driven *storage* allocations
  (`init_root_storage`, `allocate_window_storage`,
  `get_overlay_window`, `create_pixmap`, DRI3 import). Each is a
  first-time create.
- The existing `drawable_view_cache: HashMap<(DrawableId,
  SamplerConfig, SwizzleClass), CachedDrawableView>` on
  `RenderEngineInner` (engine.rs:~4084 + the helper that inserts
  into it) already handles per-paint sampling reuse. It is not
  being missed.
- `mask_scratch` and `dst_readback` grow via
  `next_power_of_two().max(256)` with a high-water mark — they
  don't churn per paint either.

So the Task 4 sub-bullet "Avoid per-frame image-view creation"
isn't a real follow-up on bee; the existing caching already
delivers it. Leave the bullet in place for completeness but do
not budget work for it.

### The two real next bottlenecks on bee

1. **`queue_submit2/s = 2119` — Task 3 (aggregate paint
   submissions).** This is what `perf report` shows as the syscall
   hot path. 35 submits per frame at 60 Hz means each paint op
   becomes its own command-buffer + `vkQueueSubmit2` + amdgpu
   ioctl. Coalescing compatible ops (same target, same pipeline,
   compatible operator/source class, no readback between them)
   into single CBs should cut submits an order of magnitude. Note
   `feedback_perf_branch_2026_05_10` memory: an earlier
   timeline-semaphore attempt at per-op-wait removal did NOT pan
   out. Approach: profile what the 2k submits carry (op kinds,
   target distribution, batch-eligibility) BEFORE designing
   aggregation. Don't repeat the timeline-semaphore mistake of
   architectural change without per-op characterization.

2. **`storage_allocations/s = 467` — Task 5 (remove allocation
   churn).** Each is a fresh X11 pixmap (CreatePixmap /
   NameWindowPixmap / DRI3 import). Likely marco's compositor
   backing-pixmap pattern. Xorg pools pixmap memory; yserver
   currently doesn't. An xtrace under the same drag would identify
   which X11 request types dominate the 467/s — cheap diagnostic
   before designing a pool. If a small set of compositor-driven
   sizes recur, a per-size storage pool buys most of the win.

`cpu_fence_wait_ns/s = 11.97 ms` (~12% of one core) is a third-tier
concern; Task 6 covers it. Don't touch until Tasks 3 and 5 land.

### Recommended order on this branch

1. **Diagnostic-first** for Task 3: instrument
   `vkQueueSubmit2` call sites to log per-second submit-kind
   histograms (paint vs compose vs upload, target distribution,
   batch-size distribution). Cheap. Don't design the aggregation
   boundary until this data exists.
2. **Brainstorm → spec → plan → execute** Task 3 (aggregation),
   same shape as DescriptorPoolRing (Task 4 layer 1).
3. Re-capture telemetry + perf on bee. If `queue_submit2/s` drops
   to the v1-comparable budget but lag persists, then look at
   Task 5 (storage allocation pool) with an xtrace.

The perf branch is staying open across machines for this work; no
intent to land Task 4 layer 1 to master yet.

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
