# KMS Vulkan frame ownership model — high-level restructuring

Date: 2026-05-12
Status: proposal
Author: codex

## Purpose

This document proposes a restructuring of the KMS Vulkan rendering
model. It is intentionally higher-level than an implementation plan.
Its goal is to define the ownership and synchronization invariants the
renderer must have before removing hot-path `vkQueueWaitIdle` calls.

The current paint/composite sync work identified the right symptom:
per-paint-op queue drains make bursty clients serialize the CPU and GPU.
However, repeated review found additional blockers in descriptor-pool
lifetime, scratch/staging reuse, image layout assumptions, global dirty
state, and per-output flip drift. These are not independent issues.
They are all symptoms of the same missing model:

**Submitted GPU work must own every resource it may still touch until a
known frame retirement point proves that reuse, reset, destruction, or
re-signaling is safe.**

## Current problem shape

The current renderer relies on a simple but expensive invariant:
after each paint op returns, the graphics queue is idle. That makes many
otherwise unsafe operations appear safe:

- command buffers can be freed immediately;
- descriptor pools can be reset without tracking in-flight users;
- scratch buffers/images can be reused or destroyed immediately;
- drawable mirrors can be sampled by composite immediately;
- image layouts can be reasoned about as if operations were synchronous;
- staging buffers can be resized after a local wait;
- composite can assume paint has completed without an explicit wait.

Once the waits are removed, these assumptions break. Replacing only the
paint → composite wait with semaphores is not sufficient; the renderer
needs a frame ownership model.

## Design Direction

Keep the existing codebase and most low-level rendering logic, but
replace the render scheduling and lifetime layer.

Keep:

- Vulkan instance/device setup and feature probing.
- Shader modules and pipeline setup where compatible.
- Drawable image allocation and metadata, with stricter layout/state
  ownership.
- Scanout BO allocation and the idea of a BO phase/state machine.
- Atomic KMS commit, `IN_FENCE_FD`, and `OUT_FENCE_PTR` plumbing.
- Existing draw-op implementations as semantic references.

Replace or heavily rewrite:

- `run_one_shot_op` and APIs that mean "record, submit, wait idle,
  then return."
- shared compositor descriptor-pool reset on the hot path;
- scratch/staging lifetime guarded by queue-idle waits;
- global `screen_dirty`;
- output skip semantics that lose dirty state;
- implicit "mirror is sampleable because previous op waited" contracts.

## Core Objects

### `RenderScheduler`

Owns the current paint batch, in-flight frame list, and per-output frame
state. It is the only layer allowed to submit renderer command buffers
on the hot path.

Responsibilities:

- collect paint ops during request/event processing;
- decide which outputs can composite at the flush point;
- submit paint and composite work in dependency order;
- export sync files for KMS where needed;
- retire completed frames and recycle resources.

### `PaintBatch`

A per-flush batch of Vulkan commands that mutates drawable images
directly. It replaces per-op submit/wait.

Properties:

- one or more command buffers;
- a set of touched drawable images;
- layout transitions needed by each op;
- optional upload/readback side work;
- signal point: `paint_done(frame_id)`.

The first implementation should use one graphics queue and one batch per
event-loop flush. Multi-queue upload work is deferred.

### `OutputFrame`

One composited frame for one output.

Properties:

- output index and dirty generation being presented;
- target scanout BO;
- composite command buffer;
- descriptor arena or descriptor pool instance;
- scratch resources used by this frame;
- wait dependency on the relevant paint batch;
- signal dependency exported to KMS;
- KMS release fence or timeline point;
- damage region used for rendering and/or `FB_DAMAGE_CLIPS`.

An `OutputFrame` owns all GPU-visible resources that cannot be reused
until its submit has finished. A scanout BO may remain unavailable
longer, until KMS releases it.

### `ResourceRetireQueue`

Tracks resources that were removed from normal ownership but are still
possibly in use by submitted GPU work.

Examples:

- descriptor pools/arenas;
- command buffers;
- scratch buffers/images;
- staging buffers replaced by resize;
- drawable images for destroyed windows/pixmaps;
- image views and samplers;
- pipeline resources if hot-reloaded.

Each entry is tagged with a renderer timeline point or fence. The queue
reclaims entries only after that point is complete.

### `OutputDamageState`

Per-output dirty state replaces global `screen_dirty`.

Properties:

- current damage region;
- dirty generation;
- last submitted generation;
- last presented generation;
- whether a flip is pending;
- whether a catch-up composite is required when the pending flip
  retires.

If an output is dirty but cannot composite because a flip is pending,
its damage and generation remain pending. Skipping an output must never
clear its dirty state.

## Frame Lifecycle

### 1. Request/Event Processing

Protocol handlers record paint intent into the current `PaintBatch`.
They do not submit GPU work and do not wait for the queue.

Paint ops still update X11-visible resource state synchronously, but GPU
completion is represented by the batch's future signal point.

### 2. Flush Point

At the event-loop quiescent point:

1. Determine which outputs are dirty and not blocked by a pending flip.
2. Submit the current `PaintBatch` if non-empty.
3. For each ready dirty output, create an `OutputFrame`.
4. Each `OutputFrame` waits on the paint batch if it samples drawables
   touched by that batch.
5. Submit composite work.
6. Export the composite completion as a sync file or timeline-backed
   handoff accepted by KMS.
7. Atomic commit the output with the rendered BO and damage metadata.

Outputs that are dirty but blocked keep their damage pending.

### 3. Page Flip / Presentation

On page-flip completion:

- the queued BO becomes current/on-screen;
- the previously current BO begins retirement;
- output generation state advances;
- pending dirty state may immediately schedule another composite;
- KMS release fences are attached to the appropriate BO lifecycle.

### 4. GPU Retirement

Independently of page flips, renderer timeline/fence completion retires
GPU-only resources:

- descriptor arenas are reset;
- command buffers are reused;
- scratch and staging resources are returned to pools or destroyed;
- destroyed drawable images are freed;
- image views/samplers no longer referenced by in-flight work are freed.

This retirement must not depend on `vkQueueWaitIdle` in the hot path.

## Synchronization Model

Use one graphics queue initially.

Primary path:

- internal renderer ordering uses timeline semaphores where available;
- binary semaphores or sync files are used at API boundaries that
  require them;
- KMS receives a sync file for the primary plane in-fence;
- KMS release is represented by the BO/page-flip state and any returned
  release fence.

If timeline semaphore export to `SYNC_FD` is not supported, bridge with
an exportable binary semaphore for the KMS handoff. Do not require
timeline-to-sync-file export for the initial design.

Binary semaphore rule:

- every binary signal must have exactly one consumer;
- never signal a reusable binary semaphore twice without its prior
  payload being consumed/exported/reset according to Vulkan rules;
- if a paint batch fans out to multiple output composites, either use a
  timeline wait or signal one binary semaphore per output consumer.

## Image Layout Model

Drawable images need explicit tracked state.

The renderer should know, for each drawable image:

- current known layout;
- last writer batch/timeline point;
- whether it is safe to sample;
- whether pending destruction is blocked by in-flight use.

The composite pass must not assume every drawable is already in
`SHADER_READ_ONLY_OPTIMAL` because a previous paint op waited. The
transition to sampleable layout belongs either to the paint batch or to a
pre-composite barrier with the correct wait dependency.

## Descriptor and Scratch Ownership

Descriptor pools and scratch resources are not global hot-path objects.

Acceptable first implementation:

- one descriptor arena per `OutputFrame`;
- one command buffer per `OutputFrame`;
- scratch allocations borrowed from pools and returned by
  `ResourceRetireQueue`;
- staging buffers grow by allocating replacement storage and retiring
  the old storage after the current timeline point.

Optimization can follow after correctness:

- ring of per-output descriptor arenas;
- pooled command buffers;
- scratch suballocation;
- timeline-driven garbage collection at frame boundaries.

## Relation to wlroots, Mutter, and Hyprland

wlroots is the closest low-level reference for yserver:

- output render pass writes to a swapchain buffer;
- Vulkan renderer uses timeline semaphores internally;
- sync files are imported/exported at DMA-BUF/KMS boundaries;
- KMS atomic commit receives in-fences and damage clips;
- buffer reuse is tied to release/lifetime tracking.

Mutter and Hyprland use GL/EGL rather than yserver's Vulkan path, but
they confirm the same compositor-level shape:

- per-output frame scheduling;
- damage-driven rendering;
- output buffer lifecycle;
- atomic KMS commit;
- direct scanout where possible;
- presentation feedback pacing future frames.

The lesson is not to copy their renderer APIs. The lesson is that frame
ownership, damage, and presentation feedback are the renderer's spine.

## Migration Strategy

### Phase A — Make the model explicit without changing behavior

- Add `OutputDamageState` and stop relying on global `screen_dirty`.
- Add in-flight frame/resource bookkeeping structs.
- Keep existing waits temporarily.
- Add assertions that skipped outputs retain dirty generations.
- Identify every hot-path `queue_wait_idle` and classify it:
  synchronization, readback, teardown, or temporary compatibility.

### Phase B — Introduce frame-owned composite resources

- Move compositor descriptor pool ownership into per-output-frame or
  per-BO arenas.
- Ensure descriptor reset cannot affect submitted frames.
- Tie command-buffer reuse to output-frame retirement.
- Keep paint ops synchronous during this phase.

### Phase C — Batch paint

- Replace `run_one_shot_op` with paint batch recording.
- Submit the batch at flush point.
- Track touched drawable images and layout transitions.
- Preserve blocking behavior only for true readback APIs.

### Phase D — GPU-side paint → composite sync

- Composite submissions wait on the paint batch signal.
- KMS handoff uses exportable sync fd from composite completion.
- Remove hot-path paint/composite `queue_wait_idle`.

### Phase E — Retire resources by timeline/fence

- Scratch/staging resize and destruction move to `ResourceRetireQueue`.
- Drawable destruction becomes deferred when in-flight.
- Pipeline/descriptor/image resources are reclaimed only after their
  retirement point.

### Phase F — Efficiency follow-ups

- Composite damage clipping.
- KMS `FB_DAMAGE_CLIPS`.
- GBM+modifier primary scanout allocation.
- Direct scanout.
- Cursor-plane policy cleanup.
- Multi-queue uploads if profiling justifies them.

## Non-goals

- Rewriting the whole server.
- Replacing every draw op at once.
- Adding multi-threaded rendering.
- Solving damage clipping before synchronization correctness.
- Making direct scanout part of the wait-removal milestone.

## Required Invariants

1. No hot-path renderer API may require the whole graphics queue to be
   idle when it returns.
2. No submitted command buffer may reference a descriptor set, image
   view, sampler, buffer, image, or scratch allocation that can be reset
   or destroyed before the command buffer's retirement point.
3. No output may lose damage because another output was ready first or
   because this output had a pending flip.
4. No binary semaphore may be signaled without a matching consumer, and
   no binary semaphore may be reused while it has an outstanding payload.
5. Every KMS commit that depends on GPU rendering must receive a valid
   in-fence or an equivalent proven synchronization path.
6. BO reuse must require both renderer completion and KMS release.
7. Readback APIs may block, but those waits must be isolated from normal
   paint/composite/present cadence.

## Open Questions

- Should the internal renderer timeline be global or per-output? A
  global timeline is simpler; per-output timelines may make diagnostics
  clearer but are not required.
- Should `PaintBatch` be one command buffer per flush or a small vector
  of command buffers grouped by operation family? Start with one unless
  command-buffer size or reset behavior says otherwise.
- Should drawable image layout be tracked centrally on `DrawableImage`
  or in a separate renderer state map? Central tracking is easier to
  audit; a separate map may reduce borrow friction.
- How much of `BoPhase` should survive unchanged? The idea is sound, but
  it should be aligned with `OutputFrame` and release-fence ownership.

## Summary

Do not throw away the KMS Vulkan backend wholesale. Keep the low-level
pieces and the protocol-visible rendering semantics. Replace the
scheduling and lifetime layer with an explicit frame ownership model.

The wait-removal milestone should be considered complete only when the
renderer no longer relies on GPU-idle side effects for correctness.
