# Rendering re-architecture - phase 3 replacement plan

Date: 2026-05-12
Status: proposed replacement for `2026-05-12-rendering-rearchitecture-phase3.md`
Author: Codex

## Recommendation

Do not implement the current phase 3 plan as written.

The old plan batches command recording first and assumes the existing recorder
closures can be moved from `run_one_shot_op` into a delayed-submit
`PaintBatch`. That is the wrong phase boundary. The current renderer has many
resources whose correctness depends on "the GPU has consumed this before the
function returns":

- `OpsStaging` is reused by PutImage/readback/mirror-upload paths.
- `MaskScratch` uploads through a reusable staging buffer at offset 0.
- `GlyphAtlas` uploads through a reusable staging buffer.
- RENDER uses `RenderPipelineCache::reset_descriptors`, so descriptor reuse is
  safe only while each operation waits for the queue.
- Several recorders update CPU-side image layout state while recording.

If several such operations are recorded into one command buffer and submitted
later, earlier commands can observe overwritten staging bytes, reset descriptor
sets, or layout state that says work happened when the batch was never
submitted. Phase 3 therefore has to make batch-owned resource lifetime real
before it migrates most recorders.

This plan keeps the high-level destination from
`docs/superpowers/specs/2026-05-12-rendering-rearchitecture-hld.md`, but splits
the work so every phase has a narrow correctness invariant.

## Goal

Introduce the destination-shaped `PaintBatch` resource model without changing
the hot paint paths wholesale. After this phase:

- a paint batch can own command buffers, upload ranges, descriptor allocations,
  and retire-time resources;
- upload bytes recorded into a batch remain stable until GPU retirement;
- paint-side descriptors recorded into a batch are not reset or reused before
  GPU retirement;
- callers have explicit batch flush reasons independent of output dirtiness;
- only audited safe paint operations are allowed to append to the batch.

Removing the hot-path `vkQueueWaitIdle` from every paint family is not the
phase 3 acceptance criterion. That becomes a sequence of smaller migrations
after the batch ownership model exists.

## Non-goals

- No timeline-semaphore fan-out yet. A close-time queue wait may remain as a
  temporary bridge, but the ownership API must already look retire-aware.
- No GetImage targeted-fence rewrite.
- No KMS sync-file or `OUT_FENCE_PTR` rework beyond preserving phase 1/2
  behavior.
- No global rewrite of all `run_one_shot_op` call sites.
- No migration of PutImage, mirror upload, mask upload, glyph upload, gradients,
  text, traps, or RENDER until their staging/descriptor dependencies are moved
  into batch-owned resources.

## Design rule

Treat `PaintBatch` as the owner of all resources needed by commands recorded
into it. If a command will read a buffer, descriptor set, scratch image, image
view, sampler, or temporary allocation after recording returns, that object must
either:

- be owned by the batch, or
- be proven immutable and longer-lived than every in-flight batch that can use
  it.

Anything else stays on `run_one_shot_op`.

## Phase 3A - batch resource ownership

### Task 1: replace the shell `PaintBatch` with a retire-aware shape

Files:

- `crates/yserver/src/kms/scheduler/paint_batch.rs`
- `crates/yserver/src/kms/scheduler/mod.rs`
- `crates/yserver/src/kms/scheduler/in_flight.rs`

Implement:

- batch states: `Idle`, `Recording`, `Closed`, `Submitted`, `Retired`,
  `Poisoned`;
- lazy command-buffer allocation from the existing ops command pool;
- a list of retire resources owned by the batch;
- an explicit `BatchFlushReason` enum:
  - `VisibleComposite`
  - `Readback`
  - `ExternalSync`
  - `ProtocolBarrier`
  - `SizeLimit`
  - `LatencyLimit`
  - `Shutdown`
- `flush_if_needed(reason)` on the scheduler/backend boundary.

Keep the temporary submit path allowed to wait idle at batch close, but do not
free batch resources through ad hoc local scope. Route cleanup through the same
retirement API the later timeline implementation will use.

Acceptance:

- unit tests cover state transitions and double-close/double-retire behavior;
- dropping a recording or poisoned batch cannot submit it implicitly;
- a batch with no recorded work allocates no Vulkan command buffer.

### Task 2: add `BatchUploadArena`

Files:

- new: `crates/yserver/src/kms/scheduler/batch_upload_arena.rs` or
  `crates/yserver/src/kms/vk/batch_upload_arena.rs`
- `crates/yserver/src/kms/scheduler/paint_batch.rs`

Implement a host-visible upload allocator owned by `PaintBatch`.

Required behavior:

- append-only allocations within a batch;
- stable `(buffer, offset, size)` ranges until the batch retires;
- alignment suitable for Vulkan buffer/image copies;
- chunk growth when the current chunk is full;
- chunk cleanup only on batch retirement;
- no reuse of `OpsStaging` for delayed-submit paint work.

Acceptance:

- unit tests prove two allocations in one batch do not alias;
- an allocation after chunk growth leaves previous allocations valid;
- test or debug assertion proves the arena never hands out an offset that can
  be overwritten while the batch is open.

### Task 3: add paint descriptor ownership

Files:

- `crates/yserver/src/kms/scheduler/paint_batch.rs`
- `crates/yserver/src/kms/vk/render_pipeline.rs`
- any pipeline module with paint-side descriptor allocation/reset

Inventory every paint-side descriptor pool, starting with
`RenderPipelineCache::reset_descriptors`. Add a batch-owned descriptor arena or
per-batch descriptor pools for any descriptors that can be bound by commands
recorded into a delayed-submit batch.

Do not migrate RENDER yet. This task only creates the allocation path and leaves
existing synchronous code intact.

Acceptance:

- no batched recorder can call a pipeline-level descriptor reset that
  invalidates earlier commands in the same batch;
- descriptor pools used by a batch are released only at batch retirement;
- old synchronous paths still work.

### Task 4: define batch error and layout-state policy

Files:

- `crates/yserver/src/kms/scheduler/paint_batch.rs`
- layout-owning Vulkan image/drawable modules as needed

Recording currently mutates CPU-side layout state while building commands. A
failed append can therefore leave CPU state ahead of GPU reality.

Implement one of these policies before broad migration:

- preferred: defer layout-state commits until the batch is accepted for submit;
- acceptable bridge: mark the batch `Poisoned`, discard it, and force dirty
  revalidation/full repair before the drawable is used again;
- limited bridge: only allow batched recorders that cannot fail after layout
  state mutation, with assertions documenting that property.

Acceptance:

- tests cover failed append behavior;
- no caller continues using a poisoned batch;
- the chosen policy is documented in `paint_batch.rs`.

## Phase 3B - migrate only audited non-upload paint

Move one family at a time. Each candidate must pass this checklist before any
call-site rewrite:

- it does not read host staging memory after recording returns, or it allocates
  from `BatchUploadArena`;
- it does not depend on descriptor reset/reuse outside the batch descriptor
  arena;
- it does not reuse scratch images or buffers whose contents can be overwritten
  before batch retirement;
- layout mutations obey the phase 3A policy;
- it has a fallback path that can remain synchronous if the audit fails.

Recommended first candidates:

- simple solid fill paths;
- copy paths that only read/write persistent drawable images and do not require
  host staging or reusable scratch.

Do not migrate in this phase:

- PutImage;
- mirror upload via `DrawableImage::record_upload_rect`;
- `MaskScratch::upload_r8`;
- glyph atlas upload;
- gradients;
- traps;
- RENDER;
- text.

Acceptance:

- only audited families use `PaintBatch::append`;
- all migrated families have smoke coverage;
- no migrated family relies on `vkQueueWaitIdle` for staging, descriptors, or
  scratch lifetime.

## Phase 3C - migrate upload-backed paint

After `BatchUploadArena` exists, convert upload users from reusable staging to
batch allocations.

Order:

1. PutImage into drawable mirrors.
2. Mirror upload helper paths.
3. `MaskScratch` upload.
4. Glyph atlas upload.
5. Gradient upload/build helpers.

Acceptance:

- two PutImage requests recorded before one submit produce distinct staged
  contents;
- two mask uploads in one batch cannot overwrite each other's source bytes;
- glyph uploads in one batch cannot overwrite earlier glyph source bytes;
- upload chunks retire with the batch, not with local function scope.

## Phase 3D - migrate descriptor/scratch-heavy paint

After paint descriptor ownership and upload arenas are used by the underlying
helpers, migrate:

1. traps;
2. RENDER;
3. text.

Acceptance:

- `RenderPipelineCache::reset_descriptors` is not called in a way that can
  invalidate descriptors recorded into an open or in-flight batch;
- mask, gradient, glyph, and render descriptors are batch-owned or otherwise
  proven stable;
- all migrated families can be recorded multiple times before one submit.

## Phase 4 handoff

At the end of this replacement phase, some paint work may still be synchronous.
That is acceptable. The important handoff to phase 4 is that any batched work
already follows the destination lifetime model:

- batch resources are owned by `PaintBatch`;
- output composite resources are owned by `OutputFrame`;
- retirement flows through `InFlight`;
- queue idle is a temporary submit/retire implementation detail, not the
  resource-lifetime contract.

Phase 4 can then replace the close-time wait with timeline-semaphore ordering
without also redesigning staging, descriptors, scratch, and layout failure
semantics.

## Verification

Run after each task:

```bash
cargo +nightly fmt
cargo clippy
cargo test
```

Hardware smoke after every migration task:

```bash
RUST_LOG=debug cargo run --bin yserver
```

Manual cases to keep exercising:

- repeated PutImage before a visible composite;
- text drawing with multiple glyph uploads in one frame;
- RENDER masks and gradients;
- multi-output movement where old and new window regions dirty different
  outputs;
- GetImage/readback after offscreen pixmap drawing with no dirty output.

