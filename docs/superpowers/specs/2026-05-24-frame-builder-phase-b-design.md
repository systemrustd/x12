# Frame-builder Phase B — design

**Status:** draft. Captured 2026-05-24 from the Phase A close-out
(`feature/frame-builder-submit-rate` merged on 2026-05-24) plus the
2026-05-23 bee MATE-load freeze data captured under Phase A.
Supersedes the "Phase B — deferred op-list FrameScheduler (sketched,
not detailed)" section in
`docs/superpowers/specs/2026-05-23-frame-builder-submit-rate-design.md`.

**Framing:** Phase A collapsed *consecutive* `vkQueueSubmit2` calls
into one ioctl with N CBs. Phase B collapses the *per-op CB* model
itself: one open frame records every paint primitive into a compact
op list, frame-close replays the list into ONE primary CB, and the
KMS handoff submits exactly one CB per frame per queue.

**Load-bearing design goal:** *one scanout opportunity consumes one
recorded frame's worth of X11 work, in one VkCommandBuffer, in one
vkQueueSubmit2 call per affected queue.* Phase A relaxed the per-op
submit; Phase B relaxes the per-op CB. The discipline that controls
*how* GPU work is recorded changes — the data model (Drawable /
storage / scene graph) stays.

**Phase B is also the fix path for the bee RDNA2/RADV MATE-load
freeze** that blocked Phase A's bee close: see § "Bee fault →
Phase B disposition" below.

## Why this exists

Phase A's bee/iMac/yoga/fuji captures show three independent
limits in the post-Phase-A submit shape:

1. **bee (Ryzen 9 6900HX / RDNA2 / RADV / Arch Mesa-current):**
   any cap ≥ 2 reproduces a RADV GPUVM TCP protection fault
   (`GCVM_L2_PROTECTION_FAULT_STATUS=0x701031`,
   `ERROR_DEVICE_LOST`, longest op `op133 (RENDER) at ~1.98 s`)
   during a `composite_glyphs` burst on MATE load. Three green
   analogues (yoga / Adreno-Turnip, iMac / Polaris-RADV, fuji /
   Intel-ANV) on the same `189e8dd` commit triangulate the cause
   to RDNA2 RADV codepaths × Arch Mesa-current. The Phase A
   "abandoned submit-shape experiment" — reshape one
   `VkSubmitInfo2{N CBs}` → one `vkQueueSubmit2{N submits × 1 CB}`
   — moved the threshold but still froze, so the fault is the
   *multi-CB execution shape within a single queue submission*,
   not the `VkSubmitInfo2` packing detail. The only structural
   way out is one primary CB per submit; that is Phase B's
   shape.
2. **iMac 19,2 (Polaris / GCN4 / RADV / Ubuntu Mesa):** Phase A
   `max_size`-flush share 50–55 % of all flushes — cap=16 is too
   low for AMD's batch shape on this workload. Phase B's "frame"
   is the natural unit, not a numeric cap.
3. **Universal (every platform):** `submit_group_size_max_in_window
   > cap` telemetry anomaly on yoga + iMac + fuji. A real
   telemetry-or-cap-check defect that Phase A inherited and
   bracketed. Phase B's frame-CB removes the per-op-class
   batching machinery entirely; the anomaly's cause goes with it.

Phase B closes the remaining gap to Xorg/wlroots (~1–5
submits/frame) by structurally removing the per-op CB.

## Bee fault → Phase B disposition

The bee fault is the load-bearing failure mode driving Phase B's
sequencing. Captured separately because the spec needs an
explicit rebuttal "Phase B fixes bee" so codex review can audit
the chain.

**Observation chain (from `docs/status.md`
§ "2026-05-23 bee MATE-load freeze"):**

- `cap=1` (every paint op flushes immediately): MATE loads,
  yserver survives a full drag session. This is Phase-A-with-
  zero-batching; equivalent to the pre-Phase-A per-op submit
  cadence except for the wrapper.
- `cap=2`: RADV GPUVM TCP protection fault during MATE load.
  First regular multi-CB submit-group shape.
- Reshaping `flush_submit_group` from one `VkSubmitInfo2`
  containing N CBs → one `vkQueueSubmit2` containing N
  `VkSubmitInfo2{1 CB}` substructures: `cap=16` reaches the
  MATE desktop, but freezes inside a `composite_glyphs` burst
  later with the same RADV GPUVM fault.

**Disposition.** The fault triggers on *multiple command-buffers
executing within one queue submission*. Whether that's
packaged as N `CommandBufferSubmitInfo`s inside one
`VkSubmitInfo2`, or as N `VkSubmitInfo2{1 CB}` inside one
`vkQueueSubmit2`, RDNA2 RADV faults the same way on this
workload. Three green analogues running the same code on
non-RDNA2 hardware (or RDNA2 with older Mesa, modulo the iMac
Polaris distinction) rule out a yserver-side barrier bug — a
genuine yserver-side missing barrier would fault iMac too.

Phase B's structural answer: every queue submission contains
exactly ONE primary CB. The frame-CB consolidates all paint +
glyph upload + scene-compose work into that single CB, separated
by full pipeline barriers between ops. Multi-output emits N
render passes inside that one CB (see § "Multi-output topology")
rather than N CBs. This removes the failure mode without needing
to characterise the underlying RADV/firmware bug.

**Phase B exit criterion for bee:** boot MATE, drag for 30 s,
no `ERROR_DEVICE_LOST`, no GPUVM faults. The same telemetry
gate the other three platforms passed under Phase A.

## Phase B architecture

### Frame lifecycle

A `FrameBuilder` is the new top-level abstraction on
`RenderEngine`. It has three states:

```
   Closed  ─open_for_paint────▶  OpenForPaint  ─close_into_cb────▶  Closed
     ▲                              │  │
     │                              │  └─attach_compose──▶  ClosingWithCompose
     │                              │                              │
     └────────────close_into_cb─────┘                              │
     ▲                                                             │
     └────────────close_into_cb────────────────────────────────────┘
```

- **Closed** (idle). No allocations. The hot path for X11
  workloads that don't drive paint (event-only requests, idle).
- **OpenForPaint.** A paint entry point (`render_composite`,
  `composite_glyphs`, `render_fill_rectangles`, `cow_copy_area`,
  `put_image`, `render_traps_or_tris`) detected no open frame
  and opened one. The frame owns a fresh `FenceTicket` (the
  *frame ticket*) and accumulates `RecordedOp`s, pinned
  resources, and a per-drawable layout overlay. Subsequent paint
  ops append into the open frame instead of submitting their
  own CB.
- **ClosingWithCompose.** Scene-compose joined the open frame.
  Triggered by `maybe_composite` when its compose-eligibility
  gate fires AND `frame.is_open()`. The compose-eligibility
  gate is `(scene_structure_dirty || any_output_has_pending_
  failed_submit_or_retry()) && has_output_ready_for_submit()`
  — see § "Per-output partial-failure handling" for the
  partial-failure-retry rationale. Compose appends its own
  `RecordedOp::Compose` entries (one per ready output) onto
  the same op list, then the frame closes into a single
  primary CB that contains paint draws + barriers + N
  rendering instances, submits once with `KMS_FB_IN_FENCE_FD`
  exported per output, and signals the frame ticket.

The Closed → OpenForPaint transition is *lazy* — a request
handler that never touches the paint path never allocates a
frame. The OpenForPaint → ClosingWithCompose transition is
driven by the existing `maybe_composite` tick; idle no-pageflip
frames close via the timeout trigger (see § "Frame close
triggers").

### Op representation — symbolic recording with append-time pinning

Spec § Phase B open question 1. The choice is between
*symbolic* (store params, resolve resources at close) and
*prepared* (snapshot resources at append). Phase B uses a
**hybrid**: a compact `RecordedOp` enum carries only the
*parameters* of each op (rects, colours, glyph keys, pipeline
choices), the raw `vk::DescriptorSet` handle resolved at op
append time, and typed indices into the frame's Arc-pinned
resource vectors (`PinnedStagingIdx`, `PinnedSyncObjectIdx`,
etc.) for resources that ARE Arc-tracked. Append-time **pins
each resource via the mechanism appropriate to it** (Arc
clone for Arc-tracked, generation-watermark snapshot for
DescriptorPoolRing — see § "Frame-wide resource pinning"). A
later op or close-time replay cannot observe a freed
view/descriptor/buffer. Close-time replay reads the op params
and records the Vulkan commands using the saved descriptor-
set handle + pinned-Arc lookups directly.

```rust
#[derive(Debug)]
pub(crate) enum RecordedOp {
    RenderComposite(RecordedRenderComposite),
    CompositeGlyphs(RecordedCompositeGlyphs),
    RenderFill(RecordedRenderFill),
    RenderTraps(RecordedRenderTraps),
    CowCopyArea(RecordedCowCopyArea),
    CopyArea(RecordedCopyArea),
    PutImage(RecordedPutImage),
    GlyphUpload(RecordedGlyphUpload),
    LayoutTransition(RecordedLayoutTransition),
    Compose(RecordedCompose),       // per-output entry
}
```

Each variant's payload includes (a) the parameters needed to
record the Vulkan commands at close time, (b) the indices into
the frame's pin vectors for any heap resource the replay needs,
and (c) the destination/source `DrawableId`s for layout overlay
lookups. Variants are intentionally plain Rust types — no
special `#[repr]` packing. The op list churns per-frame at MATE
drag rates, but layout micro-opts wait for the Phase B perf
pass that runs *after* the structural change lands (spec §
Phase B open question 1 — "rerun the size profile after the
structure is in tree, not before").

**Why symbolic with pinning, not pure-symbolic or pure-prepared:**

- Pure-symbolic ("look up the descriptor set at close") risks
  the descriptor set being freed between append and close. The
  DescriptorPoolRing (Stage 5 Task 4 layer 1) already gates
  recycle on the frame ticket via Phase A's
  `descriptor_ring_does_not_reset_in_use_group` discipline; an
  equivalent ring-watermark snapshot at append-time (see §
  "Frame-wide resource pinning" — descriptor sets are
  generation-tracked, not Arc-tracked) keeps individual
  descriptor sets live across the frame open window.
- Pure-prepared ("record the Vulkan command into a secondary CB
  at append") needs CB pool ownership, layout state, and barrier
  insertion at append time — defeating the deferred-recording
  simplification and reintroducing the multi-CB shape that the
  bee fault demands we structurally remove.
- Hybrid: parameters are tiny (a few hundred bytes/op worst
  case), Arc-tracked pins are one refcount bump per pinned
  resource per op, generation-watermark pins are one `u64`
  high-water update per pool per op, close-time replay is a
  single primary CB recorded straight-through. The pinning
  surface is bounded per-frame (capped by
  `max_pinned_resources_per_frame` — fail-the-frame guard, not
  silent growth).

### Drawable lifetime — append-time frame-ticket touch

Today's code prevents the FreePixmap-before-flush UAF (the
2026-05-22 Rembrandt iGPU fault chain) by calling
`store.touch_render_fence(id, ticket.clone())` on every
drawable a pending CB references AT THE MOMENT the CB starts
referencing it — `engine.rs:2862` is the load-bearing site
(dst + src + mask all touch the batch ticket the moment the
batch opens). `DrawableStore::decref` (`store.rs:708`) and
`poll_pending_retire` (`store.rs:881`) then gate destruction
on the touched ticket. Phase B's deferred recording opens a
longer "between append and submit" window than Phase A's
batch did; the same UAF guard must move with it.

**Contract.** At each `RecordedOp` append, the frame builder
calls `store.touch_render_fence(id, frame_ticket.clone())` on
EVERY drawable the op will read or write — dst + src + mask +
any clip-source. **The first touch of a given drawable
in-frame snapshots its prior `last_render_ticket`** into the
frame's `touched_drawables: HashMap<DrawableId,
Option<FenceTicket>>` overlay (parallel to the layout
overlay's `(pre_frame, in_frame)` pattern). The frame ticket
is the same one cloned into every retire-gating consumer
Phase A already wires (scene compositor pending-ack list, the
`SubmittedOp` queue, etc.). A `FreePixmap(src)` issued after
the op append but before frame close therefore observes a
non-signaled ticket and goes via the existing
`RetireDecision::PendingFence` path; the storage drops only
after the frame ticket signals.

**Commit on success.** Frame-close-success leaves
`last_render_ticket` pointing at the frame ticket (the same
ticket whose submit just completed); the overlay drops without
write-back. `decref` / `poll_pending_retire` poll the frame
ticket; once it signals, retirement proceeds.

**Rollback on failure.** Frame-close-failure walks
`touched_drawables` and **restores each drawable's
`last_render_ticket` to its pre-frame value** before setting
`renderer_failed`. Without restoration, a drawable's
`last_render_ticket` would point at the frame's
never-signaled fence; `decref` / `poll_pending_retire`
(`store.rs:708` / `store.rs:887`) would observe
`poll_signaled = false` forever and leak storage. With
restoration, retirement sees the pre-frame ticket (either
already-signaled, or signaled when the prior frame retires),
and storage drops normally. Even with `renderer_failed`
set (Phase A's discipline), shutdown teardown
(`KmsBackendV2::shutdown`) still iterates the store and must
not hang on phantom tickets.

**Why ticket-touch instead of pinning the storage via Arc.**
`Drawable::storage` is not Arc-owned today; it's a struct
member of `Drawable`. Wrapping it would require touching the
entire drawable store. The ticket-touch discipline is
load-bearing already (the 2026-05-22 fix added it explicitly
for the per-class batches); Phase B is the same discipline
extended to "one frame" as the unit instead of "one batch".
Implementation cost: one method call per drawable per op
append, in cache (drawables already loaded by the op's
parameter resolution).

**Atlas image.** `V2GlyphAtlas` holds its `vk::Image` as a
raw handle, not in a drawable. Frame-close's success commit
moves the frame ticket onto the atlas as a new
`V2GlyphAtlas::last_render_ticket: Option<FenceTicket>` field;
atlas destruction (only on `KmsBackendV2::shutdown`) gates on
that ticket the same way `DrawableStore::poll_pending_retire`
gates drawable destruction. This is a small new field; the
plan touches `glyph_atlas.rs` directly.

### Glyph upload — speculative atlas overlay

Spec § Phase B open question 2. `composite_glyphs` is the bee
fault site; getting glyph upload right is load-bearing.

Today (`v2/glyph_atlas.rs:283-294`): `pack` advances the shelf
packer and returns a slot; `insert_entry` commits the slot into
the cache; `record_upload` records the upload commands into the
caller's CB. The three steps happen sequentially in
`composite_glyphs`, and the upload CB submits *before* the
draw CB.

Phase B:

- **Pack stays monotonic.** Shelf advance is not transactional.
  A failed frame leaks the packed slot (the cache entry is
  never committed, so the next pack of the same glyph allocates
  a fresh slot). Atlas is 4096² R8 = ~14k glyph budget;
  per-frame leak rate is bounded by the rare-failure regime.
  Worth the simplicity over a tentative-shelf overlay; the
  spec calls this out so reviewers don't ask.
- **Cache insert is transactional.** `pack`-and-record happens
  during op append: the frame builder writes a `RecordedOp::
  GlyphUpload { atlas_xy, atlas_layout_in_frame, staging_pin_idx,
  glyph_key, atlas_entry }` and DOES NOT call
  `insert_entry`. On frame-close success the
  builder iterates `pending_glyph_inserts` and commits each
  into `V2GlyphAtlas::insert_entry`. On frame failure the
  pending list drops — the slot is leaked-but-not-cached, so
  next paint re-packs.
- **Upload and draw share one CB.** The glyph upload commands
  (layout transition + `vkCmdCopyBufferToImage` + back-transition
  to `SHADER_READ_ONLY_OPTIMAL`) record into the frame CB BEFORE
  the `composite_glyphs` draw commands that reference the
  uploaded slot, with a full `PipelineBarrier2` between them
  (no inter-CB sync needed; this is the bee fix). The frame's
  layout overlay tracks the atlas image's "in-frame" layout
  state the same way it tracks drawable layouts (§ "Transactional
  layout state").
- **One frame, many glyph misses.** A glyph-heavy MATE drag may
  miss N glyphs in one frame. All N uploads + barriers + draws
  record into the same frame CB; one submit. Spec target for
  this case: a MATE drag frame with 10–20 glyph misses runs
  in ≤ 1 submit/frame versus Phase A's 10–20 submits/frame.

### Damage accumulation — append-time, not rolled back

`DrawableStore::damage` (`store.rs:781`) tracks both protocol-
level damage (X11 DAMAGE extension state — clients see this
via DamageNotify) and presentation damage (scene compose's
input — drives repaint region selection at
`scene.rs:1328`). Today's paint paths mutate damage as part
of the op; Phase B inherits this exactly:

- **Append-time mutation.** Each `RecordedOp` append calls
  the existing damage mutator at the same time it would have
  under Phase A. The frame builder does NOT defer or shadow
  damage state. Reason: damage reflects what the X11 client
  asked the server to do, not what the server's GPU
  successfully executed — the client's `XRender` /
  `XCopyArea` / `XPutImage` request "happened" the moment the
  server accepted it; subsequent damage events fire on that
  acceptance, before any GPU work.
- **No rollback on frame failure.** A failed frame submit
  doesn't undo the protocol-level state ("client wrote
  pixels; repaint that area"). The damage region is correct;
  the engine just can't render it because `renderer_failed`
  short-circuits subsequent paint, and the only correct
  response is teardown. Restoring damage to pre-frame would
  *lose* a DamageNotify the client has already been told
  about. Out of scope.
- **In-frame compose (B.4+).** When compose joins the frame,
  the compose RecordedOp snapshots damage at frame-close as
  part of its payload (the existing pattern at
  `scene.rs:1328` lifted into `RecordedOp::Compose`
  construction). All paint ops in the same frame have
  already appended, so the compose op sees every contribution
  from the open frame.
- **Cross-frame compose (B.1–B.3).** Legacy compose runs
  after M3 closes the open frame; compose then snapshots
  damage via today's `scene.tick` path, observing committed
  state. Same correctness as today.

### Transactional layout state

Spec § Phase B open question 3. Today
`Drawable::record_layout_transition` (`store.rs:506`) mutates
`storage.current_layout` BEFORE the GPU executes the barrier;
the CPU tracker is single-source-of-truth for what the *next*
op's barrier old-layout should be. Phase B defers the GPU
barrier to close, so the tracker must shift in two places to
stay correct:

1. **FrameLayoutTable.** A `HashMap<DrawableId,
   LayoutOverlayEntry>` on the FrameBuilder. Each entry holds
   `(pre_frame_layout, current_in_frame_layout)`. The first
   `record_layout_transition` for a drawable during the open
   frame snapshots its `storage.current_layout` into
   `pre_frame_layout`, sets `current_in_frame_layout` to the
   target, and appends a `RecordedOp::LayoutTransition` to the
   op list. Subsequent transitions on the same drawable update
   only `current_in_frame_layout` (and append a new
   `LayoutTransition` op with the up-to-date pre-old-layout).
2. **Commit on success.** Frame-close-success walks the overlay
   and writes `current_in_frame_layout` back into each drawable's
   `storage.current_layout`. The store is now consistent with
   what the GPU executed.
3. **Rollback on failure.** Frame-close-failure walks the overlay
   and writes `pre_frame_layout` back. The store is restored to
   its open-frame-start state. **Combined with the Phase A
   choice** (`renderer_failed` is fatal-after-failure), rollback
   only needs to leave the layouts in a non-corrupting state
   long enough for the next entry-point's `renderer_failed`
   short-circuit to fire; subsequent paint ops never run.
   Phase B can still attempt full restoration as a "best-effort"
   so that *diagnostic* paths (`get_image` readbacks during
   shutdown) don't read mid-mutation values, but
   correctness depends only on `renderer_failed`. The atlas
   image's layout follows the same pattern: an
   `atlas_pre_frame_layout` field on the FrameBuilder snapshots
   `V2GlyphAtlas::current_layout` on first glyph-upload-in-frame,
   and the commit/rollback step mirrors `current_in_frame`/
   `pre_frame` semantics.

**Why an overlay, not direct mutation:** the CPU tracker is also
read by non-paint code paths (the existing `current_layout`
reads in `engine.rs` ops that don't open a frame — though there
should be none of those by the end of Phase B). The overlay is
queried via `frame.current_layout_for(drawable_id)` which falls
back to `storage.current_layout` when the drawable isn't in the
overlay; any code path that pre-dates Phase B and reads
`storage.current_layout` directly continues to see correct
data because the open-frame's overlay hasn't committed yet —
its mutations have not actually executed on the GPU.

### Multi-output topology — one CB, N rendering instances, N semaphores

Spec § Phase B open question 4. The frame builder must support
multi-output (silence's dual 2560×1440, future > 2-output
configs) without reintroducing the multi-CB submit shape that
bee faults on.

**Phase B picks option (a) from the spec sketch**: one primary
CB containing N back-to-back dynamic-rendering instances
(`cmd_begin_rendering` … `cmd_end_rendering`, the same API the
current `record_compose_v2` already uses at `scene.rs:2528`),
one per ready output's scanout BO, signaling N binary
semaphores at the end of the CB. Each pageflip ack-semaphore
acquires its output-specific scanout BO via
`KMS_FB_IN_FENCE_FD`. Single-output reduces to N=1 trivially.

Reasoning vs the spec-sketched alternative (b) — N CBs in one
`vkQueueSubmit2`:

- (b) is the multi-CB-per-submit shape; the bee fault rules it
  out. (Phase A's "abandoned experiment" reshaping into "N
  one-CB submits inside one queue_submit2" demonstrated the
  fault survives this packaging.)
- (a) requires the primary CB to know all N output scanout BOs
  at record time. The frame builder learns the BO assignments
  at frame-close from `scene.tick`'s output-readiness sweep
  (the existing logic in `scene.rs:1517 build_scene`); the
  paint half of the frame doesn't depend on BO selection.
- (a) trades rendering-instance switching inside one CB for
  CB-switching inside one submit. Dynamic-rendering instance
  switching is cheaper than CB switching on every
  implementation tested (no allocator overhead, just
  `cmd_end_rendering` + `cmd_begin_rendering` with the next
  per-output target).

**Per-output semaphores:** today `record_compose_v2` queues a
`vkQueueSubmit2` then calls `bo.export_signaled_fd()` to obtain
the SYNC_FD handed to `submit_flip_with_fences` (`scene.rs:2385
-2398`). Phase B's close emits N `vk::SemaphoreSubmitInfo`
signal-entries on the single CB's submit, calls
`export_signaled_fd` per output AFTER the submit returns, and
attaches each SYNC_FD to its output's pageflip. The fence
ticket signals once all N rendering instances complete.

**Per-output partial-failure handling.** A multi-output close
can still fail per-output at `submit_flip_with_fences` even
when the shared GPU submit succeeded, exactly as today
(`scene.rs:1347-1404`). Each output's pageflip is an
independent kernel atomic-commit:

- **Shared submit succeeded, output `i` flip succeeded.**
  Standard path. Output `i`'s `pending_ack` parks under the
  shared frame ticket; on pageflip-event the ack retires and
  releases output `i`'s pool slot.
- **Shared submit succeeded, output `i` flip failed
  (IO error path).** Mirrors today's "9b" branch:
  `platform.invalidate_bo(output_idx_i, token_i.bo_idx)`,
  record `missed_pageflip`, push
  `FailedSubmitBo { bo_idx: token_i.bo_idx, pool_slot:
  slot_i, ticket: frame_ticket.clone() }` onto output `i`'s
  `state.failed_submit_bos`, set
  `next_submit_retry_at`, fold the output's repaint forward
  via `pending_repaint_after_failed_submit`. Successful
  outputs in the same close are unaffected — they proceed
  through their normal pending_ack path. **The shared frame
  ticket is the gate for all parked resources** (failed and
  successful outputs share it); per-output bookkeeping is
  what's independent. The plan must walk every output's
  failure-handling state on close and dispatch per-output.
- **Shared submit failed (any output 9a branch equivalent).**
  No output's BO was actually written. All outputs' pool
  slots release via `pool_ring.release(slot)`; all outputs'
  scanout BOs go through `platform.recycle_failed_submit_bo`.
  Frame-builder `renderer_failed` discipline fires (the failed
  shared submit IS the frame-close failure path).

**Why one shared frame ticket for all outputs is safe under
partial failure.** Today each output had its own compose
fence; partial failure parked only the failed output's BO
against its own fence. Under Phase B, all outputs share the
same fence (the frame ticket), so successful outputs'
pending_ack and failed outputs' `FailedSubmitBo` both hold
clones of the same ticket. Resource retirement
(scanout BO recycle, pool slot release) on the failed
outputs simply waits for the same shared fence as the
successful outputs — slightly more pessimistic than today
(failed-output BO can't recycle until ALL outputs' work
retires), but correct. **No new failure mode is
introduced;** partial failure is just routed through one
shared ticket instead of per-output tickets.

**Compositor schedulability across partial failure.** A
distinct concern from BO/ticket bookkeeping: today
`maybe_composite` gates on `scene_structure_dirty &&
has_output_ready_for_submit()` (`backend.rs:4580`), and a
successful output clears the global dirty bit
(`scene.rs:850`). If output A's flip succeeded and output B's
failed, B's only retry signal under today's per-output design
is its own `next_submit_retry_at` deadline — which is fine
because today each output's compose was independent. Under
Phase B's shared submit, the dirty bit is cleared once for
the whole frame even when output B needs a retry; without an
explicit fix, B would stay stale until unrelated damage
marks the scene dirty again.

**Spec requirement.** The Phase B partial-failure path MUST
extend the `maybe_composite` gate to:

```
(scene_structure_dirty
    || any_output_has_pending_failed_submit_or_retry())
&& has_output_ready_for_submit()
```

where `any_output_has_pending_failed_submit_or_retry()`
returns true while ANY output has a non-empty
`failed_submit_bos` queue OR `next_submit_retry_at.is_some()`
— i.e. as long as a retry is *outstanding*, regardless of
whether the deadline has been reached. (`has_output_ready_
for_submit()` and the existing `earliest_retry_deadline()`
control *when* the wake-up fires; the predicate controls
whether scheduling is gated at all.) The dirty-or-retry
predicate must NOT clear on "deadline reached"; it must clear
on "retry attempted" (the retry path explicitly resets
`next_submit_retry_at = None` and either re-pushes a new
`failed_submit_bos` entry on re-failure or proceeds via the
standard path on success).

A simpler-but-wrong shape that this spec REJECTS: gating on
"unreached deadline" alone. The shared frame fence may
signal and drain `failed_submit_bos` before the deadline,
flipping the predicate to false; once the deadline passes
without an actual retry attempt, the retry would be lost.
Persist the retry signal until the retry actually happens.

This makes failed-output retry state dirty-equivalent for
scheduling. The plan wires this through a
`Scene::has_failed_outputs_pending_retry()`-shaped predicate
(or equivalent). Spec does NOT mandate "leave
`scene_structure_dirty = true` on partial failure" because
muddling "scene state changed" with "output needs retry"
confuses other consumers of the dirty bit; an explicit
predicate is cleaner.

### Frame close triggers

Spec § Phase B open question 5. The open frame must close at
deterministic points; otherwise resource pinning grows
unbounded and paint never reaches the GPU.

**Triggers, in order of "should be the common case":**

1. **`maybe_composite` ready-output sweep** (the dominant path).
   `(scene_structure_dirty ||
   any_output_has_pending_failed_submit_or_retry()) &&
   has_output_ready_for_submit() && frame.is_open()` triggers
   `close_into_cb` with attached compose. Paint + compose land
   in one CB, one submit. This is the design's normal "one
   frame ⇒ one submit" path. The OR with the partial-failure
   predicate keeps a stale failed output schedulable across
   shared submits even when `scene_structure_dirty` has
   already been cleared by a successful sibling output — see
   § "Per-output partial-failure handling".
1b. **PRESENT-completion semaphore attached.** When a COW
   PRESENT request attaches a `present_completion` semaphore
   to a paint op (today: `engine.rs:2460`-ish flush before
   semaphore export in Phase A), the open frame closes
   IMMEDIATELY so `vkGetSemaphoreFdKHR(SYNC_FD)` observes a
   queued signal-op. This is the Phase-A
   `PresentCompletionSignal` flush trigger lifted into the
   frame builder: same VUID-VkFenceGetFdInfoKHR-handleType-
   01457 hazard (the Task 6.1 yoga hang); same mitigation.
   Submit shape: the frame closes, the submit signals the
   present-completion semaphore as one of its signal entries
   (alongside any per-output compose semaphores if compose
   joined the frame in the same tick), then the
   semaphore-export FD threads through to PRESENT's
   CompleteNotify.
2. **`get_image` sync-barrier** (inherited from Phase A § flush
   trigger 1). `get_image` is the only `ticket.wait()` site
   after Phase A's Task 15 cleanup. Phase B routes get_image
   through `close_into_cb_for_sync_wait` which closes the open
   frame, submits the CB, and waits on the frame ticket before
   recording its readback CB. The readback itself stays a
   one-shot small CB outside the frame builder — its lifetime
   matches today's `get_image` bypass path under Phase A.
3. **Non-frame-builder paint op** (only relevant during B.1–
   B.4 sub-phase rollout — see § "Migration boundaries").
   Any paint op that hasn't been ported to the frame builder
   yet closes the open frame BEFORE recording its own CB. This
   is the hard boundary that keeps mixed-rollout correct: a
   non-ported op cannot see the open-frame layout overlay, so
   the only safe ordering is "flush + commit the frame, then
   the non-ported op runs against committed `current_layout`
   state". Trigger retires at B.5 when SubmitGroup is deleted.
3b. **Legacy scene compose** (only relevant during B.1–B.3).
   `maybe_composite`'s tick closes the open frame BEFORE
   `scene.tick` records compose, since compose still records
   outside the frame builder until B.4. Same reasoning as
   trigger 3 (compose samples drawables; must observe
   submitted paint), applied to the compose call site
   specifically. This is Invariant M3 in § "Migration
   boundaries"; trigger retires at B.4 when compose joins
   the frame.
4. **Timeout** (idle / no-pageflip case). A frame that's been
   open for > T ms without a ready output forces a close to
   release pinned resources. Default T = 16 ms (one vblank at
   60 Hz). Configurable via env knob
   `YSERVER_FRAME_BUILDER_TIMEOUT_MS` (default 16) for hardware
   triage. The timeout closes the frame without compose
   (compose runs at the next `maybe_composite` tick that
   observes a ready output).
5. **Shutdown.** `KmsBackendV2::shutdown` closes any open frame
   before tearing down platform state. Mirrors Phase A's
   `Shutdown` flush reason.
6. **Pin-set ceiling.** `max_pinned_resources_per_frame` =
   1024 (initial guess, retune from telemetry). Catches a
   pathological glyph-storm or self-aliasing-readback loop that
   would otherwise exhaust descriptor pool / staging pools
   between vblanks. Reaching the ceiling forces a close and
   logs `frame_builder: pin set ceiling at {n}` once.

**Pageflip retire** is *not* a Phase B close trigger by itself:
the open frame's lifetime is `open paint → close on compose-or-
timeout`, and the next frame opens fresh on the next paint.
Pageflip retire signals output BO availability for the *next*
frame's compose; it does not interact with an open paint-only
frame.

### Frame-wide resource pinning

Spec § Phase B open question 6. Task 6.1 solved Arc-pinning for
xshmfence/syncobj specifically; Phase B generalises. The
generalisation has TWO retirement mechanisms because v2's
resource model splits the same way today:

**Mechanism 1 — Arc-tracked resources.** Things that have an
owning `Arc<...>` already (or can trivially get one): per-op
staging buffers, gradient/sampler caches, owned syncobj /
xshmfence handles, owned semaphores. These pin via Arc clone
into the frame's pin vectors:

```rust
pub(crate) struct FramePinSet {
    staging_buffers: Vec<Arc<StagingBuffer>>,
    sync_objects: Vec<Arc<OwnedSyncObject>>,
    semaphores: Vec<Arc<OwnedSemaphore>>,
    // Future: image_views once they're Arc-wrapped. Today the
    // drawable_view_cache returns raw vk::ImageView handles
    // whose lifetime is tied to the owning drawable; the
    // append-time touch_render_fence (see § "Drawable lifetime")
    // is what keeps THOSE handles live, not Arc clones.
}
```

**Mechanism 2 — Generation-watermark resources.** Things
already tracked by a monotonic generation counter:
**descriptor sets only**, via `DescriptorPoolRing`
(`engine.rs:553` / `engine.rs:686`). These pin via a
*watermark snapshot*, not an Arc clone:

```rust
pub(crate) struct FrameWatermarks {
    descriptor_pool_gen: u64,
}
```

The frame builder, at every op append that acquires a
descriptor set, calls `acquire_for_frame(&mut frame
.watermarks)` on the ring; the ring captures
`acquire_generation` as a high-water value on the frame.
Retirement (frame ticket signal) is the gate at which
`DescriptorPoolRing::release_up_to` can recycle pools whose
generation is ≤ the watermark. This mirrors Phase A's
`descriptor_ring_does_not_reset_in_use_group` invariant — but
the unit shifts from "the SubmitGroup's shared ticket" to
"the frame's ticket".

**Mechanism 3 — Singleton scratch resources via Arc-wrap.**
`DstReadback` (`engine.rs:509`), `src_alias_readback`
(`engine.rs:520`), `mask_scratch` (`engine.rs:536`),
`white_mask_image` (`engine.rs:505`), `solid_color_scratch`,
and the trap-pipeline coverage scratch are *singleton
mutable handles* on `EngineInner`, not generation-tracked
pools. A `u64` watermark cannot pin them: growth replaces the
handle entirely (`dst_readback.rs:107-145` returns the old
image/view as a `BatchResource`; v2 currently drops it on
the floor — known retired-resource leak called out at
`engine.rs:529-535`). Phase B's structural fix is to wrap each
scratch as `Arc<dyn ScratchHandle>` on `EngineInner`:

```rust
pub(crate) struct EngineInner {
    dst_readback: Arc<DstReadback>,
    src_alias_readback: Arc<DstReadback>,
    mask_scratch: Arc<MaskScratch>,
    white_mask_image: Arc<SolidColorImage>,
    // …
}
```

(`#[allow(clippy::type_complexity)]` where the Arcs sit
behind generic ScratchHandle traits; the plan picks the exact
trait shape.)

At op-append, the frame's pin set clones the appropriate
Arc into a typed `scratch_resources: Vec<Arc<dyn
ScratchHandle>>` (or per-kind sub-vectors for clarity).
**Growth** (e.g. `DstReadback::ensure_image_size_returning_
old`) swaps the engine's singleton Arc to a new instance and
drops the engine's strong ref to the old one; the open
frame's pinned clone keeps the old Arc alive until the frame
ticket signals; on signal, the old Arc drops and its
`Drop` impl returns memory to the platform. This collapses
the existing retired-resource leak as a side effect of the
fix. **Layout state for scratch images** is then tracked
either inside `Arc<DstReadback>` itself (each instance owns
its layout state across the open frame) or via the
FrameLayoutTable using a synthetic `DrawableId` per
scratch instance — the plan picks the cheaper option, but
the spec mandates that scratch layout state is per-Arc-
instance, not per-singleton-slot, so growth doesn't strand
the prior instance's layout state.

**Why mechanism 3 instead of "promote scratch into a pool".**
Pool-shape works for descriptor sets because acquire/release
is per-call; scratch is per-engine-singleton with growth as
the only mutation. Pool refactoring is out of scope for this
spec; Arc-wrap + retire-on-fence is the minimal change.

**Equivalence with Phase A.** Phase A held mechanisms 1 + 2
implicitly: `SubmittedOp` Arc'd its staging / atlas_ticket
refs (mechanism 1); `DescriptorPoolRing` already gated
recycle on `acquire_generation` watermark (mechanism 2).
Mechanism 3 (Arc-wrapping the singleton scratch handles) is
new to Phase B — Phase A masked the underlying retired-
resource leak because per-op submits retired scratch
references via the per-op `SubmittedOp` Arc structure, but
that path doesn't survive the deferred-frame window where a
growth event can strand the prior singleton. Phase B keeps
mechanisms 1 + 2 as they are; mechanism 3 is the new piece.

**Lifetime contract:** frame-close-success moves the pin set
+ watermarks into a `FrameSubmittedRecord` parked alongside
the frame ticket; on ticket-signal (CPU side, retirement
sweep) the Arcs decrement (returning resources to their pools)
AND the descriptor pool ring's `release_up_to(watermark)` is
called. Frame-close-failure drops both immediately (CBs never
executed, resources never read).

**Retention math.** Phase A's worst-case retention was `16 ×
worst-op footprint`. Phase B's worst-case is *one frame's*
footprint — bounded by `max_pinned_resources_per_frame` and
the per-frame paint count. For a MATE drag frame with ~55
paints (today's bee average), the per-frame retention is
roughly equal to Phase A's max group at the cap, modulo the
larger pin set vs Phase A's per-op `SubmittedOp` set. Phase B
keeps Phase A's `active_staging_bytes` /
`active_descriptor_pool_count` telemetry and adds a new
`active_pinned_resources` gauge.

### CB recording at close

`close_into_cb` runs a deterministic three-pass walk of the
op list:

1. **Resource pass.** Iterate ops to build the consolidated
   barrier list (read/write sets per drawable, atlas image
   layout transitions, staging buffer barriers). No Vulkan
   commands recorded yet; the pass produces a
   `Vec<vk::DependencyInfo>` that interleaves with the op
   draws.
2. **Record pass.** Issue `vkBeginCommandBuffer`, then for each
   op emit (a) any layout transitions logged for that
   drawable's first-touch-in-frame, (b) the op's draw or
   transfer commands via the existing per-op recorders
   refactored to take a `&CbRecorder` instead of submitting,
   and (c) any layout transitions for the *next* op that
   require a separate barrier. Compose ops emit
   `cmd_begin_rendering` / `cmd_end_rendering` per output
   target (same dynamic-rendering API as today's
   `record_compose_v2`).
3. **Finalise pass.** End the CB, build the
   `VkSubmitInfo2` with N signal semaphores (one per compose
   target, exported via `vkGetSemaphoreFdKHR(SYNC_FD)`), submit,
   stash the frame ticket on a `pending_frames` queue parallel
   to today's `submitted` queue, and (on success) commit the
   layout overlay + atlas pending inserts.

The three-pass walk is a deliberate trade-off:

- Single pass would need backward-references ("this op needs the
  prior op's layout barrier"). Easier to get wrong; harder to
  audit.
- Pass 1 is pure CPU-side data flow; pass 2 is pure
  CB-recording; pass 3 is submit + bookkeeping. Each pass has
  one job, simplifying review and unit tests.

### Migration boundaries

Phase B is not a single atomic switch — too much surface to
land in one task arc. The spec proposes a phased rollout that
is **bee-safe from sub-phase B.1**, not just bee-safe at B.5.
Two invariants make the mixed-paths state correct:

**Invariant M1 — non-ported paint ops force SubmitGroup
`cap = 1`.** During B.1–B.4 the SubmitGroup is reconfigured to
single-CB-per-`vkQueueSubmit2` for the duration of the
rollout. Phase A's bee captures showed cap=1 boots MATE clean
on bee (`status.md` § "2026-05-23 bee MATE-load freeze" —
the `cap=1` row); the freeze begins at the first regular
cap≥2 group. B.1 sets `set_max_size(1)` at platform open and
keeps it there until B.5 deletes SubmitGroup entirely. The
SubmitGroup machinery effectively becomes the
pre-Phase-A per-op submit cadence for the duration of the
sub-phase rollout; the frame builder handles the ported ops
in one CB per frame. Bee survives B.1.

**Invariant M2 — non-ported paint ops close the open frame
first.** Before any non-ported entry point records its own
CB, the engine calls `frame.close_into_cb(FrameCloseReason::
NonPortedPaintOp)`. This guarantees (a) the non-ported op
reads a committed `current_layout` (the open frame's layout
overlay has been written back to `storage.current_layout`),
(b) drawable `last_render_ticket` values reflect submitted
work, not append-time placeholders, (c) submit order between
the frame builder and the non-ported op matches X11
chronological order. Without M2, the non-ported op would see
stale `current_layout` and race against the deferred frame on
the GPU. M2 retires when the last non-ported entry point
moves into the frame builder at B.4 close.

**Invariant M3 — legacy scene compose closes the open frame
first.** Until sub-phase B.4 folds compose into the frame, the
existing `maybe_composite` path (`backend.rs:4574`) records
the compose CB outside the frame builder and submits via
`scene.tick`. `scene.tick` samples drawable storage at record
time (`scene.rs:1307`), so any open paint frame must close +
commit BEFORE compose records. During B.1–B.3, the load-
bearing flush in `maybe_composite` becomes:
`if frame.is_open() { frame.close_into_cb(LegacyScCompose);
} self.engine.flush_submit_group(SceneCompose); scene.tick(…)`.
Without M3, legacy compose would sample stale `current_layout`
on drawables whose paint sits in the open frame, or worse,
race the deferred frame on the GPU. M3 retires at B.4 when
compose itself ports into the frame.

**Sub-phases:**

- **Sub-phase B.1 — FrameBuilder skeleton + `composite_glyphs`.**
  Build the FrameBuilder behind a feature gate
  (`YSERVER_FRAME_BUILDER=on`). Set SubmitGroup `max_size=1`
  (M1). Port `composite_glyphs` first; every other paint op
  stays on SubmitGroup with M2's flush-first discipline.
  Composite glyphs is the bee fault site, so landing this
  alone — in combination with M1's cap=1 on remaining paths
  — eliminates every multi-CB submit shape from the
  frame-on-bee workload. B.1 IS the bee fix. Telemetry split
  into `frame_builder_*` vs `submit_group_*` counters so the
  two paths report independently. **B.1 acceptance: bee MATE
  drag survives, telemetry confirms `submit_group_size_max =
  1` throughout.**
- **Sub-phase B.2 — port the top three by submit budget.**
  `render_composite` + `render_fill` + `composite_glyphs`
  cover 75 % of submits per the bee 2026-05-23 capture
  (`yserver-mate.submit.tsv` § "Submit-source ranking from the
  bee 2026-05-23 capture" in
  `2026-05-23-frame-builder-submit-rate-design.md`). After
  B.2 the bulk of MATE-drag work is in the frame builder; M1
  + M2 still keep the unported tail bee-safe.
- **Sub-phase B.3 — port the remaining paint ops.** `cow_copy_area`
  + `copy_area` + `put_image` + `render_traps_or_tris` +
  `glyph_upload` (the standalone CB path, distinct from
  glyph-uploads-inside-composite_glyphs). After B.3 every paint
  op lives in the frame builder; M2's "close frame on non-
  ported op" trigger should never fire from production code
  paths (only from test paths that exercise the legacy code).
- **Sub-phase B.4 — scene compose joins the frame.** Compose's
  CB merges into the frame CB; the load-bearing flush in
  `maybe_composite` becomes `frame.close_with_compose(...)`.
- **Sub-phase B.5 — delete SubmitGroup.** With every entry
  point on the frame builder and compose folded in, the Phase A
  machinery (`submit_group.rs`, `pending_group_ops`, the
  per-class `pending_cow_batch` / `pending_render_batch` paths,
  Invariants M1 + M2) retires. `last_render_ticket` on
  drawables becomes "the frame ticket from the frame that
  last touched this drawable".

Each sub-phase is its own implementation plan with its own
codex review round; this spec covers only the *design*
shared across them.

### Error handling and rollback

Phase A made `renderer_failed` fatal-on-submit-failure for
drawable-visible state. Phase B inherits the same discipline
but expands the rollback surface:

- **Op list.** Dropped immediately on `vkBeginCommandBuffer`
  or `vkQueueSubmit2` failure. CBs that the platform CB
  recorder reached but failed to finalise free via the existing
  `OpsCommandPool::free` path (the same surface Phase A used).
- **Pin set.** Dropped on failure (the CB never executed; no
  GPU reads outstanding).
- **Layout overlay.** Best-effort restored to
  `pre_frame_layout` per drawable. If restoration itself fails,
  set `renderer_failed` and stop — subsequent entries
  short-circuit on the existing Phase A gate.
- **`touched_drawables` overlay.** Per-drawable
  `last_render_ticket` restored to its pre-frame value (see
  § "Drawable lifetime"). This is load-bearing: without it,
  `decref` / `poll_pending_retire` would observe a never-
  signaled phantom ticket and leak storage even under
  `renderer_failed`.
- **Atlas pending inserts.** Dropped (uncommitted; the cached
  glyph pixel addresses were never read).
- **`pending_present_completions`.** Inherits Phase A § "COW
  PRESENT-completion-failure force-fire" — semaphore-attached
  PRESENT completions on the failed frame fire via the existing
  `PendingPresentBatch::Ready` path BEFORE the Err propagates,
  so CompleteNotify clients don't hang.
- **`pending_frames` queue.** Failed frames never park here;
  successful-then-GPU-failed frames are out of scope (a frame
  that submits successfully but the device losses afterwards
  is a separate "device-lost teardown" path in `KmsBackendV2`,
  unchanged by this spec).

**Why `renderer_failed` fatal is acceptable for Phase B.** The
expected failure mode is the bee-style RDNA2 GPUVM fault
(`ERROR_DEVICE_LOST` on submit), at which point the device is
hosed and the only correct response is teardown anyway. Partial
recovery from a submitted-frame failure is not a goal of this
spec.

## Telemetry

Phase B introduces a parallel telemetry counter set so the
sub-phase rollout can compare frame-builder vs SubmitGroup
paths side by side. All counters report at the existing
`v2_telemetry:` per-second granularity.

- `frame_builder_opens/s`, `frame_builder_closes/s` (lifetime
  counts of paint-driven open + close-into-cb cycles).
- `frame_builder_close_reason` (lifetime counts:
  `MaybeComposite`, `PresentCompletionSignal`, `SyncWait`,
  `NonPortedPaintOp`, `LegacyScCompose`, `Timeout`,
  `Shutdown`, `PinCeiling`). `NonPortedPaintOp` and
  `LegacyScCompose` retire over the sub-phase rollout (M2 at
  B.4, M3 at B.4); their lifetime counters going to zero is
  the rollout signal.
- `frame_builder_ops_per_frame_avg/max/histogram`. Histogram
  buckets `1, 2-3, 4-7, 8-15, 16-31, 32-63, 64-127, 128+`.
- `frame_builder_active_pins` (gauge — current pin set size
  on the open frame; reported as 0 when closed).
- `frame_builder_active_pins_high_water` (gauge — peak across
  the run).
- `frame_builder_aborts/s` (lifetime — failed submits).
- `frame_builder_glyph_uploads_per_frame_avg/max` —
  load-bearing for the bee-fault narrative; pinned to show
  composite_glyphs collapses to ≤ 1 submit/frame.
- Phase A counters (`submit_group_size_avg`, `_max`,
  `_flush_reason_*`, `_aborts`) remain wired during B.1–B.4.
  On B.5 they retire alongside the SubmitGroup struct.

**Quantitative target:** `queue_submit2/s` on bee MATE drag at
the end of B.4 in the 200–400/s band (≈ 3–7 submits/frame at
60 Hz), down from Phase A's 900–1500/s target. Xorg+glamor
sustains the same workload at ~60–300/s (1–5 submits/frame).

## Acceptance tests

Unit tests:

- `frame_builder_lazy_open_does_not_allocate_when_no_paint`.
- `frame_builder_op_append_watermark_pins_descriptor_pool` —
  appending an op that acquires a descriptor set bumps the
  frame's `FrameWatermarks::descriptor_pool_gen` to the
  acquired set's generation. Subsequent
  `DescriptorPoolRing::release_up_to` calls (which Phase A's
  retirement sweep drives) cannot release pools whose
  generation is ≤ the watermark until the frame ticket signals.
- `frame_builder_layout_overlay_first_touch_snapshots_pre_frame`.
- `frame_builder_layout_overlay_commit_writes_back_on_close`.
- `frame_builder_layout_overlay_rollback_restores_pre_frame`.
- `frame_builder_glyph_upload_records_before_draw_in_same_cb`
  — assert the recorded op order via a `peek_ops` introspection
  parallel to Phase A's `peek_entries`.
- `frame_builder_atlas_insert_deferred_until_close_success` —
  failed frame leaves `V2GlyphAtlas::lookup` returning None for
  the would-be-inserted key.
- `frame_builder_multi_output_one_cb_two_rendering_instances` —
  dual-output close produces one CB with two
  `cmd_begin_rendering` / `cmd_end_rendering` pairs and two
  signal semaphores.
- `frame_builder_timeout_closes_frame_without_compose`.
- `frame_builder_pin_ceiling_force_closes_and_logs_once`.
- `frame_builder_get_image_closes_open_frame_before_wait`.

Integration tests (`tests/v2_acceptance.rs`):

- `v2_frame_builder_composite_glyphs_one_submit` — 32-glyph
  composite_glyphs call produces exactly one `vkQueueSubmit2`
  (was 2 under Phase A: upload + draw).
- `v2_frame_builder_render_composite_collapses_to_one_per_frame`
  — three back-to-back `render_composite` calls across a tick
  produce exactly one `vkQueueSubmit2`.
- `v2_frame_builder_renderer_failed_on_submit_failure` —
  injected submit failure marks `renderer_failed` and rolls the
  layout overlay back to `pre_frame`.
- `v2_frame_builder_mixed_sequence_smoke` — realistic ordering
  (paint → glyph_upload-in-composite → paint → compose) produces
  exactly one `vkQueueSubmit2`.

Hardware gates (run by the user, not the agent):

- **bee MATE-load survival** — boot MATE with `cap` removed
  (env knob retired), drag for 30 s, expect zero
  `ERROR_DEVICE_LOST` and zero GPUVM faults. This is the bee
  closure gate, replacing Phase A's "deferred to Phase B" note.
- **yoga / iMac / fuji regression check** — same MATE drag,
  expect `queue_submit2/s` ≤ Phase A's measured peak (no regressions).
  Phase B doesn't have to *improve* any specific platform; it
  has to (a) fix bee, (b) not regress the other three.
- **silence dual-output regression check** — dual 2560×1440
  MATE drag at silence (i9 13900k / rx580 / RADV), expect both
  outputs compose correctly under the one-CB-N-render-passes
  topology. Catches multi-output rendering bugs before they
  hit users.

## Open questions for the implementation plan

These are deliberately punted from the design to the plan,
because the implementation will surface answers from the code
that the design can't predict:

1. **Op variant sizing.** `RecordedOp` enum size is bounded by
   its largest variant. The plan profiles `mem::size_of::
   <RecordedOp>()` after the first variant lands and decides
   between `Box`-ing large variants vs accepting padding waste.
2. **Owned-handle wrapper churn.** `OwnedImageView`,
   `OwnedDescriptorSet`, `OwnedSyncObject`, `OwnedScratch`,
   `OwnedStaging`, `OwnedAtlasPage` — which already exist as
   strong types, and which need wrapping. Plan inventories
   existing `Arc<...>` sites in `engine.rs` and chooses minimal
   refactor scope.
3. **Frame-builder feature gate retirement timing.** Plan decides
   whether to flip the default mid-rollout (start at
   B.2 close?) or only at B.5 close. The feature gate exists
   so the rollout can ship behind it; the question is
   when to remove the gate, not whether to have one.
4. **Compose's existing dst_readback / mask_scratch paths.**
   These are not paint primitives in the X11 sense but they
   record CBs today. The plan decides whether they fold into
   the frame CB at B.4 or stay separate (the latter would
   reintroduce the multi-CB shape and is therefore unlikely
   to be acceptable).
5. **CompletionSemaphore export ordering.** Today's per-output
   compose calls `vkQueueSubmit2` first and then
   `bo.export_signaled_fd()` (`scene.rs:2385-2398`) — submit-
   then-export, NOT export-before-submit. Phase B preserves the
   same ordering: one `vkQueueSubmit2` per frame with N signal
   semaphores, then N `export_signaled_fd` calls (one per
   ready output) feeding `submit_flip_with_fences`. The plan
   validates that batched signal-then-export works correctly
   under one submit (semaphore signal payload visible to the
   matching `vkGetSemaphoreFdKHR(SYNC_FD)` call); spec assumes
   yes but the plan needs an explicit pass-through test.
   Separately, the COW `PresentCompletionSignal` semaphore
   used for CompleteNotify uses the same submit-then-export
   shape; see § "Frame close triggers" trigger #1b.
6. **Telemetry overlap window.** B.1–B.4 has two paint paths
   reporting submit-rate counters simultaneously. The plan
   defines a clear sum (`paint_submits = submit_group_paint_
   submits + frame_builder_paint_submits`) so dashboards
   show consistent totals during the rollout.

## Out of scope (intentional)

- **Per-CB timeline-semaphore retirement** (Phase A's Model
  A2). Frame ticket = one fence per frame; timeline
  semaphores would be an optimisation for sub-frame retirement
  that buys nothing structural.
- **Direct scanout for fullscreen GL clients (wlroots-style).**
  Out of scope per the Phase A spec's same exclusion; revisit
  after Phase B if there's appetite.
- **Plane composition (cursor + scene on different planes).**
  Same reasoning; the hw-cursor work already covers the cursor
  plane case; future plane work is a separate spec.
- **Refactoring v1.** Same as Phase A; v1 stays in tree until
  its deletion gates pass.
- **Multi-queue async transfer.** Stage 5 § Task 6 deferred
  decision. The frame CB is a graphics-queue submit; if
  profiling justifies an async-transfer split later, it's a
  follow-up spec.
- **bee RDNA2 RADV bug characterisation.** Phase B fixes bee
  *structurally* (removes the multi-CB-per-submit shape);
  filing the actual RADV/firmware bug is a separate task
  (and out of yserver's repo).

## References

- Phase A spec:
  `docs/superpowers/specs/2026-05-23-frame-builder-submit-rate-design.md`
- Phase A plan:
  `docs/superpowers/plans/2026-05-23-frame-builder-submit-rate-phase-a.md`
- Phase A close-out (status doc):
  `docs/status.md` § "Phase A — CLOSED 2026-05-24".
- bee 2026-05-23 freeze capture:
  `docs/status.md` § "2026-05-23 bee MATE-load freeze (KNOWN,
  deferred to Phase B)".
- Multi-platform Phase A captures (yoga / iMac / fuji / nvidia):
  `docs/status.md` § Phase A capture entries dated 2026-05-23.
- Submit-source ranking baseline:
  `2026-05-23-frame-builder-submit-rate-design.md` § "Submit-
  source ranking from the bee 2026-05-23 capture".
- v2 layout-tracker single source of truth:
  `crates/yserver/src/kms/v2/store.rs:506`
  (`Drawable::record_layout_transition`).
- v2 glyph atlas pack / insert split:
  `crates/yserver/src/kms/v2/glyph_atlas.rs:283-294`.
- v2 compose entry:
  `crates/yserver/src/kms/v2/backend.rs:4574` (`maybe_composite`).
- v2 scene build entry:
  `crates/yserver/src/kms/v2/scene.rs:1517` (`build_scene`).
- Task 6.1 Arc-pinning precedent:
  `docs/superpowers/specs/2026-05-23-deferred-present-completion-design.md`.
