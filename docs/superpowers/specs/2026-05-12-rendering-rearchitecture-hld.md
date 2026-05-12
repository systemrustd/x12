# Rendering re-architecture — high-level design

Date: 2026-05-12
Status: draft, for discussion
Author: jos + claude session
Supersedes: `docs/superpowers/specs/dead-ends/2026-05-12-paint-composite-sync-design.md` (see `dead-ends/POSTMORTEM.md`)

## Why this exists

The KMS backend's Vulkan renderer was grown from a "GPU-is-idle-when-this-returns"
contract: every paint op submits its own command buffer and `vkQueueWaitIdle`s
before returning. Recorders, scratch pools, scanout BO reuse, and the
compositor's mirror-sampling all silently depend on that contract.

The contract is the bug. It saturates the GPU under paint bursts (the
mate-control-center / wezterm symptom) and it almost certainly contributes
to the dual-output flicker on Polaris. The previous attempt tried to fix it
in place — preserve recorders, scratch types, and compositor shape, and slip
async sync underneath — and produced a system where every iteration surfaced
new edge cases. The postmortem captured the lesson: **the right unit of
replacement is the scheduling/lifetime layer, not the draw implementations.**

This document describes the replacement at the shape-of-the-code level. It
is deliberately not a sync algorithm spec.

## Goal

A renderer where paint, composite, and flip are scheduled by an explicit
in-flight frame model — wlroots/Mutter shape — instead of by CPU-side queue
drains. Recorders keep their drawing logic but stop owning sync.

Single load-bearing outcome: **`vkQueueWaitIdle` does not appear in the
paint → composite → flip hot path** on KMS. Lifetime/teardown drains stay
and are documented.

Secondary outcomes, verified after the rework lands (not promised):

- No GPU saturation spike on mate-control-center hover or wezterm-open
  across Polaris / Renoir / Skylake.
- Dual-output Polaris flicker reduced or eliminated.
- The HW cursor plane workaround (commit `2d357cc`) becomes unnecessary;
  pointer motion rides composite cadence without lag.

## Non-goals

- Changing the Vulkan API level (we already require 1.3).
- Changing scanout BO allocation. GBM-with-modifier is a separate
  follow-up; this rework is allocator-agnostic.
- Damage-region clipping in the composite pass.
- Multi-queue parallelism (graphics + transfer). A correctness-first
  single-graphics-queue model first; queue split is a later optimisation
  if profiling justifies it.
- ynest backend. ynest forwards paint to a host X server and inherits
  host sync. This is KMS-only.
- New threads or locks on the hot path. Phase 6.8's single-core
  invariant holds.

## Shape of the new code

Four types, three of them new, replace today's "recorder submits and waits"
model. Every type has a single owner and a single lifetime contract.

### `PaintBatch` — one per server frame

Accumulates all paint work between two composites into a single primary
command buffer. Owns:

- the primary CB,
- a per-frame descriptor pool,
- a per-frame scratch arena (mask images, gradient LUTs, glyph upload
  ranges) whose entries' lifetime equals the batch's.

Recorders take `&mut PaintBatch` (today they take a `vk::CommandBuffer`
and submit themselves). They append commands, allocate descriptors and
scratch from the batch, and return. They do not submit, do not wait, do
not touch `screen_dirty`.

A batch is opened lazily on the first paint op after the previous
composite. It is closed at the next core-loop quiescent point if it
has any recorded work **and** any *flush reason* holds:

- at least one output is dirty,
- a synchronous-reply request needs CPU-visible pixels
  (`GetImage`, the host-readback path),
- an external sync export is pending (DRI3 / Present fence handoff,
  SYNC extension fence),
- the batch has hit a size or op-count limit chosen to bound paint
  latency,
- an explicit protocol barrier requested it.

Offscreen-pixmap-only drawing has no dirty output but is still a
flush reason via the readback or sync-export rules. Idle ticks with
no work produce no batch.

### `OutputFrame` — one per output per composited frame

Represents "the GPU+KMS work for one output presenting frame F." Owns:

- a composite command buffer that samples the mirrors and writes the
  output's scanout BO,
- the output's per-frame descriptor sets (sampler bindings for the
  mirrors visible on that output),
- the sync primitives that order it against `PaintBatch` and against
  KMS (see below),
- the scanout BO slot it targets.

`OutputFrame` is created per-output at composite time from a
`PaintBatch` plus the output's dirty state. Multiple `OutputFrame`s
from the same batch are allowed and expected — that's how
multi-monitor works.

The existing `BoPhase` machine (`Free` / `Submitted` / `Pending` /
on-screen / retiring in `vk/scanout.rs`) is **kept**. It models real
KMS state and stays the source of truth for the scanout BO's
lifecycle. The rework subordinates it to `OutputFrame` ownership:
the `OutputFrame` holds the BO slot it targets, and BO phase
transitions are driven by the frame's submission and the matching
KMS release fence. `BoPhase` is not flattened to a binary
retired/free.

### `InFlight` — the server-wide retirement list

A queue of `OutputFrame`s that have been submitted but not yet
retired. The core loop drains it at every quiescent point with
non-blocking polls of the relevant primitives — never a blocking
wait with timeout > 0.

Each `OutputFrame` has **two retirement points**, separately
observable:

1. **GPU retirement** — the frame's `composite_done[output]`
   timeline has reached the frame's signal value
   (`vkGetSemaphoreCounterValue`, or `vkWaitSemaphores` with
   timeout = 0). Releases:
   - composite command buffer back to its pool,
   - per-frame descriptor sets,
   - image views, samplers, scratch allocations bound only into
     this frame's GPU work,
   - the `PaintBatch` refcount; when no in-flight frame still holds
     the batch, the batch's CB + descriptor pool + scratch arena
     GPU-retire too.

2. **Scanout retirement** — the KMS release primitive returned via
   `OUT_FENCE_PTR` (a sync_file fd) has signalled. The poll
   representation is an implementation choice (poll the fd
   directly for `POLLIN`, or import into a `drmSyncobj` and query
   it); the HLD is agnostic. Releases:
   - the scanout BO slot back through the `BoPhase` machine to
     `Free`.

Scanout retirement is generally later than GPU retirement (the BO
sits on-screen until KMS releases it on the next flip for that
output) and lower-frequency. Modelling them as one list with a pair
of non-blocking polls is fine for phase 1; the two-point split is
the invariant, not the data structure.

`InFlight` is the only place that knows "is anything still using
this." Recorders and pools never ask the queue or
`vkQueueWaitIdle` again.

### Per-output `DirtyGeneration` — replaces `screen_dirty`

Today's global `screen_dirty: bool` collapses "anything anywhere
changed" with "this output needs a composite this tick." Under in-flight
frames those are different questions.

Replace with a monotonic `gen: u64` per output. Producers (request
handlers, host input, hotplug) bump the relevant output's generation
when they touch state the composite needs to re-read. A composite for
output X captures `gen_at_composite[X]`; X is "still dirty" iff
`gen[X] > gen_at_last_successful_present[X]`.

This makes "skipped output catches up" trivial: a skipped output is
exactly one whose `gen_at_last_successful_present` lags. No `bool` to
clear conditionally, no orphan flags, no global flush flag.

Window→output dirty propagation rules:

- **Paint to window W**: bump every output X where W intersects X's
  layout rect (the same visibility predicate `composite_and_flip`
  uses today).
- **Geometry change** (move, resize, unmap, restack, reparent,
  destroy of a mapped window): bump every output X intersecting the
  **union of the pre-change and post-change visible regions**.
  Bumping only the new region leaves stale pixels on outputs the
  window vacated; bumping only the old region misses the new
  location. Both are required.
- **Hotplug / RandR layout change**: bump every output whose layout
  rect changed.

Cheaper damage tracking (sub-output rectangles, `FB_DAMAGE_CLIPS`)
is a follow-up — per-output generation bumps are enough for
correctness.

## Sync model (one paragraph each — full spec lives downstream)

**Inside the renderer: timeline semaphores.** One timeline per role
(`paint_done`, `composite_done[output]`). Values are monotonic frame
counters. Many consumers can wait on the same `(timeline, value)` pair,
so paint→multi-output composite fan-out is a single signal, not the
N-way binary fan-out that broke the previous design. No semaphore-pool
keying on `(slot, output)`. Same-queue submission ordering carries
catch-up cases.

**Primary retirement signal: the renderer timelines themselves.**
`InFlight` polls `composite_done[output]` via
`vkGetSemaphoreCounterValue` to drive GPU retirement, and the
`OUT_FENCE_PTR` sync_file fd to drive scanout retirement. No
separate `VkFence` is allocated per submission for retirement —
the timeline value carries that signal too. `VkFence` is reserved
for the readback exception (`GetImage`) where a dedicated
small-scope fence simplifies the targeted-wait path, and as a
phase-1 fallback if a specific submission step is materially
simpler to land with a fence first. The destination model has
one CPU-side primitive per role, not three.

**At the KMS boundary: binary SYNC_FDs.** KMS atomic commit's
`IN_FENCE_FD` does not understand timeline semaphores portably. The
boundary converts: at composite-submit time, signal a frame-local
binary semaphore in the same submit, export it as SYNC_FD via
`VK_KHR_external_semaphore_fd`, hand to atomic commit. `OUT_FENCE_PTR`
keeps its current role driving scanout BO retirement.

**Capability probe at startup.** Timeline-semaphore-as-SYNC_FD is
*not* probed — we don't need it. We need: binary
`EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD` exportable, plus core 1.2
timeline semaphores (guaranteed by 1.3). If the binary SYNC_FD export
isn't supported, KMS is unsupported on that device; ynest still works.

**Lifetime drains stay.** Pipeline-cache teardown, scratch pool drop,
image destruction on resize — these `vkQueueWaitIdle`s are correct
because they need "GPU done with this object forever," not "GPU done
with this frame." Each surviving site gets a comment explaining what
it gates.

## What gets replaced, by current file

- `vk/ops/mod.rs::run_one_shot_op` — gone. Replaced by
  `PaintBatch::append_with(&mut self, |cb| record_…)`. The recorders
  themselves (`fill`, `copy`, `image`, `render`, `text`, `traps`)
  keep their record-into-CB bodies; only the wrapper changes.
- `kms/backend.rs::composite_and_flip` — split. The scheduling part
  (which outputs composite this tick, skip-if-pending logic) moves
  into the new scheduler that creates `OutputFrame`s. The actual
  draw moves into `OutputFrame::record`. The page-flip handoff stays
  where it is and reads its sync primitives off the `OutputFrame`.
- `kms/backend.rs::screen_dirty` — gone. Replaced by per-output
  generations on each `OutputLayout` (or wherever per-output state
  lives — `OutputLayout` is the natural home).
- `vk/compositor.rs::CompositorPipeline.descriptor_pool` — moves
  into `OutputFrame`. The pipeline keeps shader/layout state; the
  per-frame descriptor sets live with the frame that owns them.
- `vk/copy_scratch.rs`, `mask_scratch.rs`, `gradient.rs`, `glyph.rs`
  scratch buffers — their "single buffer reused after waitIdle"
  shape becomes "allocate from `PaintBatch::scratch`, retire with
  the batch." Their `Drop` still drains (lifetime, off hot path).
- `dst_readback.rs` — `GetImage` cannot return until the GPU has
  written. The targeted-fence model in the dead-end spec is still
  right: this op gets its own small `VkFence` and waits on just
  that fence, not the whole queue. Sits outside the `PaintBatch`
  flow because it's a synchronous-reply request.

## Phasing

Each phase ends with `cargo test`, `just xts-yserver`, and
`just rendercheck-yserver` green. The order is correctness-first,
performance-second.

1. **Probe + scaffolding.** Add the capability probe. Introduce
   `PaintBatch`, `OutputFrame`, `InFlight`, and per-output
   `DirtyGeneration` as types with no callers. Land
   `OpsCommandPool` alongside its replacement; both exist briefly.
2. **Cut over composite scheduling.** Move `composite_and_flip`'s
   per-output loop to `OutputFrame` creation + `InFlight` push.
   Sync inside is *still* `vkQueueWaitIdle` after paint — recorders
   haven't migrated yet. This phase proves the in-flight machinery
   and per-output dirty generations are sound under XTS and the
   target WMs without touching the paint hot path.
3. **Migrate recorders to `PaintBatch`.** One family at a time
   (fill → copy → image → render → text → traps). After each
   family, the family's `run_one_shot_op` calls and their associated
   scratch drains are gone. Resource ownership transfers to the
   batch as the family migrates — no "all recorders eager, drain
   late" intermediate state.
4. **Sync rework.** Replace `vkQueueWaitIdle` between paint and
   composite with the timeline-semaphore wait. KMS handoff converts
   to binary SYNC_FD. After this phase, the hot-path waitIdle is
   gone.
5. **`GetImage` targeted fence.** Last of the per-op drains, lives
   off the hot path but matters for correctness once it stops
   getting "free" sync from a global drain.
6. **Re-verify the flicker.** Run the dual-output Polaris workload.
   If flicker persists, the fix is a separate scheduling change to
   the skip-if-pending logic — out of scope for this rework, but
   newly tractable because `gen_at_last_present` is the right
   primitive to reason about it.

Each phase is bisectable. The architecture survives if (4) lands
without (5); we just keep the readback drain a little longer.

## Out of scope, on purpose

- **`PaintBatch` per-window or per-pixmap split.** Mirrors are
  global; one batch keeps the dependency model trivial. Per-target
  batches are an optimisation only if profiling shows the shared
  CB is the bottleneck.
- **Tighter pipeline barriers.** Conservative `ALL_GRAPHICS` masks
  in the migrated CB are fine until measured.
- **Damage-clipped composite.** Same composite shader, full-output
  draws. `FB_DAMAGE_CLIPS` and finer damage are a separate
  follow-up after sync is correct.
- **A unified test for "no `waitIdle` on hot path."** A grep
  doesn't catch transitive cases. The signal is XTS green + no
  GPU saturation in the symptom workload.

## Required invariants

Beyond the standard ones (no hot-path `vkQueueWaitIdle`; no resource
reuse before its retirement point; binary-semaphore cardinality;
KMS in-fence on every GPU-dependent commit), one invariant has bitten
the previous design hard enough to call out explicitly:

**Any request that needs CPU-visible pixels or externally-visible
GPU completion must force the current `PaintBatch` to submit (and,
where needed, wait on a targeted fence or export a sync fd) before
returning to the client.**

Concrete request paths this covers — non-exhaustive, but the survey
must enumerate them all before phase 3:

- `GetImage` and the host-readback path through `dst_readback.rs`.
- `CopyArea` / `CopyPlane` chains where a source pixmap was drawn
  earlier in the same batch and the destination is sampled by a
  later op or read back. In-CB barriers handle the GPU-side
  ordering; the boundary case is when the *next* request needs
  CPU-visible state.
- DRI3 buffer presentation (`PresentPixmap`) and any path that
  exports a fence fd to a client.
- SYNC-extension fence triggers tied to drawing completion.
- Explicit protocol-level barriers (`GetInputFocus` round-trips
  used by toolkits as ad-hoc barriers — these don't force submit
  themselves, but any request earlier in the same batch that
  *did* require it must already have flushed).

This is a hard invariant, not a phase-1 survey item. Each request
handler that crosses the boundary names the flush reason it triggers
(see "Shape of the new code → `PaintBatch`" above) and asks the
scheduler to honour it before returning.

## Open questions

- **`gen: u64` overflow.** 2^64 frames at 1kHz is 5e14 years.
  Not a problem. Mentioned only to pre-empt review.
- **Recorder error paths.** Today a recorder error returns before
  submit and nothing was queued. Under `PaintBatch`, an error
  mid-batch leaves partial commands in the CB. The batch must
  either roll back to a recorded save point or fail the whole
  frame. Preference: fail the frame, bump dirty generations for
  affected outputs so the next composite tries again. Drawing
  errors on the KMS backend are diagnostic-only today.

## Status

This HLD has been cross-reviewed against the parallel codex
proposal (`2026-05-12-kms-vulkan-frame-ownership-codex.md`). The
two converge on the same primitives and sync model; codex's
review tightened five points which are folded in above:

- two-stage retirement (GPU completion vs KMS release) in
  `InFlight`,
- explicit `PaintBatch` flush reasons (not just output dirtiness),
- old-∪-new region dirty propagation on geometry changes,
- `BoPhase` is kept and aligned with `OutputFrame`, not flattened,
- request paths that need CPU-visible pixels or external sync
  promoted from open question to required invariant.

Next step is a phase-1 plan: scaffolding for the new types,
per-output dirty generations, two-stage retirement, all with
existing `vkQueueWaitIdle` calls left in place. XTS green at the
end of phase 1 proves the lifetime model independently of any
sync rewrite.
