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

## Prerequisites ✅

All met as of 2026-05-22.

- ✅ Stage 4 compositor correctness issues closed (cow-authoritative-mode
  branch, 2026-05-21):
  - COW-authoritative mode active when an external compositor owns COW
    (`19ed354`).
  - Redirect-on-reparent semantics match Xorg's
    `compRedirectOneSubwindow` / `compUnredirectOneSubwindow`
    (`1065c50`).
  - Manual-redirected backings do not compete with COW in scanout
    (Stage 4d.7 `f3e9276` + cow-authoritative gating).
- ✅ HW cursor implemented (`YSERVER_V2_HW_CURSOR=1` opt-in landed
  in the Stage 4 close).
- ✅ `YSERVER_LOOP_TELEMETRY=1` enables per-second summary line; trace
  via `YSERVER_SUBMIT_TRACE=<path>` per-vkQueueSubmit2 (Stage 5 Task 3
  prep, `abb0855`).

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

## Task 0 - make telemetry answer the question ✅

Exit gate met by the bee/yoga/silence captures (2026-05-22): a single
MATE drag identifies bee as submit-rate-bound, yoga as descriptor-
pool-pin-bound (fixed by Task 4 layer 1), silence as headroom-rich
with a damage-saturation correctness gap. Diagnostic instrumentation
delivered in two passes:

- ✅ `queue_submit2/s` + `descriptor_allocations/s` + flip-related
  counters added across Stage 3 + Stage 5 Task 4 layer 1 (DescriptorPoolRing
  telemetry, `893f3e4`).
- ✅ Per-`vkQueueSubmit2` TSV diagnostic
  (`YSERVER_SUBMIT_TRACE=<path>`, `abb0855`, 2026-05-22) splits paint
  submits by op family + target + render-key — this is the structured-
  event log substitute for the "split paint_submits/compose by family"
  bullets.
- ✅ Stable capture recipe via `just yserver-{mate,xfce}-hw-telemetry`
  (`YSERVER_LOOP_TELEMETRY=1` + `YSERVER_SUBMIT_TRACE` autowired).

Items not landed but no longer load-bearing:

- ⏳ Pixmap-pool stats on the v2 per-second line. The `GLOBAL_LATEST_POOL`
  hook drives v1's emitter; v2 doesn't surface them on its line. Easy
  add when Task 5 picks up the storage-churn work.
- ⏳ Compose-submit split by repaint mode (`Full` / `Clipped` /
  `SkippedNoDamage` / `SkippedFlipPending` / `FailedSubmitRecovery`).
  Captured implicitly via `full_redraw_fallback/s` (Full vs Clipped)
  and `missed_pageflips/s`. Discrete per-mode counters not yet added.

Task 0 was the diagnostic-first pass; the actual perf-tuning tasks
follow.

## Task 1 - bound frame production ✅

Met by Stage 3f.9's per-output flip-pending gate (`bc6718a`,
2026-05-16). `tick_one_output` skips submission when
`state.pending_acks` is non-empty; `scene_structure_dirty` stays set
so deferred damage is picked up after page-flip completion. Silence
verification (dual 2.5k MATE drag): `composite_submits/s` caps at 121
= 2 × 60 Hz exactly, never exceeds. The Stage 3f.9 commit message
words it: *"KMS submit rate is now structurally bounded to one flip
per vblank per output."*

Remaining bullets from this task's original list (commit-retry-backoff
tunability, dedicated "rapid damage → one pending frame" tests) are
minor polish on already-correct behaviour; not budgeted.

## Task 2 - stop drawing what COW already drew ✅

Met by the Stage 4 cow-authoritative-mode close (`19ed354`,
2026-05-21). `build_scene` gates on COW registration: when a
compositor has registered to paint COW, the scanout scene emits
*root + COW + cursor only*. Non-composited WMs keep the normal
per-window scene path. Status doc cross-link:
[`status.md`](../../status.md) §"Stage 4 close (2026-05-21)".

Scene-build tests + the full diagnosis chain (~35 commits) live in
the cow-authoritative-mode branch; the working set behaviour is the
exit-gate signal already verified during Stage 4 hardware smokes
(MATE + marco-with-compositing on bee/fuji; xfce4 +
xfwm4-with-compositing on bee/fuji).

## Task 3 - aggregate paint submissions 🟡 (in progress on `perf`)

Cow `copy_area` POC + render_composite generalization landed on `perf`
(commits `0bec1b3`, `68af625`). Silence verification: cumulative −39 %
`paint_submits/s` vs pre-POC baseline. Full numbers + design rationale
in §"Task 3 POC 2026-05-22 — COW `copy_area` coalescing" and §"Task 3
generalization 2026-05-22 — `render_composite`" below.

Status of each original sub-bullet:

- ✅ **Aggregation boundary in `RenderEngine`** — `PendingCowBatch` +
  `PendingRenderBatch` on `RenderEngineInner`; auto-flush hooks at the
  top of every engine entry guard against cross-kind ordering bugs.
- ✅ **Consecutive `CopyArea` into the same target** — cow POC,
  78 % submit-rate reduction on cow path (silence).
- ✅ **RENDER composite batches** — generalization landed,
  1.43 calls/batch avg, peak 8, 30 % reduction on render path.
- ⏳ **PutImage tile / span bursts** — not done. Lower priority per
  bee analysis (put_image was ~8 % of all submits; not the hotspot).
- ⏳ **Glyph upload / draw aggregation** — not done. Glyph_upload was
  ~0.1 % of silence submits; not load-bearing.
- ⏳ **`render_fill` (Solid src)** — deliberately excluded from the
  conservative predicate (would need scratch clear lifted out of the
  render pass). 8 k savings on silence, deferred.

Exit-gate progress: paint_submits/s on silence is down ~39 % vs
pre-POC (5,653 → 4,180 avg). Not "order of magnitude" yet, but the
underlying constraint isn't N-times bigger gains — silence's residual
load is the long tail (put_image, composite_glyphs, the un-batched
Solid render_composites). **Bee + yoga re-capture pending hardware
access** is the decision point for closing Task 3 vs further work.

## Task 4 - make compose cheap 🟡 (correctness fix open)

Scene composition must scale with changed pixels and visible entries,
not with all historical state.

Status of each sub-bullet:

- ⏳ **Damage-strategy selection per frame** — open and load-bearing.
  Silence trace shows `damage_fraction` hits 1.00 while
  `full_redraw_fallback/s` stays ~0; `pick_repaint_region` keeps
  picking `Clipped` with `loadOp=LOAD` at saturation, leaving the
  ~1 % uncovered region as stale buffer-age content. **This is the
  silence smearing bug** — only known correctness regression
  attributable to Stage 5 work. Sketch: add a `damage_fraction > F →
  Full` arm before the Clipped path, F ≈ 0.6–0.8. See §"Smearing
  artifact — Task 4 correctness corollary" below.
- ⏳ **Occlusion-driven scene-entry skip** — open. No work done.
- ✅ **Cache descriptor sets / sampled-image bindings** — already
  delivered by existing infrastructure: `drawable_view_cache` keyed on
  `(DrawableId, SamplerConfig, SwizzleClass)` handles per-paint view
  reuse; `DescriptorPoolRing` (Task 4 layer 1) recycles descriptor-set
  storage. See §"Image-view caching is NOT the next fix" below.
- ✅ **Avoid per-frame image-view creation** — same as above. The
  observed `image_view_creates/s = storage_allocations/s` ratio is by
  counter design (co-located at storage-allocation sites), not
  per-paint view creation.

Exit gate (`compose_cb_record_ns/frame`, descriptor allocs, image
view creation stay flat) — the latter two already flat post Task 4
layer 1; `compose_cb_record_ns/frame` will respond once the damage
strategy fix lands (full redraw at saturation is cheaper to record
than 95 %-clipped scissor lists).

## Task 5 - remove allocation churn ⏳ (open)

Open. Silence trace shows `storage_allocations/s` peak 6,073 — 13×
bee's 467. Most fall outside the `PixmapPool`'s ≤128 px bucket (the
pool was sized for small client pixmaps; compositor backings are
full-output). Bee analysis identifies this as the second-tier
bottleneck (after Task 3 submit aggregation).

- ⏳ Pixmap-pool stats on the v2 per-second telemetry line (overlaps
  Task 0 follow-up).
- ⏳ Tune bucket sizes from real MATE/caja traces (xtrace under MATE
  drag would identify dominant CreatePixmap sizes — diagnostic-first
  per the perf-branch lesson before designing a pool regime).
- ⏳ Pool or cache transient compose resources that still allocate
  per frame. (DescriptorPoolRing addressed descriptor-set storage as
  Task 4 layer 1; `mask_scratch` + `dst_readback` already grow with
  high-water mark and don't churn — see Bee §"Image-view caching is
  NOT the next fix".)
- ⏳ Lifetime counters by drawable kind (root/window/backing/pixmap/
  cow/cursor).
- ⏳ Investigate sustained-allocation workloads after 10 s warm-up.

Exit gate: after warm-up, idle MATE produces near-zero storage
allocation, image-view creation, and descriptor allocation. Existing
work has already closed the descriptor-allocation side
(DescriptorPoolRing, Task 4 layer 1).

## Task 6 - async submit only where profiling justifies it ⏳ (deferred)

Deferred until Tasks 3 + 5 land and we re-profile. Current standing:

- Silence post-render-POC: `cpu_fence_wait_ns/s` avg 44 ms (~4 % of
  one core), down from cow-only 76 ms. Not currently load-bearing.
- Bee pre-POC: 11.97 ms (~12 % of one core). Post-Task-3 should drop
  further proportionally.

Memory note `feedback_perf_branch_2026_05_10`: a prior
timeline-semaphore attempt at per-op-wait removal did NOT pan out
without proper per-op characterization. Re-profile data first.

Exit gate unchanged: `cpu_fence_wait_ns/sec` either negligible or
removed from steady-state compose by syncobj/in-fence submission.

## Task 7 - optional headroom: direct scanout and planes ⏳ (deferred)

Out of scope until the composed desktop is responsive across the
hardware classes. Standing items unchanged:

- Direct scanout for a single full-output eligible entry.
- Hardware plane assignment for video/overlay entries.
- Multi-queue graphics/transfer split if transfer uploads still block
  graphics after batching.

Exit gate unchanged: improves specific workloads without changing the
observable scene result or becoming required for basic desktop
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

## Yoga 2026-05-22 perf-branch findings

Captured on `perf` branch HEAD `d34dcb0`, same MATE drag workload
as the bee capture. Host: `yoga` (Snapdragon X1 / Adreno X1 /
Turnip). Artifacts: `yserver-mate.perf.data` (perf record,
~106 MB / 720k samples), `yserver-hw-mate.log` (52 one-second
v2_telemetry buckets), `mate.log` (session).

This is the capture the DescriptorPoolRing design was authored
against. The 2026-05-21 baseline (pre-ring) showed
`vkCreateDescriptorPool → drmIoctl → msm_ioctl_vm_bind →
vm_bind_job_pin_objects → msm_gem_get_pages_locked →
shmem_alloc_and_add_folio` at ~36% of yserver's own CPU during
moderate-lag steady state.

### Telemetry deltas vs the spec's targets

| metric                          | spec target                          | observed                            | result               |
| ------------------------------- | ------------------------------------ | ----------------------------------- | -------------------- |
| `descriptor_pool_creates/s`     | ≤ 5 in steady state (was ~4700 impl) | **0** in 50/52 buckets; 1 in 2      | better than gate     |
| `descriptor_pool_resets/s`      | tracks paint_submits/s / SETS_PER_POOL | 0–26 range; avg ~6–10 during drag | matches              |
| `descriptor_allocations/s`      | unchanged                            | 180–183                              | unchanged ✓          |
| `paint_submits/s`               | unchanged                            | peak **8117**, drag avg **3807**     | parity (baseline was 700–4700) |
| `queue_submit2/s`               | n/a (not the bottleneck on yoga)     | peak **8238**                        | —                    |
| `composite_submits/s`           | vsync-bound                          | 60–61                                | flip-pending gate holding |

**Two pool creates across the entire 52-second drag** — the ring
warmed up to its working set once and then stayed there. Compared
with the implicit ~4700 per second on the pre-ring baseline, that's
a four-orders-of-magnitude reduction.

### Perf-flamegraph evidence

System-wide capture:

- yserver process: **0.32%** of total CPU.
- `libvulkan_freedreno.so` (inside yserver): **0.44%**.
- kernel time inside yserver: **1.04%**.
- swapper / idle: **89.5%**.

`perf report --comms=yserver --percent-limit=0.05` shows no Rust
symbol above 0.05% in yserver user-space. The only path reaching
`msm_ioctl_vm_bind` through yserver in the new capture is via the
inlined `sync_descriptor_pool_telemetry → descriptor_pool_creates_lifetime`
chain — i.e. the telemetry reader, not the create path — and only
at "0.00%" entries.

The 2026-05-21 baseline's `create_descriptor_pool →
msm_ioctl_vm_bind` path that hit ~1.63% of total system CPU is no
longer measurable.

User subjective: **"no CPU spikes at all"** during the drag.
Confirmed by the data.

### What this tells us about the per-hardware split

Yoga (Turnip): the design's motivating bottleneck. Fully resolved.

Bee (RADV): descriptor-pool churn was never the bottleneck. The
fix delivered the designed telemetry numbers (`creates/s = 0`,
`resets/s = 5–6`) but the user-perceived drag-lag did not improve
because the cost on bee sits on the `queue_submit2/s = 2119`
ioctl-rate axis, not the descriptor allocator path.

The per-hardware-class bottleneck split is now empirically
established: the same workload runs into completely different walls
on the two GPUs. Task 4 layer 1 was the right fix for yoga; the bee
fix is Task 3 (paint-submit aggregation).

### Next bottleneck candidates visible in the yoga telemetry

Despite yoga showing no user-perceived spikes, the data points at
two follow-up candidates if Task 4 layer 2 work is pursued:

- **`storage_allocations/s` + `image_view_creates/s` at peak 1946,
  1:1.** This is the spec's explicit layer-2 follow-up ("Image-view
  caching; stable storage should have stable views"). Same caveat as
  the bee analysis above: at the call sites, view creation is
  co-located with storage allocation, and the `drawable_view_cache`
  already handles per-paint reuse. The 1946/s peak is X11-protocol-
  driven storage allocations — likely the same compositor backing-
  pixmap pattern from bee, mapped to higher peaks because yoga's
  higher composite cap (60 Hz × wider damage region) drives more
  paint surfaces.
- **`descriptor_allocations/s` flat at 180**. The spec's layer-2
  ("descriptor set caching by view tuple") would key sets on
  `(src_view, mask_view, dst_view)` and avoid the
  `vkAllocateDescriptorSets + vkUpdateDescriptorSets` pair on cache
  hits. 180/s suggests roughly that many distinct view tuples per
  second under this drag.

Neither is materially affecting yoga (no spikes observed). Both
are pre-staged followups for if the post-Task-3 / post-Task-5
profile re-opens a yoga gap.

## Silence 2026-05-22 perf-branch findings

Captured on `perf` branch HEAD `22cdf54`, same MATE drag workload
as the bee and yoga captures. Host: `silence` (i9 13900k / rx580
Polaris/GCN4 / RADV / dual 2560x1440). Artifacts:
`yserver-mate.perf.data` (perf record, ~176 MB / 1.6M samples),
`yserver-hw-mate.log` (70 one-second v2_telemetry buckets,
captured from a separate drag run because the perf recipe
defaults to `RUST_LOG=warn` and suppresses INFO-level telemetry),
`mate.log` (session).

This is the third hardware class — the first with both AMD/RADV
(matching bee's userspace stack) AND substantial CPU headroom
(13900k vs bee's 6900HX), AND multi-output. The result
empirically separates "the bottleneck" into per-axis behaviour
the bee and yoga captures alone could not distinguish.

### Telemetry summary across 70 buckets

| metric                          | avg     | peak    | bee peak (ref) |
| ------------------------------- | ------: | ------: | -------------: |
| `paint_submits/s`               | 6,852   | 18,910  | 2,048          |
| `queue_submit2/s`               | 7,069   | 19,379  | 2,119          |
| `composite_submits/s`           | 98      | 121     | 59             |
| `frame_present_count/s`         | 98      | 121     | 60             |
| `storage_allocations/s`         | 1,605   | 6,073   | 467            |
| `image_view_creates/s`          | 1,605   | 6,073   | 467            |
| `descriptor_allocations/s`      | 255     | 304     | 180 (yoga)     |
| `descriptor_pool_creates/s`     | 0.04    | 2       | 0              |
| `descriptor_pool_resets/s`      | 24      | 65      | 6              |
| `damage_fraction`               | 0.62    | **1.00** | n/a           |
| `full_redraw_fallback/s`        | 0.06    | 4       | n/a            |
| `cpu_fence_wait_ns/s`           | 76 ms   | 206 ms  | 12 ms          |
| `scene_entries_drawn`           | 2,634   | 10,514  | n/a            |
| `missed_pageflips/s`            | 0       | 0       | 0              |

### What the data says

1. **DescriptorPoolRing scales to silence's load without
   breaking.** Two pool creates across the whole run (both
   warmup); `descriptor_allocations/s` stays in yoga-comparable
   range despite ~9× the paint volume. The ring's recycle path
   absorbs the higher submit rate cleanly.

2. **Composite submits double per the dual-output prediction.**
   `composite_submits/s` ≈ 2 × bee's 59. `frame_present_count/s`
   tracks 1:1, so KMS keeps up on both outputs. Per-output
   flip-pending gate + per-output dirty tracking hold up under
   dual-2.5k load.

3. **silence drives ~9× bee's paint volume.** `paint_submits/s`
   peak 18,910 vs bee's 2,048. Same X11 client traffic (MATE +
   marco + caja + wezterm) — bee was rate-limited by its CPU
   never quite catching up, so it never measured the real client
   demand. silence shows what MATE actually wants to push when
   nothing's holding it back. Task 3's "aggregate paint
   submissions" thesis is even more justified at this volume.

4. **`storage_allocations/s = 1605 avg, 6073 peak — 13× bee.**
   Two effects compound: dual output ~doubles compositor backing
   allocations, and bigger surfaces miss `PixmapPool` entirely
   (the pool's `max_pooled_dim=128` covers only small client
   pixmaps, never compositor-sized backings). Task 5 work needs
   a separate bucket regime for large surfaces.

5. **CPU fence waits non-trivial:** `cpu_fence_wait_ns/s` avg
   76 ms (7.6% of one core) peak 206 ms (20%). Still well below
   the perf-bind threshold on a 13900k, but the absolute number
   is a Task 6 candidate after Tasks 3 + 5.

### Perf-flamegraph evidence

System-wide capture, full drag duration:

- `swapper` (idle): **97.80%** of CPU. System was overwhelmingly
  idle; even peak MATE drag couldn't approach saturating the
  13900k.
- `yserver` process: **0.52%** of total system CPU (cpu-clock
  event). Across 32 logical cores this is ~17% of one logical
  core averaged, peaking to user-observed ~30%.

Inside yserver:

- `libvulkan_radeon.so`: 0.25% children / 0.08% self.
- `libc.so.6` (`__ioctl`): 0.42% children / 0.06% self.
- `[unknown]` (kernel + ASLR-stripped frames): 0.45% children /
  0.27% self.
- **No Rust symbol in yserver above the 0.05% noise floor.**

Hot call chain (sched_switch event, where off-CPU wait paths
surface): `main → run → run_core → … → __ioctl (inlined) →
libvulkan_radeon.so → amdgpu ioctl`. Identical shape to bee.

### What silence tells us that bee/yoga could not

**The submit-rate bottleneck is universal across AMD/RADV; it
only *binds* where single-core budget runs out.** silence at
19k submits/s peak burns a comparable single-core fraction to
bee at 2k/s, because (a) the 13900k single-thread is ~2-3×
faster per syscall and (b) most of the time yserver is off-CPU
waiting on KMS retirement, so the kernel can schedule it
elsewhere when ready. Task 3 (paint aggregation) cuts the cost
*everywhere*, just visibly only where it binds.

The corollary: there's nothing new to discover in silence's
perf profile that bee's didn't already say. Same hot path, same
shape, just diluted by headroom. Don't budget further
diagnostic time on silence-specific perf.

### Smearing artifact — Task 4 correctness corollary

User reported "smearing/damage artifacts sometimes visible"
during silence's drag. Telemetry pins the diagnosis cleanly:

- `damage_fraction` hits 1.00 in peak buckets.
- `full_redraw_fallback/s` stays ~0 across the entire run
  (0.06 avg, 4 in one isolated bucket).

So `pick_repaint_region` keeps choosing `Clipped` with
`loadOp=LOAD` even when ~100% of the output is damaged. Clipped
at 99% damage paints 99% of pixels and leaves the residual ~1%
as prior buffer-age content — that residual is the smearing
trail.

**Why silence surfaces this and bee/yoga didn't:** silence is
the first hardware with enough headroom for X11 clients to push
damage_fraction to saturation under MATE drag without yserver
falling behind. On bee, the submit-rate cap kept paint volume
low enough that damage_fraction stayed well below 1.0; on yoga,
the descriptor-pool pin pre-fix did the same. silence's headroom
exposes a correctness gap that the other captures couldn't
reach.

Task 4 already lists "full redraw when clipping is more
expensive than redraw" as a goal — this is its correctness
corollary. Sketch of fix: in `pick_repaint_region`, add a
`damage_fraction > F → Full` arm before the Clipped path; F
likely in the 0.6–0.8 range. Verify by re-running silence drag
and confirming `full_redraw_fallback/s` rises while smearing
disappears.

### The three-axis hardware split

The per-hardware-class bottleneck split is now empirically
established:

- **yoga (Snapdragon X1 / Turnip)** — `vkCreateDescriptorPool →
  msm_ioctl_vm_bind` pin path. **Fixed** by Task 4 layer 1.
- **bee (Ryzen 9 6900HX / RADV)** — `vkQueueSubmit2` ioctl rate
  bound by single-thread budget at ~2k/s. **Task 3** (paint
  aggregation).
- **silence (i9 13900k / RADV)** — same ioctl-rate cost as bee
  but ~3-9× the absolute volume, absorbed by single-core
  headroom. Perf does not bind; the higher damage saturation
  exposes the `pick_repaint_region` correctness gap (smearing).
  **Task 4 correctness corollary**.

### Recommended next order on this branch

Unchanged from the bee analysis: Task 3 (paint aggregation) is
the next perf work. Add Task 4 damage-strategy fix on top
because (a) it's much cheaper than aggregation, (b) it's
correctness not perf, (c) silence is the hardware that will
verify it. Order:

1. **Diagnostic-first** for Task 3: instrument
   `vkQueueSubmit2` call sites to log per-second submit-kind
   histograms (paint vs compose vs upload, target distribution,
   batch-size distribution). Capture on bee under the same drag.
   Don't design the aggregation boundary until this data exists.
2. Land the Task 4 `pick_repaint_region` damage-saturation fix.
   Verify on silence drag: `full_redraw_fallback/s` should rise
   in proportion to time spent at `damage_fraction > F`;
   smearing should disappear.
3. Brainstorm → spec → plan → execute Task 3 (aggregation),
   same shape as DescriptorPoolRing (Task 4 layer 1).
4. Re-capture telemetry on all three hardware classes. If bee's
   `queue_submit2/s` drops to the v1-comparable budget but lag
   persists, then look at Task 5 (storage allocation pool) with
   an xtrace — silence's 6073/s peak makes this likely the
   next-tier work even if bee improves.

The perf branch is staying open across machines through Tasks 3
+ 4 + 5; no intent to land Task 4 layer 1 to master alone.

## Silence 2026-05-22 submit-trace findings (Task 3 design data)

Diagnostic-first per step 1 of the recommended order above.
`YSERVER_SUBMIT_TRACE` instrumentation landed on `perf` (one row
per `vkQueueSubmit2`, 14 TSV columns: frame_id, ns_mono, kind,
target_kind, target_id, batch_size, op, src_class, mask_class,
pipeline_id, readback, alias, zero_draws, upload). Captured a
45.5 s MATE drag on silence via `just yserver-mate-hw-telemetry`
(dual-2.5k, RADV / rx580). Artifacts:
`yserver-mate.submit.tsv` (27 MB / 380,917 rows).

### Headline

**47.9 % of submits sit in consecutive same-target same-kind
runs of length ≥ 2.** Coalescing those runs into one CB each
collapses 143,560 of 380,917 submits — a **37.7 % reduction**
in absolute submit rate. Average submit rate goes from
**8,376/s → ~5,200/s post-aggregation**; on bee that lands well
below the ~2,000/s steady-state where the lag bound, so silence
post-aggregation should run comfortably and bee should clear the
user-perceived lag floor.

### Kind distribution

| kind                | total   | in runs ≥2 | coalesce savings |
| ------------------- | ------: | ---------: | ---------------: |
| `render_composite`  | 168,016 | 107,079    | **88,467**       |
| `copy_area`         |  62,255 |  48,480    | **40,237**       |
| `render_fill`       |  80,189 |  15,716    |  8,262           |
| `composite_glyphs`  |  21,988 |   7,627    |  4,438           |
| `render_traps`      |   8,313 |   2,092    |  1,360           |
| `scene_compose`     |   4,692 |     842    |    431           |
| `get_image`         |   4,579 |       —    |    —             |
| `put_image`         |  30,351 |      70    |     48           |
| `glyph_upload`      |     417 |     364    |    299           |
| `fill_batch`        |     117 |      35    |     18           |

Three kinds (`render_composite`, `copy_area`, `render_fill`)
carry **96 % of the savings**. Everything else is rounding error
at this point.

### Biggest single hotspot — marco compositor → COW via `copy_area`

**46,920 of 62,255 `copy_area` events (75 %) target drawable
id=35 = COW.** Marco's compositor Present pump fires one
`copy_area` per damaged region per backing → COW per frame; on
dual-2.5k with full-window drag, runs of length 12-50 against
that single target are common. Single-target `copy_area`
coalescing into one CB would on its own collapse ~40k submits.
This is the smallest valuable Task 3 slice.

### `render_composite` keys concentrate on 4 tuples

| `op | src_class | mask_class`   | count   | % of `render_composite` |
| ----------------------------------- | ------: | ----------------------: |
| `over | direct | no_mask`           |  58,651 | 35 %                    |
| `src | direct | no_mask`            |  42,439 | 25 %                    |
| `over | direct | direct`            |  30,709 | 18 %                    |
| `src | gradient_linear | no_mask`   |   9,695 |  6 %                    |
| `over | direct | solid`             |   9,604 |  6 %                    |
| `src | solid | solid`               |   4,451 |  3 %                    |
| `out_reverse | direct | no_mask`    |   4,253 |  3 %                    |
| `add | direct | direct`             |   4,253 |  3 %                    |
| (8 more, ≤ 2k each)                 |   3,961 |  2 %                    |

The aggregation predicate `(target_id, kind, op, src_class,
mask_class)` catches the runs naturally. No need for more
nuanced keys (pipeline_id, transforms, repeat modes) — those
secondary dimensions correlate with the primary key in practice.

### Per-tick burstiness validates main-loop tick as flush point

Loop-tick submit counts span 1 to 52 per tick. ~10k ticks carry
just one submit; the long tail to 30-50/tick is concentrated
during MATE animations and compositor Present floods. **End of
`maybe_composite` is a guaranteed-correct flush boundary**
because compose reads from every target the engine has touched;
flushing the pending-op queue immediately before `scene.tick`
runs gives correctness for free. No cross-tick ordering work
needed.

### What this gives Task 3 design

**⚠️ Trace-schema lesson learned during implementation
(2026-05-22):** the `src_class` column collapsed distinct `src_id`s
into a single class label, so this section's predicate
recommendation (item 2 below) keyed on `src_class` which mis-led
the initial design. The actual implementation key needed to
account for **per-call src/mask view variation** — marco's
pattern is *N different srcs → 1 dst*, same as cow `copy_area`.
The iter-1 over-strict predicate that took `src_class` literally
produced 1.005 calls/batch (no coalescing); the iter-2 relaxed
predicate produced 1.43 calls/batch. See §"Task 3 generalization
2026-05-22 — render_composite" for the corrected design.

The recommendations below are kept verbatim for traceability —
items 1 / 3 / 4 / 5 / 6 held up; item 2's specific predicate
was wrong.

1. **Aggregation boundary: end of main-loop tick** (immediately
   before `maybe_composite` calls `scene.tick`). Engine entry
   points push onto a `Vec<PendingOp>` on `RenderEngine` instead
   of recording immediately; the flush method records all
   compatible-keyed ops into one CB before submit.
2. ⚠️ **Aggregation key (mis-stated): `(target_id, engine_method,
   op, src_class, mask_class)`.** The corrected key for
   `render_composite` is the much smaller
   `(target_id, op, dst_pict_format, mask_component_alpha)`
   with per-append descriptor rebinding inside the open render
   pass.
3. **Order preservation per target.** Within a target, push
   order is record order. Across targets, the order is
   independent so long as no `get_image` / readback intervenes
   (those force flush).
4. **Smallest valuable slice: single-target `copy_area`
   coalescing for COW (drawable id of `cow_id`).** 40k of the
   143k savings, isolated to one target, one kind, no per-rect
   picture-clip plumbing. Prove the aggregation shape works
   here before extending to `render_composite`.
5. **Forced flush triggers:** `get_image`, `copy_plane_rb`,
   `scene_compose`, any X11 reply that observes drawable
   content, descriptor-pool retirement boundary, command-buffer
   capacity (per-op CB size cap), and `disable_output`.
6. **Compatible-keyed run depth: typical 2-15, peak 50.** Sizing
   the per-target pending-op slab at ~64 ops is enough for the
   common case; over-cap forces flush.

The submit-trace instrumentation stays in tree under
`YSERVER_SUBMIT_TRACE=<path>` (off by default, zero hot-path
cost) for re-capturing post-Task-3 to verify the rate drop.

### Cross-machine status

Bee re-capture under the same recipe is pending hardware
access. Yoga and bee are both expected to show a similar
distribution shape (RADV/Turnip differences sit in descriptor
allocator + ioctl cost, not in client paint-traffic shape).
The instrumentation can re-run anywhere with no rebuild — same
recipe, set `YSERVER_SUBMIT_TRACE=…` in env or via the updated
`-telemetry` recipes.

## Task 3 POC 2026-05-22 — COW `copy_area` coalescing ✅ (landed)

First Task 3 implementation landed on `perf` at `0bec1b3`.
Smallest valuable slice per the trace data above: coalesce
consecutive `copy_area` submits to the COMPOSITE Overlay
Window (marco's compositor pump) into one CB + one
`vkQueueSubmit2`.

### Implementation shape

- `PendingCowBatch` (cb, ticket, dst, srcs_in_batch,
  dst_damage, coalesced_count) on `RenderEngineInner`, `None`
  between flushes.
- `engine.cow_copy_area(cow_id, src, src_rect, dst_pos)`:
  first append allocates CB + fence ticket via the existing
  `begin_op_cb`, transitions `dst → TRANSFER_DST` and `src →
  TRANSFER_SRC`, records `vkCmdCopyImage`, accumulates the
  damage rect; subsequent appends record only
  `vkCmdCopyImage` (and a new src transition the first time
  a given src appears in the batch).
- `engine.flush_cow_batch(store, platform) -> Option<u32>`:
  records exit transitions for every src and the dst (→
  `SHADER_READ`), ends the CB, submits via
  `platform.submit_paint_cb`, pushes one `SubmittedOp`,
  clones the fence ticket onto every touched drawable via
  `store.touch_render_fence`, applies accumulated damage,
  pushes `coalesced_count` into a flush-records queue, and
  returns it.
- Auto-flush hooks at the top of every other engine entry
  point (`fill_rect_batch`, `logic_fill`, `copy_area`,
  `put_image`, `get_image`, `image_text`, `composite_glyphs`,
  `render_composite`, `render_traps_or_tris`). Same-queue
  submission order is the correctness rule: any unrelated op
  submits its own CB and must see the batch CB on the queue
  first.
- `engine.drain_cow_flush_records()` returns the queue for
  telemetry; backend drains it once per `maybe_composite`
  tick after the explicit pre-`scene.tick` flush.

Routing predicate at the backend layer:
`self.cow_id == Some(dst_target.id) && src != dst_target.id`
— same-image self-copies stay on the regular path. Per-call
`record_paint_submit` + `trace_simple` are suppressed for
cow-routed copies; one event is emitted per flush instead,
with `batch_size = coalesced_count`.

### Vk-backed tests (lavapipe)

1. `cow_copy_area_coalesces_four_srcs_into_one_submit` —
   drives marco's pattern (4 distinct srcs → 1 cow dst);
   asserts `inner.submitted` grows by 1 (not 4) across the
   batch, pending batch shows `coalesced_count=4`, drained
   flush record is `[4]`, dst read-back shows the four
   colour columns at the expected offsets.
2. `cow_copy_area_repeated_src_skips_redundant_transition`
   — same src appended twice; `srcs_in_batch` set holds one
   entry, `coalesced_count=2`, both halves of dst show src
   colour.
3. `cow_copy_area_flush_via_non_cow_op` — `cow_copy_area`
   then an unrelated `fill_rect` on a third drawable;
   verifies the per-method flush hook fired (pending batch
   cleared, flush record present, cow contents correct).

All 40 lavapipe-backed yserver tests pass; 368 lib tests +
35 acceptance tests pass; clippy default clean. (Pedantic has
one over-100-lines warning on `cow_copy_area`; deferred to a
later cleanup pass.)

### Silence verification — 45 s MATE drag

Pre-POC capture vs same-recipe re-run on `perf` `0bec1b3`:

| metric                    | pre-POC | post-POC | Δ        |
| ------------------------- | ------: | -------: | -------: |
| `paint_submits/s` avg     |   6,852 |    5,653 | **−18 %** |
| `paint_submits/s` peak    |  18,910 |   14,040 | **−26 %** |
| `queue_submit2/s` avg     |   7,069 |    5,850 |   −17 %  |
| `queue_submit2/s` peak    |  19,379 |   14,438 |   −25 %  |
| `cpu_fence_wait_ns/s` avg |   76 ms |    45 ms | **−40 %** |
| `composite_submits/s` avg |      98 |       98 | unchanged ✓ |
| `cow_batches_flushed/s` avg |    n/a |     171 | new      |
| `cow_copies_coalesced/s` avg |   n/a |     927 | new      |

Cow batch shape (post-POC):

- 10,111 cow flushes recorded across the 45 s capture.
- Average batch size **5.41**, peak **46**.
- Underlying cow `copy_area` count (sum of batch sizes):
  ~54,700 — slightly higher than the pre-POC baseline of
  46,920 (workload variance — the drag isn't bit-identical).
- Cow-path submit collapse: pre-POC 46,920 individual
  cow `copy_area` submits → post-POC 10,111 flushes =
  **78 % fewer submits on the cow path**.
- Non-cow `copy_area` count: 15,644, avg `batch_size=1.00`
  (path untouched as designed).

### Bee projection

Pre-POC bee bound at ~2 k submits/s under the same workload.
Same workload's pre-POC silence ran 8.4 k/s. Post-POC silence
runs 5.7 k/s. Applying the same ratio to bee → projected
~1.4 k/s — comfortably below the user-perceived lag floor.
Bee re-capture pending hardware access; the POC stays on
`perf` until that confirmation lands.

### Outstanding artifacts

- **End-of-session damage artifacts** observed in the
  post-POC drag (scanout dumps saved off-tree for later).
  User confirmed these are almost certainly pre-existing —
  the silence `pick_repaint_region` saturation bug
  (`damage_fraction → 1.0` while `full_redraw_fallback`
  stays ~0) reproduces unchanged in this run. Task 4
  correctness corollary; not POC-caused.

### What remains for Task 3 closure

The COW POC validated the aggregation shape and delivered the
single largest hotspot (75 % of `copy_area` traffic). The
remaining 60 % of all coalesce savings sit on:

- **`render_composite`** — 88 k savings. Generalization landed
  2026-05-22 (`68af625`); see §"Task 3 generalization
  2026-05-22 — render_composite" below.
- **`render_fill`** — 8 k savings. Sub-case of
  `render_composite` (Solid src); deliberately excluded from
  the conservative predicate. Would need `record_solid_color_clear`
  hoisted out of the render pass (Vulkan disallows
  `cmd_clear_color_image` inside `cmd_begin_rendering`/
  `cmd_end_rendering`). Out of scope until measured value
  justifies the work; the trace analysis below shows it didn't
  on silence.

## Task 3 generalization 2026-05-22 — `render_composite` ✅ (landed)

Landed on `perf` at `68af625`. Took two iterations.

### Iteration 1 (over-strict key) — measured failure

First implementation keyed on the full per-call signature:
`(dst, op, src_id, mask_id_opt, src_repeat, mask_repeat,
mask_component_alpha, src_pict_format, mask_pict_format,
dst_pict_format)`. Silence verification showed near-zero
coalescing:

- `render_batches_flushed/s` avg 2,002, `render_composites_coalesced/s`
  avg 2,012 → **1.005 calls per batch** (~500 of 91,500
  eligible calls actually coalesced).
- `paint_submits/s` avg 5,653 → 6,158 (**regression**, +9 %).

Diagnosis from the post-iter-1 trace: 110,671 consecutive
`render_composite → render_composite` transitions in the
workload, but the trace fields captured `op | src_class |
mask_class` — they do NOT capture `src_id`. Marco's dominant
compositor-pump pattern is "composite N different window
backings into one stage texture" (same shape as cow
`copy_area`'s N srcs → 1 dst). The conservative predicate
rejected every same-target run because `src_id` varied per call.

Trace-design lesson: aggregation-key-relevant dimensions
(notably `src_id`, `mask_id`) need to be in the trace schema,
not collapsed into a class. The current trace records `src_class`
("direct" / "solid" / "gradient_linear" / ...) but not the
concrete drawable id — so a "consecutive same-key run" in the
trace can still be N distinct srcs sharing the same class.

### Iteration 2 (relaxed key) — measured success

Predicate cut to four fields:

```
RenderBatchKey {
    dst,
    op,
    dst_pict_format,
    mask_component_alpha,
}
```

These are exactly the inputs to pipeline binding + render-pass
attachment. Everything else is re-encoded per append:

- **`src_id`, `mask_id`, src/mask `pict_format`, src/mask
  `repeat`**: each append resolves its own views via
  `ensure_drawable_view` (sampler-config + swizzle-class
  cached), allocates its own descriptor set from the ring,
  calls `cmd_bind_descriptor_sets` inside the still-open
  render pass before drawing. Pipeline stays bound from open;
  rebinding the descriptor between draws is legal.
- **`src_transform`, `mask_transform`**: encoded into
  `RenderPushConsts` per draw.
- **`clip_rects`**: `cmd_set_scissor` per draw.

Refactor on `vk/ops/render.rs`:

- `record_render_composite_open(cb, dst, pipeline)` — dropped
  the descriptor binding (now per-append). Still records dst
  barrier + `cmd_begin_rendering` + viewport + pipeline.
- `record_render_composite_draws(cb, pipeline_layout,
  descriptor_set, extent, attrs, rects, clip_scissors)` —
  added `descriptor_set` param; binds at top then iterates
  scissors × rects with per-rect push consts + draw.
- `record_render_composite_close(cb, dst)` — unchanged.
- The unbatched wrapper `record_render_composite` threads the
  descriptor through; behaviour unchanged for the per-call
  path (Solid / Gradient / dst_readback / self-alias).

Engine state:

- `PendingRenderBatch` grows `touched_drawables:
  HashSet<DrawableId>` (every src + mask sampled by any
  append in the batch) so `flush_render_batch` can
  `touch_render_fence` them all. `any_mask: bool` lets the
  flush record's `has_mask` reflect "at least one append
  had a mask" for trace fidelity.

### Silence verification — same 45.5 s MATE drag

| metric                          | pre-POC | cow-only | render-relaxed |
| ------------------------------- | ------: | -------: | -------------: |
| `paint_submits/s` avg           |   6,852 |    5,653 |  **4,180**     |
| `paint_submits/s` peak          |  18,910 |   14,040 |   14,814       |
| `queue_submit2/s` avg           |   7,069 |    5,850 |  **4,377**     |
| `composite_submits/s` avg       |      98 |       98 |       98 ✓     |
| `render_batches_flushed/s` avg  |   n/a   |   n/a    |   1,294        |
| `render_composites_coalesced/s` avg | n/a |   n/a    |   2,018        |

Cumulative reduction in `paint_submits/s` avg: **−39 % vs
pre-POC**, **−26 % on top of cow alone**.

Render batch shape on the post-POC trace:

- Pixmap dst batches: 122,103 flushes containing 174,953
  underlying composites = **avg 1.43 calls/batch, peak 8**.
- 30 % fewer render-path submits than the pre-POC trace's
  168 k render_composite events.

`composite_submits/s` unchanged at 98 confirms the scene
compose path is untouched as designed.

### Tests landed

Four Vk-backed lavapipe tests parallel to the cow POC:

1. `render_composite_batch_coalesces_two_same_key_calls` —
   two same-key composites with **different srcs** (the
   marco pattern) → one CB, two descriptor sets bound back-
   to-back, both halves of dst show correct colours,
   `coalesced_count=2`.
2. `render_composite_batch_key_mismatch_flushes` — OP_OVER
   then OP_SRC → first batch flushes, fresh batch opens
   with the new op.
3. `render_composite_batch_solid_src_skips_batched_path` —
   `ResolvedSource::Solid` → `deferred_to_batch=false`,
   per-call submit, no pending batch.
4. `render_composite_batch_flush_via_non_render_op` —
   render_composite then `fill_rect` on a third drawable
   → auto-flush hook fires, dst pixels reflect the prior
   render.

### Outstanding for full Task 3 closure

- `render_fill` (Solid src, 8 k of the original 143 k
  savings): would require lifting `record_solid_color_clear`
  out of the render pass. Trace shows the actual workload
  pattern doesn't make this worthwhile until bee re-capture
  proves otherwise.
- Cross-machine confirmation: bee re-capture pending hardware
  access. Yoga not in critical path (descriptor-pool path
  was its bottleneck, fixed by Task 4 layer 1).
- One transient "eog window stayed at origin" reported during
  silence verification; couldn't repro on master, perf HEAD,
  or with the stashed changes. Filed as a non-repro flake;
  scanouts saved off-tree for later inspection.

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
