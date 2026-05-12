# Paint → composite → flip sync rework — design

Date: 2026-05-12
Status: revised (v5) after codex review round 4
Author: jos + claude session

## Background

After the COMPOSITE redirect work (L1+L2) landed on master, two
related symptoms on the KMS backend remained:

1. **GPU saturates on paint bursts.** Opening wezterm, resizing
   uxterm, or hovering rows in mate-control-center spikes GPU
   activity to 100% on Polaris (RX 580). On UMA hardware
   (Renoir, Skylake) the spike is shorter but still visible. The
   user-visible effect on Polaris was severe pointer lag in
   mate-control-center — documented in commit `2d357cc`
   (`feat(kms): DRM hardware cursor plane`) which worked around
   the symptom by routing cursor motion via legacy
   `drmModeMoveCursor` ioctls, bypassing the composite cadence
   entirely. That introduced cursor artefacts on amdgpu DCE
   (Polaris) because legacy and atomic cursor paths don't
   interop cleanly on that display generation.

2. **Dual-output flicker under burst paint.** On Polaris with
   two 2560×1440 outputs (5120×1440 logical), the same paint
   bursts produce whole-frame flicker — visibly stale or
   partial content reaching the screen. Single-output on the
   same machine is flicker-free under the same load. The flicker
   correlates with paint-rate, not idle motion.

The diagnosed root cause for (1) is the **per-paint-op
`vkQueueWaitIdle`** scattered across the Vulkan paint pipeline.
Each Vulkan submission drains the entire GPU queue and blocks
the CPU thread until completion. With N small ops per frame
(GTK hover fanout, terminal scroll, control-center repaint),
the cumulative serialisation is N × queue-drain × submit-
latency. On UMA the round-trip is microseconds; on discrete
PCIe-attached GPUs each round-trip costs more. The HW cursor
plane workaround shifted cursor motion off this path but didn't
fix the path itself.

The diagnosis for (2) is **provisional**. A likely contributor
is the per-output `vk_flip_pending` skip in `composite_and_flip`
(`backend.rs:6212..6243`): when burst paint pushes one output
to keep a pending flip alive longer than the other, the per-
output skip can drift the two outputs into different frame
ages. The sync rework reduces the rate at which this happens,
but does not provably eliminate it; flicker reduction is
**plausible**, not **expected**, and gets re-verified after
sync rework lands (P6 below).

## Current state — concrete observations

These are the load-bearing facts. Sources inline.

- **Queue drain surface.** `vkQueueWaitIdle` is invoked from
  many more sites than the per-op paint recorders. Per a `rg`
  pass across `crates/yserver/src/kms/vk/`:
  - `vk/ops/mod.rs:59..184` — the central per-op drain (the
    canonical one referenced in the symptom diagnosis).
  - `vk/glyph.rs:444, 460` — glyph atlas upload + sampler
    teardown drain.
  - `vk/dst_readback.rs:105, 264` — host readback for GetImage
    / capture tests.
  - `vk/target.rs:735` — target image clear / initialisation.
  - `vk/pipeline.rs:314`, `vk/render_pipeline.rs:510, 652`,
    `vk/text_pipeline.rs:329`, `vk/logic_fill_pipeline.rs:137`
    — pipeline-cache teardown / shader rebuilds.
  - `vk/copy_scratch.rs:76, 137`, `vk/mask_scratch.rs:110,
    133, 239`, `vk/gradient.rs:250, 522` — scratch buffer /
    image teardown after one-shot use.
  Each of these has a **lifetime reason** for draining. The
  paint recorder drain is "guard the next op against in-flight
  writes." The scratch drains are "this resource is about to
  be reused/freed; ensure the GPU's done with it." The
  readback drains are "host wants to read what the GPU just
  wrote." Classifying each is part of P1, not part of "remove
  waitIdle everywhere blindly."
- **Vulkan API level.** `crates/yserver/src/kms/vk/device.rs:54`
  requests `vk::API_VERSION_1_3`. Vulkan 1.3 makes timeline
  semaphores (`VK_KHR_timeline_semaphore` promoted to core 1.2)
  and `synchronization2` (`VK_KHR_synchronization2` promoted to
  core 1.3) available unconditionally on every conformant
  driver.
- **External semaphore export.** `device.rs` already enables
  `VK_KHR_external_semaphore_fd` (line 152) and the device
  exports SYNC_FDs for `IN_FENCE_FD` handoff to KMS — see
  `crates/yserver/src/kms/vk/scanout.rs:55..211` and
  `crates/yserver/src/kms/vk/compositor.rs:125..166`. The
  current export uses **binary semaphores**.
- **Timeline-semaphore-as-SYNC_FD portability is NOT yet
  verified.** `vkGetSemaphoreFdInfoKHR` takes a semaphore
  handle but no value field. Exporting "this timeline at value
  N" as a sync_fd requires either (a) Vulkan's `VK_KHR_external_
  semaphore_fd` advertising `TIMELINE` + `SYNC_FD` in its
  exportable handle types (queried via
  `vkGetPhysicalDeviceExternalSemaphoreProperties`), or (b)
  waiting for the timeline to reach the value on the CPU and
  *then* taking a binary-semaphore SYNC_FD bridge. RADV gfx8
  may not support (a); modern drivers may. P0 below verifies
  this empirically before the design commits to either path.
- **Composite pass and flip submission.** The composite pass is
  in `crates/yserver/src/kms/vk/compositor.rs`; per-bo scanout
  state in `crates/yserver/src/kms/vk/scanout.rs::BoPhase`. The
  pageflip handoff (with `IN_FENCE_FD` + `OUT_FENCE_PTR`) is
  spec-correct on the bo-bookkeeping side. The bug is upstream
  of it — composite submission doesn't wait on paint completion
  via Vulkan-side sync, it waits via CPU-side queue drain.
- **xorg-server reference.** `../xserver/glamor/glamor.c` reads
  paint commands into the GLES driver's command stream all
  event-loop iteration long, and `glamor_block_handler` calls
  `glamor_flush()` → `glFlush()` once per `select()` cycle.
  `glFlush()` is async — driver submits, returns; GPU works in
  parallel. The only `glFinish()` in the tree is at server
  shutdown. **This is a batching reference, not a sync
  reference.** glamor inherits implicit dma-buf sync from the
  GLES driver; yserver's Vulkan renderer has no implicit-sync
  path and must model image layouts, memory hazards, external
  fd export, and KMS acquire/release explicitly. The glamor
  pattern informs *when* to flush (event-loop quiescent
  point), not *how* to sync.
- **wlroots / Mutter / KWin reference.** Per-frame batch
  submission with explicit Vulkan sync (binary semaphore per
  frame, exported as SYNC_FD for IN_FENCE_FD; timeline
  semaphore on drivers that support exporting it). This is the
  proximate reference for the sync model.

## Goal

Eliminate `vkQueueWaitIdle` (and any other CPU-blocking queue
drain) from the paint → composite → flip **hot path** on the
KMS backend, replacing it with Vulkan-native sync (binary or
timeline semaphores, plus `synchronization2` barriers).
`vkQueueWaitIdle` calls on **resource lifetime / teardown**
paths (scratch destruction, pipeline cache rebuild) stay.

Single observable outcome required: **no GPU 100% spike on a
single mate-control-center hover or wezterm-open under any
output configuration on any tested driver (RADV-Polaris,
RADV-Renoir, i915-Skylake).**

Plausible secondary outcomes (verified after sync rework,
not promised):
- Dual-output flicker on Polaris under burst paint reduced or
  eliminated.
- The HW cursor plane (commit `2d357cc`) becomes unnecessary;
  pointer motion rides composite cadence with no visible lag.

## Non-goals

- Changing scanout BO allocation. The Vulkan-linear-export
  path is the current fallback for cap-restricted drivers and
  stays. A separate follow-up will add **GBM-with-modifier**
  allocation as a feature-detected primary path on modern
  drivers (`VK_EXT_image_drm_format_modifier` + modifier-
  intersection success); the eventual architecture is
  `GBM+modifier if available, else Vulkan-linear fallback`.
  That follow-up does not block this work and is **orthogonal**
  to it — paint→composite→flip sync is the same shape under
  either allocator. See the GBM follow-up spec when written.
- Damage-region clipping in the composite pass. Cheaper
  composite is real future work but orthogonal to sync.
- ynest backend. This is a KMS-only change. ynest forwards
  paint to a host X server and inherits the host's sync model.
- Vulkan API level bump. We already require 1.3.
- Multi-Vulkan-queue parallelism (graphics + transfer). Codex
  observation: "graphics+transfer queues can help later for
  uploads but do not address the architectural flicker
  problem. On older RADV/Polaris, extra queue choreography may
  make things worse."

## Constraints

- **Must work on every Vulkan 1.3 driver that advertises the
  per-device capabilities the KMS sync model requires** —
  yserver is a general X11 server, not Polaris-targeted. The
  required capabilities (P0): `VK_KHR_external_semaphore_fd`
  with binary `SYNC_FD` exportable for the primary path, plus
  optional `TIMELINE + SYNC_FD` for the P7 optimisation. Pass
  the P0 probe → supported. Fail → documented as unsupported
  on KMS (ynest backend, which inherits host-X sync, is
  unaffected). Targeted drivers: RADV gfx8+ (Polaris and
  later), AMDVLK, Intel ANV, NVIDIA proprietary / nvk,
  lavapipe, Mesa Venus. Vulkan-1.3-conformance alone is *not*
  sufficient — external-fd export is a per-device feature.
- Single-threaded core (Phase 6.8). No new threads on the hot
  path. No new locks. New state goes on `ServerState` or the
  KMS backend struct.
- Must not regress `cargo test`, `just xts-yserver`, or
  `just rendercheck-yserver`.
- The `IN_FENCE_FD` / `OUT_FENCE_PTR` handoff to KMS must be
  preserved — Vulkan→KMS does not have implicit-sync the way
  GLES→KMS does, so explicit sync stays load-bearing.

## Architecture overview

```
┌──────────────────────────────────────────────────────────────┐
│ Core loop (single-threaded)                                  │
│                                                              │
│   request_in → process_request → paint op records into the  │
│                                  frame's command buffer.    │
│                                  No submit yet.              │
│                                                              │
│   poll-loop quiescent point (pre-poll.poll):                 │
│     0. Determine "outputs that will composite this flush":   │
│          dirty(F) ∩ (previous flip retired). Call this       │
│          set C(F). Outputs that are dirty but skipped (the   │
│          existing vk_flip_pending gate) are NOT in C(F).     │
│     1. If frame-cmdbuf is non-empty:                         │
│          submit it with                                      │
│          signal_semaphore_infos = [                          │
│              paint_done[X, F]  for each X in C(F)            │
│          ]                                                   │
│          signal_fence = paint_fence[F]                       │
│        Critically the signal set is exactly C(F): one        │
│        signal per consumer. If C(F) is empty (paint-only     │
│        frame, off-screen pixmap, or all outputs skipped),    │
│        the array is empty and only paint_fence is signalled. │
│     2. For each output X in C(F):                            │
│          submit composite for X.                             │
│             wait_semaphore   = paint_done[X, F]              │
│             signal_semaphore = composite_done[X, F]          │
│             signal_fence     = composite_fence[X, F]         │
│          export composite_done[X, F] as SYNC_FD              │
│          atomic_commit with IN_FENCE_FD = SYNC_FD,           │
│                           OUT_FENCE_PTR = release_fence      │
│     3. On pageflip-complete: BO retire; release_fence[X, F]  │
│                              signalled.                      │
│                                                              │
└──────────────────────────────────────────────────────────────┘
```

### Binary semaphore wait/signal cardinality

Vulkan binary semaphores have **1:1 wait/signal** cardinality
(Vulkan spec, "Semaphore Signaling" / "Semaphore Waiting"): a wait
operation consumes the signalled state and leaves the semaphore
unsignalled; a binary semaphore can have **at most one pending
signal** at any time. The architecture cannot have multiple
consumers wait on a single binary semaphore signalled once — the
first wait consumes it, subsequent waits see it unsignalled. The
inverse also holds: a binary semaphore signalled with **no matching
wait** ever issued is also invalid for reuse — the next signal on
the same handle would be a second pending signal.

We therefore **fan out paint_done at signal time** and tie the
fan-out set to actual consumers (C(F) — outputs that will run a
composite this flush, determined at flush_frame time). One paint
submit, |C(F)| output-specific signal entries in
`VkSubmitInfo2::signal_semaphore_infos`, one wait per composite
submit. The fan-out is free at submit time (no extra command
buffers; the driver tracks N signal points on the same submit's
completion).

**Skipped-output rule.** If an output X is dirty but its previous
flip hasn't retired (`vk_flip_pending`), X is **not in C(F)** and
`paint_done[X, F]` is **not signalled** (no orphan signal, so
binary cardinality holds). X's later catch-up composite gets its
paint-visibility from a different mechanism — **same-queue
submission ordering**, not a carried semaphore:

- All paint submissions and all composite submissions go to the
  same graphics queue (`vk.graphics_queue`).
- Vulkan guarantees that work submitted to a single queue
  executes in submission order on the device (modulo intra-submit
  reordering, which is bounded by barriers we already insert).
- Therefore: if frame F's paint was submitted at time T_p and X's
  catch-up composite is submitted at a later time T_c > T_p, then
  on the GPU the catch-up composite begins after the paint
  completes, regardless of whether anything signalled
  `paint_done[X, F]`.

Two catch-up scenarios both work:
1. **Catch-up alongside a later frame F'**: X's flip retires at
   some flush F'. X joins C(F') and waits on `paint_done[X, F']`,
   which the F' paint submit signals. F' is queued after F, so
   the wait covers F's mirror writes by queue order.
2. **Catch-up with no intervening paint** (idle server, only X's
   flip retire arrived): the next composite_and_flip cycle finds
   X dirty (its dirty flag was never cleared by a successful
   composite) but no new paint. The cycle submits X's composite
   with **no `wait_semaphore`** (since paint isn't being submitted
   this cycle) — queue order alone ensures F's paint writes are
   visible. Spec implementations must therefore allow a composite
   submit with no paint_done wait, falling back on same-queue
   ordering. This is correct because by definition the prior
   paint submits are already in flight or complete on the queue.

This matches the existing pre-rework behaviour where a skipped
output "shows the latest mirror state when it catches up." No new
content semantics are introduced.

On drivers where `(TIMELINE, SYNC_FD)` export is supported (P0
probe + P7), a single timeline `paint_done` semaphore can replace
the N binary fan-out — timeline waits are not exclusive: many
consumers can wait the same `(semaphore, value)` pair.

### SYNC_FD export transference

`vkGetSemaphoreFdInfoKHR` with `handle_type = SYNC_FD` against a
binary semaphore uses **copy transference** (Vulkan spec,
"External Semaphore Handle Types"; "Importing Semaphore Payloads"):
the export transfers the semaphore's payload into the returned fd
and **consumes the semaphore payload for cardinality purposes**
(equivalent in cardinality effect to a wait operation — the source
semaphore is left unsignalled). The exported fd is independent:
closing or consuming it (KMS does the latter on atomic accept) does
not affect the semaphore. The semaphore handle is therefore
reusable for the next frame's signal as soon as the export call
returns.

The model mirrors `glamor_block_handler` for **batching**:
paint commands accumulate during request processing and flush
at the next event-loop quiescent point.

The **sync model** is explicit Vulkan semaphores. Whether the
intra-frame semaphore is **binary** or **timeline** depends on
the device's external-semaphore export capability (P0):

- **Binary semaphore per frame (primary path).** One binary
  semaphore per `(frame_id, role)` where role ∈ {paint_done,
  composite_done[output]}. Pool allocated, recycled per frame.
  SYNC_FD export of binary semaphores is common across our
  target drivers (RADV gfx8+, AMDVLK, Intel ANV, lavapipe,
  Mesa Venus) but is not guaranteed by Vulkan 1.3 conformance
  alone — it requires the device to advertise `EXTERNAL_
  SEMAPHORE_HANDLE_TYPE_SYNC_FD` as exportable for
  binary semaphores. P0 probes this explicitly per device
  before declaring the device supported.

- **Timeline semaphore per role (optimisation, where
  exportable).** One timeline semaphore for paint_done (with
  monotonic values), one per output for composite_done. Fewer
  semaphore objects. Requires `vkGetPhysicalDeviceExternalSemaphore
  Properties` to advertise `TIMELINE + EXTERNAL_SEMAPHORE_HANDLE
  _TYPE_SYNC_FD` as exportable. If not advertised: fall back
  to binary per frame.

The architecture commits to **binary-per-frame as the primary
model** and the timeline upgrade as an optional optimisation
landed in a separate phase if and when P0 verifies it works.

## Design

### Frame lifetime

A *frame* is the unit of paint accumulation: all paint ops
issued between two composite passes. A frame opens lazily on
the first paint op after the previous composite, and closes
when the core loop reaches the next quiescent point with
dirty state.

Per-frame state on the backend. The output set is **dynamic**
(RandR hotplug, output add/remove) — all per-output collections are
runtime-sized, keyed by stable `OutputId`. Fixed-size arrays in v3
were a typo:

```
struct Frame {
    id: u32,                          // monotonic, wraps; compare
                                      // by ring-slot, not by value
    cmd_buffer: VkCommandBuffer,      // primary, per-frame

    // GPU↔GPU ordering for paint→composite fan-out. One binary
    // semaphore per output in C(F). Each is signalled exactly once
    // by the paint submit (multi-entry signal_semaphore_infos) and
    // waited once by that output's composite submit. **Each
    // (frame_slot, output) pair owns a distinct VkSemaphore handle**
    // — sharing one per-output semaphore across in-flight frame
    // slots would violate binary cardinality (two pending signals
    // on the same handle). Allocated lazily from a backend-side
    // semaphore pool keyed on `(slot_id, output_id)`; the pool
    // grows when a new output appears at a slot that hasn't seen
    // it before, and the (slot, output) entry is retired (cleared
    // for reuse on the next frame in that slot) when
    // `composite_fence[output]` for the slot's frame signals.
    paint_done: HashMap<OutputId, VkSemaphore>,

    // GPU↔KMS ordering for composite→flip. Exported as SYNC_FD
    // for IN_FENCE_FD handoff. Allocated with VkExportSemaphoreCreate
    // Info{SYNC_FD} so the export path works. paint_done semaphores
    // are NOT export-config'd — only composite_done is exported.
    // Same per-(slot, output) ownership rule as paint_done.
    composite_done: HashMap<OutputId, VkSemaphore>,

    // CPU-side retirement. VkFence (not semaphore). Used by the
    // host to know when frame resources can be recycled. The
    // resource pool waits on these via vkWaitForFences /
    // vkGetFenceStatus, NOT vkWaitSemaphores (which is timeline-
    // only and does not apply to binary semaphores).
    paint_fence: VkFence,             // signalled by paint submit
    composite_fence: HashMap<OutputId, VkFence>,
                                      // signalled by each composite submit

    // KMS release fence per output. Allocated by KMS via
    // OUT_FENCE_PTR on atomic commit, signals on flip-complete.
    // Drives scanout BO retirement.
    release_fence: HashMap<OutputId, DrmSyncObjFd>,
}
```

**Output-set timing.** The output set is **captured at flush_frame
time**, not at open_frame. Reasons:
- The dirty / vk_flip_pending state of each output is only known
  at flush.
- RandR hotplug between open_frame and flush_frame is handled
  trivially — new outputs simply join the C(F) set if dirty.
- Removed outputs drop out of all per-frame maps; in-flight frames
  that still reference the removed output retain the entry until
  the frame retires (the existing scanout BO drop path handles the
  KMS-side teardown).

Frames are pool-allocated. Retirement is **two-tier** because the
resources have different lifetimes:

**Tier 1 — paint frame CB, paint-side descriptor pool, frame-scoped
scratch.** Recyclable when:
- `paint_fence` signalled (paint submit done), AND
- For every output X in C(F): `composite_fence[X]` signalled. C(F)
  is exactly the set captured at flush time (see above) — outputs
  that were skipped (vk_flip_pending) are not in C(F) and don't
  block Tier 1 retirement because they didn't consume the paint
  resources.

Scanout completion is irrelevant here; once composite has run, the
mirror-sampling descriptor sets aren't touched by the flip.

**Tier 1b — composite pipeline's own descriptor pool.** The current
`CompositorPipeline` (see `crates/yserver/src/kms/vk/compositor.rs:144`
and `crates/yserver/src/kms/vk/pipeline.rs:272`) carries a single
backend-shared `descriptor_pool` reset at the start of every
composite pass. Under the new model multiple per-output composite
submits can be in flight simultaneously, so resetting that single
pool for output 1 would invalidate descriptor sets still in flight
on output 0. **Required change**: replace the single pool with a
ring keyed on `(frame_id % N_FRAMES, output_id)`. Each
`(frame, output)` slot is reset when its `composite_fence` signals
— that's the same condition that retires Tier 1 for that output.
Equivalent in effect to extending Tier 1 to cover compositor
descriptors. This change lands as part of P1 scaffolding, not
P2/P3 per-pipeline migration, because it's a property of the
compositor itself.

**Tier 2 — scanout BO.** Recyclable when:
- `release_fence[X]` signalled for that X (the scan-out engine
  released the BO).

Tier 2 is handled by the existing `ScanoutBo` phase machine in
`scanout.rs` and is unchanged by this rework — note though that
the BO retirement is per-output, not per-frame: a frame's slot can
be Tier-1-retired while one of its BOs is still scanning out.

### Paint accumulation

Paint op recorders today each create a one-off
`VkCommandBuffer` and submit it via `run_one_shot_op`
(`vk/ops/mod.rs`). The new model:

- A single **per-frame primary command buffer** is opened at
  the start of each frame (lazily, on first paint op).
- Each paint-op recorder appends its commands to that buffer
  via the existing `record_*` functions. The recorder no
  longer submits.
- At the next core-loop quiescent point, the primary buffer
  is closed and submitted with `signal_semaphore = paint_done`.

This matches glamor's BlockHandler pattern for **batching**.

### Shared mirror dependency model

Window/pixmap mirror images are **global** — a paint to window
W produces content that any output's composite pass may sample
on the next frame (the window may appear on output A, output
B, both, or neither). The sync model must therefore order
**all outputs' composites in a frame** after **all paint ops
in that frame**.

The single-frame primary command buffer handles the ordering on
the GPU-work side: all paint ops in frame F land in one command
buffer. The submission of that command buffer signals **N binary
semaphores in one go** — one per dirty output — via
`VkSubmitInfo2::signal_semaphore_infos`. Each output's composite
waits on its own dedicated `paint_done[output, F]`. The fan-out is
cardinality-correct (each binary signal has exactly one matching
wait) and adds zero CPU or GPU overhead vs a hypothetical single
shared semaphore — the driver tracks N signal points on the same
submission's completion.

The "one paint submit per frame" architecture stays the synchronis-
ation pivot; we only widened the signal side from 1 → N.

In-frame paint ordering (e.g., FillRect then CopyArea on the
same mirror) is enforced by `vkCmdPipelineBarrier2` *inside*
the frame's command buffer — image barriers with conservative
masks (`ALL_GRAPHICS` / `SHADER_WRITE|COLOR_ATTACHMENT_WRITE` →
`SHADER_READ|COLOR_ATTACHMENT_READ`). Finer-grained masks are
a tuning follow-up.

### Composite submission

For each output X with dirty state and a retired previous
flip:
- Record a command buffer for X's composite (draws into X's
  scanout BO from the window mirrors).
- Submit with:
  - `wait_semaphore = paint_done[X, frame_id]` (X's dedicated
    binary semaphore from the paint submit's fan-out; GPU wait —
    no CPU wait)
  - `wait_stage = ALL_COMMANDS` until barriers in P3 are audited
    (codex round-2 recommendation; tighten in a follow-up)
  - `signal_semaphore = composite_done[X, frame_id]` (allocated
    with `VkExportSemaphoreCreateInfo{SYNC_FD}` at construction
    time so the export path is wired)
  - `signal_fence = composite_fence[X, frame_id]` (CPU-side
    retirement signal for descriptor / scratch reuse)
- Export `composite_done[X, frame_id]` as a SYNC_FD via
  `vkGetSemaphoreFdInfoKHR`. The export uses copy transference
  (see "SYNC_FD export transference" above); the source semaphore
  becomes unsignalled and is reusable for the next frame.
- The exported SYNC_FD is the `IN_FENCE_FD` of the atomic
  commit for X.

### KMS flip handoff

`OUT_FENCE_PTR` semantics unchanged: KMS allocates a release
fence per flip, signals on scanout, returns the fd. We track
it per-(output, frame) in the existing `ScanoutBo::Pending`
state.

The current per-output `vk_flip_pending` skip in
`composite_and_flip` stays. After sync rework lands, the
per-output frame-age drift hypothesis for the dual-output
flicker (P6) gets re-tested; if it persists, the fix is a
separate scheduling change to the skip logic.

### Resource lifetime — per-pipeline migration

The `vkQueueWaitIdle` surface outside `ops/mod.rs` falls into
three categories, each with its own migration story:

1. **Per-op staging / scratch reuse** (`glyph.rs`,
   `mask_scratch.rs`, `copy_scratch.rs`, `gradient.rs`,
   `dst_readback.rs:105`).
   These resources back single paint ops. Today
   `vkQueueWaitIdle` after the submit guards the resource
   against reuse before the GPU's done.
   *New model*: resource ownership transfers to the frame.
   Each frame submit takes a **`VkFence`**
   (`paint_fence[frame_id]`) — not a semaphore. Binary
   semaphores carry the GPU↔GPU and GPU↔KMS ordering;
   `VkFence` carries the CPU-side retirement signal that the
   host needs to recycle resources. The pool waits on
   `paint_fence[frame_id]` (via `vkWaitForFences` /
   `vkGetFenceStatus`) before reusing the slot. **Do not** use
   `vkWaitSemaphores` for binary semaphores — that API is
   timeline-only. Each migrated paint pipeline lands its own
   resource-lifetime fix — **not** in a separate later phase.

2. **Cross-frame readback** (`dst_readback.rs:264`, the
   `record_get_image` path).
   Host needs to read the GPU's writes. Today: queue drain.
   *New model*: targeted fence wait on **just this op's
   completion** (a small CPU-side `VkFence` signalled by the
   op's submit, waited at the readback point). Other ops in
   the queue keep flowing.

3. **Teardown / pipeline rebuild** (`pipeline.rs:314`,
   `render_pipeline.rs:510, 652`, `text_pipeline.rs:329`,
   `logic_fill_pipeline.rs:137`, `target.rs:735`,
   scratch destructors).
   Off the hot path — pipeline-cache rebuild, image
   destruction, scratch buffer free. `vkQueueWaitIdle` here
   is correct and **stays**. Document why each one is
   load-bearing for lifetime so a future cleanup pass doesn't
   remove it accidentally.

The per-pipeline migration in P2/P3 carries (1) and (2) with
each path. (3) is audit-and-comment only.

### Descriptor pool reuse

Many paint ops allocate descriptor sets from a pool. With the
per-op drain gone, descriptor sets allocated for frame F-1
may still be in use by the GPU when frame F starts allocating.
*New model*: descriptor pools are scoped per frame — a ring of N
pools indexed by `frame_id % N`. Slot S's pool is reset when
**all of**:
- `paint_fence[S]` signalled, AND
- every `composite_fence[output, S]` for outputs that ran a
  composite waiting on `paint_done[output, S]` signalled.

`release_fence` is **not** part of this rule — the scan-out engine
doesn't touch our descriptor sets. Lumping release-fence wait into
descriptor reuse would needlessly stretch frame retirement to
vsync cadence and starve the ring.

Pool sizing: Vulkan command buffers **cannot be partially rolled
back**, so mid-recording allocation failure is unrecoverable.
Discovery must happen **before** the recorder emits any commands.

Invariant: every recorder allocates descriptor sets at the top of
its `record_*` function, before any `vkCmd*` calls into the frame
CB. If allocation returns `OUT_OF_POOL_MEMORY`:
1. The recorder returns the error without writing to the frame CB.
2. The dispatch helper calls `flush_frame()` (submits whatever
   paint is already accumulated, signals C(F) paint_done, schedules
   composites).
3. A new frame slot opens (which resets its descriptor ring slot
   once paint_fence + composite_fence retire — usually immediately
   if N_FRAMES is right-sized).
4. The recorder retries in the new frame.

Sizing therefore matters as a **performance** consideration (a
mid-frame flush stalls the batching benefit) but not a
**correctness** one. Size the per-frame pool from observed upper
bounds in real workloads (CompositeGlyphs + RENDER Composite-heavy
frames). Codex round-2/3 finding.

This lands with the first migrated pipeline in P2 because every
paint pipeline allocates descriptors.

### HW cursor plane revert

After the sync rework lands and pointer motion no longer
piggybacks on a slow composite path:
- Optionally revert commit `2d357cc` (the HW cursor plane).
- Cursor renders as a Vulkan-composited quad again.
- The Polaris-DCE legacy-cursor-ioctl artefact goes away as a
  side effect (the code that triggered it is gone).

Cursor pacing has more inputs than just GPU sync (frame
pacing, dirty scheduling, motion-event rate). Validate
responsiveness empirically before reverting; if the composite-
quad cursor still feels laggy, keep the HW plane as a
performance feature, classified as a non-fallback path, and
fix the legacy/atomic interop on amdgpu DCE separately.

### Test strategy

1. **Unit: binary-semaphore submit pipelining with fan-out.**
   Two paint ops in one frame command buffer, one paint submit
   signalling **two** binary semaphores (`paint_done[A]` +
   `paint_done[B]`), two composite submits each waiting on its
   own dedicated semaphore. Assert each composite's SYNC_FD
   signals only after the paint ops complete by `vkWaitForFences`
   on the frame's `paint_fence` and per-composite `composite_fence`
   (CPU-side retirement), plus `poll(POLLIN)` on each exported
   SYNC_FD. `vkWaitSemaphores` is intentionally **not** used —
   the binary-semaphore + VkFence split keeps GPU-ordering and
   CPU-retirement separate.
2. **Unit: in-frame barrier insertion.** Two paint ops to
   the same mirror image; inspect recorded command stream
   for the inserted `vkCmdPipelineBarrier2` between them.
   Validation layers should report zero errors in debug
   builds.
3. **Unit: descriptor-pool ring.** Allocate descriptors in
   frame F-1, submit; frame F allocates more before F-1
   retires. Assert no resource reuse before
   `paint_done[F-1]` signals.
4. **Unit: per-op fence on readback.** GetImage-equivalent
   path; assert host-side fence wait happens **only on the
   readback op**, not as a queue drain.
5. **Integration (`#[ignore]`, needs ICD):
   paint→composite→flip end-to-end** through the ServerFixture.
   Verify scanout image matches a software-rendered reference.
6. **xts5**: no regression. Per-op sync removal can expose
   ordering bugs that were latent.
7. **rendercheck**: no regression. Same reasoning.
8. **Manual: mate-control-center hover.** No pointer lag on
   Polaris; no GPU 100% spike on a hover that doesn't actually
   need to repaint anything.
9. **Manual: wezterm open + uxterm resize/hover, dual-screen
   Polaris.** Flicker absent or reduced.
10. **Manual: vkcube / glxgears on a redirected window.** No
    regression from the sync change.

## Phasing

**P0 — verify export semantics.** Two device-capability probes
and one end-to-end smoke:

1. **Binary export probe** (gates the primary sync model).
   Call `vkGetPhysicalDeviceExternalSemaphoreProperties` with
   `handleType = EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD` and
   `semaphoreType = SEMAPHORE_TYPE_BINARY` (default), assert
   `externalSemaphoreFeatures` contains `EXPORTABLE_BIT`. This
   is the load-bearing capability for the primary path — if a
   device fails it, that device is unsupported by yserver-KMS
   and must fall back to whatever pre-sync-rework path remains
   (or be documented as unsupported).
2. **Timeline export probe** (gates the optional P7).
   Same query with `semaphoreType = SEMAPHORE_TYPE_TIMELINE`.
   Result determines whether P7's object-count optimisation
   ever lands.
3. **End-to-end smoke**: allocate a binary semaphore, submit a
   dummy command buffer with `signal_semaphore = sem`, export
   the resulting signal as SYNC_FD via
   `vkGetSemaphoreFdInfoKHR`, hand to KMS as `IN_FENCE_FD` on
   a no-op atomic commit. Validates that the explicit-sync
   handoff actually works on RADV-Polaris, RADV-Renoir, Intel
   ANV, lavapipe. Drivers that pass the probe but fail the
   smoke get documented as broken; the architecture decision
   doesn't change.

1-2 days.

**P1 — scaffolding.** Per-frame command buffer + binary
semaphore pool + per-frame descriptor pool ring. No paint
pipeline migrated yet; this is the infrastructure paint paths
plug into. 1-2 days.

**P2 — one paint pipeline as proof.** Migrate the smallest
recorder (`FillRect` via `record_fill_rectangles`) to the per-
frame accumulator. Composite for the affected output **split
into a separate submit** that waits on `paint_done[output, F]`
via the per-output binary semaphore — *not* one combined submit.
The two-submit structure validates **binary split-submit + KMS
fd handoff** (the primary sync model): a single-submit shortcut
would let intra-buffer ordering substitute for inter-submit sync
and not exercise the explicit semaphore-and-fd path that
composite + KMS rely on. Land the resource-lifetime fix for
FillRect's staging / descriptor allocations in the same commit,
using `paint_fence` + `composite_fence` for CPU-side retirement
(both required — see "Descriptor pool reuse" above). Verify
against the legacy serialised path under
`YSERVER_LEGACY_VK_SYNC=1`. 2-3 days.

**Rollout invariant (intra-frame and inter-frame ordering).**
Per-frame gating is insufficient: even if frame F is "entirely
new" (only migrated recorders) and frame F+1 is "entirely legacy"
(touches an un-migrated recorder), F's composite may still be
deferred to the next composite_and_flip cycle while F+1's legacy
op submits + `queue_wait_idle` first. The legacy idle drains
F's *paint* submit (already submitted) but cannot drain F's
unsubmitted composite; when composite finally submits it samples
mirrors that F+1's legacy paint has already overwritten. Codex
round-3 finding.

The structurally safe rule is: **legacy submits flush any open
frame and any deferred-but-unsubmitted composite before they
run.** Concretely, the legacy dispatch helper does:
1. If `current_frame_id.is_some()`: call `flush_frame()` (submits
   paint, signals the C(F) paint_done set).
2. For each output in C(F): immediately record + submit its
   composite (does not wait for the next core-loop iteration).
3. Then proceed with the legacy one-shot CB + `queue_wait_idle`.

The `queue_wait_idle` at step 3 fully drains the GPU including the
composites from step 2, so subsequent paint (legacy or new) is
ordered after both the prior frame's paint and its composites.
Slow during the mixed-mode rollout window (one drain per legacy
op), but correct. After P4 strips the legacy path entirely, the
rule is moot.

**P3 — remaining paint pipelines.** Each migrated path lands
with its own resource-lifetime fix. Order by complexity:
`CopyArea` → `PutImage` / `GetImage` → RENDER Composite →
RENDER CompositeGlyphs → RENDER Trapezoids → text → logic-fill.
Each pipeline ~half-day to a day. The legacy dispatch helper
(see "Rollout invariant" above) carries each unmigrated pipeline
safely across the rollout window. After all migrated, remove the
legacy dispatch path entirely. 3-5 days.

**P4 — composite + flip thread-through.** Per-output composite
submission switches from "wait via implicit queue ordering" to
"wait via explicit `paint_done` semaphore." Per-op
`vkQueueWaitIdle` removed from every paint recorder. The
`ops/mod.rs` central drain is the last to go. 1 day.

**P5 — HW cursor plane revert (optional).** Manual smoke
under MATE on Polaris confirms no pointer lag, no cursor
artefacts. If lag returns when reverting the HW plane,
investigate composite-quad cursor pacing separately and keep
the HW plane as a performance feature (refactored to atomic-
plane commits, not legacy ioctls, on amdgpu DCE). Half day to
revert, indefinite if quirks surface.

**P6 — dual-output flicker re-test.** Replay the wezterm-open
and uxterm-resize tests on dual-screen Polaris with the new
sync model. If absent, done. If present, follow-up scoped to
the per-output `vk_flip_pending` skip behaviour. Half day to
verify; follow-up size unknown.

**P7 — timeline-semaphore optimisation (conditional on P0).**
On drivers that advertise `TIMELINE + SYNC_FD`, replace the
binary-per-frame model with a timeline-per-role model. Fewer
semaphore objects, slightly cleaner state. Skipped on drivers
that don't support it. 1-2 days when implemented.

Expected total: 1-2 weeks of focused work for P0-P5; P6 is
empirical; P7 is conditional.

## Risks

- **Latent ordering bugs.** The per-op queue drain was a
  sledgehammer that hid every missing barrier. Some paint
  paths almost certainly relied on it implicitly. Mitigation:
  per-pipeline rollout under `YSERVER_LEGACY_VK_SYNC=1` build
  flag, so we can A/B compare visual output before flipping
  the default.
- **TIMELINE+SYNC_FD portability.** Codex flagged this as a
  high-risk hand-wave in v1. Resolved structurally by making
  the binary-per-frame model the primary path and timeline an
  optimisation gated on P0 verification. If P0 says timeline
  export doesn't work on any of our targets, P7 is dropped
  entirely; the architecture still works.
- **Shared mirror sync across outputs.** Codex flagged. The
  design's single-`paint_done`-per-frame answer relies on all
  paint going into one frame command buffer. If paint
  pipelines ever submit their own outside-the-frame command
  buffers (e.g., a future async upload path), the invariant
  breaks. P1's frame scaffolding enforces "all paint goes in
  the frame buffer" as the only path — additions must justify.
- **Scratch / staging / descriptor pool lifetime.** Codex
  flagged "many `vkQueueWaitIdle` calls are outside `ops/*.rs`."
  Addressed by classifying every call site (the "Current
  state" section) and landing the relevant lifetime fix
  alongside each migrated pipeline (P2/P3), not as a deferred
  phase.
- **Validation-layer regressions.** Once explicit barriers go
  in, validation layer errors must be zero on debug builds.
  Enforce via a CI run that exports `YSERVER_VK_VALIDATION=1`
  and fails on any validation error. P4 cuts in.
- **In-frame barrier coverage gaps.** Wrong barriers produce
  validation errors at best and TDR (GPU hang) at worst.
  Validation layers in debug builds catch the validation
  errors. Mitigation: conservative `ALL_GRAPHICS` masks first,
  finer-grained as a follow-up.
- **Dual-output flicker may persist** (codex flagged). The
  per-output `vk_flip_pending` skip can cause per-output frame
  divergence even with perfect GPU sync. Treated as "plausible
  reduction, not promised fix" in the Goal section, and P6
  re-tests it; a separate follow-up handles it if needed.
- **Cursor pacing depends on more than GPU sync.** P5 is
  optional and validated empirically.
- **Device-lost / GPU hang.** `vkWaitForFences` with
  `u64::MAX` blocks the single-threaded core loop indefinitely
  on driver hang. Mitigation: bounded timeout (≤250 ms) on
  every `vkWaitForFences` in the hot path; on `TIMEOUT` or
  `ERROR_DEVICE_LOST`, log state + transition the backend into
  a fatal/reinit path. Call sites that must be bounded:
  - `open_frame` retiring a previous-occupant slot (waits on
    `paint_fence` and `composite_fence`s of frame F-N).
  - The Tier-1b compositor-descriptor-ring reset (per-output
    `composite_fence` wait).
  - `record_get_image`'s readback fence wait (own one-shot fence).
  - `FrameScopedQueue::drain_retired` if it ever falls back to a
    fence wait (it normally uses `vkGetFenceStatus`, no block).
  Out of scope of this list: `Drop` impls that call
  `device_wait_idle` at shutdown; teardown is allowed to block.
  Codex round-2/3 finding.
- **Binary fan-out cardinality.** Initial v1/v2 designs had
  every output's composite wait on a single `paint_done[F]`.
  Vulkan binary semaphores enforce 1:1 wait/signal; the first
  composite consumed the signal and subsequent composites
  waited on nothing. Codex round-2 caught this. Fixed
  structurally in v3 by signalling N per-output `paint_done`
  semaphores from the single paint submit's
  `signal_semaphore_infos` array. v4 refinement: signal set is
  C(F) (outputs actually compositing this flush), determined at
  flush_frame time — skipped outputs (vk_flip_pending) are
  excluded, avoiding the cardinality-violating "signal without
  matching wait" found in codex round-3.
- **Inter-frame rollout ordering.** During P3 the legacy
  dispatch helper must flush any open frame + run its deferred
  composites before issuing a legacy submit + queue_wait_idle.
  Without that, a legacy frame F+1 can reach the GPU before
  the prior new frame F's composite, and F's composite then
  samples mirrors F+1 has overwritten. Codex round-3 finding.
  Documented in the "Rollout invariant" subsection of P3.
- **Compositor descriptor pool collision under async submits.**
  The existing `CompositorPipeline::descriptor_pool` is shared
  across outputs and reset at the start of every composite
  pass. Under the new model multiple per-output composites can
  be in flight simultaneously, so the single-pool reset
  invalidates in-flight descriptors. Codex round-3 finding.
  Fixed in v4 by introducing a `(frame, output)`-keyed
  compositor descriptor ring as part of P1 scaffolding (Tier
  1b retirement).
- **Binary paint_done pool sizing.** v4 left the pool
  description ambiguous (Frame struct said "fresh per frame",
  prose said "reuse across frames" — both can't hold under
  binary cardinality with frames in flight). v5 (codex round-4
  finding) makes it explicit: each `(frame_slot, output)` pair
  owns a distinct VkSemaphore handle, retired when that slot's
  `composite_fence[output]` signals.
- **Skipped-output correctness argument.** v4 incorrectly
  claimed paint_done[X, F'] "carries forward" F's paint
  dependency — the F' semaphore actually covers F' paint, not
  F. v5 corrects the argument to **same-queue submission
  ordering**: paint and composite share the graphics queue, so
  the later catch-up composite is queue-ordered after the
  earlier paint regardless of which semaphore (if any) is
  waited. Also covers the no-new-paint catch-up case (composite
  submit with no `wait_semaphore`, relying purely on queue
  order).

### Issues deferred to the implementation plan

The codex review of v4 flagged several spec-level issues that
are better resolved at plan/code time than at spec time. These
are explicitly accepted as plan-scope for v5:

- **Tier 1b compositor descriptor ring slot-creation policy for
  new outputs** (capacity, allocation failure, retirement
  binding). The plan defines an explicit "ensure slot exists
  before recording, abort cleanly on failure" pattern.
- **Rollout-invariant ownership** — which module owns the
  `flush_frame() + record/submit composites + legacy submit +
  wait_idle` sequence (legacy op helper, `composite_and_flip`,
  or a frame manager). The plan picks the legacy dispatch
  helper as owner and documents the cost (one composite cycle
  per legacy op during the mixed window).
- **Output-set-at-flush prerequisites** — a new output joining
  C(F) needs scanout BOs, compositor descriptor ring slot, and
  scene-building state. The plan adds an explicit "is this
  output composable this frame" guard before adding to C(F).
- **Compositor descriptor allocation-failure policy.** Existing
  code logs and produces a partial composite
  (`compositor.rs:149`). Plan unifies the policy to "abort the
  composite for that output this frame, log, keep dirty so the
  next cycle retries." Better than partial scenes that produce
  visual artefacts.
- **"Dirty at flush" explicit precedence.** Plan implements
  `composite_and_flip` so the dirty/vk_flip_pending state is
  re-read at flush time, after pageflip completions for the
  current cycle have been processed.

## Open questions

1. **Does `BlockHandler`-equivalent live where I think it does?**
   The natural quiescent point is "after processing all
   available messages, before the next `poll()` call."
   `crates/yserver-core/src/core_loop/run.rs:104`
   (`poll.poll(&mut events, poll_timeout)`) is the boundary;
   the frame submit happens immediately before it. Composite +
   flip submission can stay where it is today (in
   `composite_and_flip`, called from `on_page_flip_ready` and
   from the dirty-mark path); the change is what it waits on
   and what it signals. Validate in P1.
2. **Frame ID space — u32 monotonic or `frame_id % N`?**
   Pool of N frames where N is small (3-4); `frame_id % N`
   indexes into the per-frame state. Wrap is fine **for ring
   indexing**, but any direct numeric comparison of frame IDs
   (e.g., "drain everything ≤ retired_id" in a `FrameScopedQueue`)
   is wrap-unsafe. Implementations must compare via ring slot,
   not via the raw `u32`, or use `u64` IDs internally. Codex
   round-2 finding.
3. **Descriptor pool ring depth.** Same N as frame depth.
   Reset pool when its `paint_fence` AND every
   `composite_fence[output]` for outputs that ran a composite
   that frame signal. `release_fence` is **not** part of this
   rule (the scan-out engine doesn't touch descriptors).
   Validate in P2.
4. **Paint-only frames** (no output dirty, no composite, but
   paint happened — e.g., off-screen pixmap; or all outputs
   skipped due to vk_flip_pending). The paint submit's
   `signal_semaphore_infos` is **empty** (C(F) is empty); only
   `paint_fence` is signalled. The frame slot is Tier-1-retirable
   as soon as `paint_fence` signals (no composite consumers to
   wait on). This satisfies binary semaphore cardinality — no
   semaphore is signalled without a matching wait.
5. **Validation-layer CI gating.** Today validation layers are
   opt-in via `YSERVER_VK_VALIDATION`. CI gate cuts in at P4
   (when the legacy path is removed). Defaults to off on
   normal `cargo test` to keep test wallclock down.

## Glossary

- **Paint op**: any X11/RENDER request that writes pixels —
  FillRect, CopyArea, PutImage, RENDER Composite, glyph blit,
  trapezoid coverage, etc.
- **Frame**: the unit of paint accumulation. All paint ops
  between two composite passes share one frame. Each frame
  owns a command buffer, a `paint_done` semaphore, per-output
  `composite_done` semaphores, and per-output `release_fence`s.
- **Binary semaphore**: traditional Vulkan semaphore — signals
  once per submit, waits clear it. **1:1 wait/signal cardinality:**
  each signal is consumed by exactly one wait; a binary semaphore
  can have at most one pending signal at a time. A signal with no
  matching wait is also invalid for reuse. Architectures with N
  consumers per producer therefore need N signals, not one shared
  signal. **Export-config (`VkExportSemaphoreCreateInfo{SYNC_FD}`)
  is required only for semaphores that will be exported** —
  `composite_done` in this design. `paint_done` is GPU-internal
  (waited by composite, never exported), so its semaphores do not
  need export-config. SYNC_FD export is common across our target
  drivers (RADV gfx8+, AMDVLK, Intel ANV, lavapipe, Mesa Venus)
  but is a per-device capability, not a Vulkan-conformance
  guarantee. SYNC_FD export uses copy transference — the export
  transfers and consumes the source semaphore's payload, returning
  the payload in a fresh fd that the caller owns. P0 probes the
  export capability explicitly.
- **Timeline semaphore**: `VK_KHR_timeline_semaphore` /
  Vulkan 1.2 core. A monotonic u64 counter. Submit-side
  `wait` blocks the GPU pipeline (not the CPU thread) until
  the semaphore reaches a specific value. SYNC_FD export of
  a specific value is not universally supported and is
  feature-detected per device.
- **In-frame barrier**: `vkCmdPipelineBarrier2` inside a
  single command buffer, ordering ops that touch the same
  image.
- **IN_FENCE_FD / OUT_FENCE_PTR**: KMS atomic-commit
  properties. IN waits for our fence to signal before
  scanout. OUT is allocated by KMS, signals when scanout is
  done. Both are carried as SYNC_FDs.
