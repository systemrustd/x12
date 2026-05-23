# Frame-builder submit-rate reduction — design

**Status:** draft. Captured 2026-05-23 from a Task 6.1 post-mortem,
sharpened by two codex review rounds the same day. Splits into two
phases at codex's recommendation:

- **Phase A** (this spec, plan-ready) — multi-CB single-submit:
  keep per-op-class CBs but batch many into one `vkQueueSubmit2`.
  Cheap, low-risk, captures ~50-60 % of the bee drag-lag win.
- **Phase B** (follow-up spec, deferred) — deferred op-list
  FrameScheduler. Bigger refactor, needs design work on op
  representation + glyph uploads + transactional rollback before
  it's plan-ready. Sketched at the bottom of this doc; not
  detailed.

**Framing:** not "rewrite v2" — **"replace v2's submit
scheduler."** The data model stays. The discipline that controls
*when* GPU work is submitted changes.

**Load-bearing design goal:** *one scanout opportunity should
consume one submitted frame worth of accumulated X11 work*, not
dozens of independent submits that happen before the next pageflip
can display anything.

## Why this exists

Task 6.1 (deferred PRESENT completion) closed the per-PRESENT CPU
fence-wait stall but did not change yserver's underlying
*per-X11-operation* submit model. Telemetry on bee MATE
post-Task-6.1: `queue_submit2/s` peak **3304** ≈ 55 submits/frame.
Xorg + glamor on the same machine sustains the same workload at
~1–5 submits/frame (one GL flush + one pageflip per frame);
wlroots sustains it at ~1–3 (one scene render pass + one
pageflip). The **bee drag-lag is now bounded by per-`queue_submit2`
`ioctl → libvulkan_radeon → amdgpu` round-trip cost**, not by
anything Task 6.1 touched. Catching up to Xorg/wlroots on submit
rate requires changing yserver's render scheduling discipline.

## Submit-source ranking from the bee 2026-05-23 capture

Total submits across a ~30 s MATE session, from
`yserver-mate.submit.tsv`:

```
20171  render_composite   ← GTK widget composite (dominant)
17973  render_fill
 8993  composite_glyphs   ← text
 8525  put_image
 5857  copy_area          ← marco's XCopyArea(window→COW)
 2107  render_traps
 2069  scene_compose
  340  get_image          ← readbacks
  165  glyph_upload
   48  fill_batch
```

**Implication:** the top three (`render_composite`, `render_fill`,
`composite_glyphs`) are 75 % of the submit budget. `copy_area`
(marco's COW pump) is only 9 %. Phase B's later sub-phases need to
target RENDER widget paths first, not the COW pump. Phase A
(multi-CB single-submit) is op-class-agnostic — it captures
whatever submits come through, in whatever order.

## Architectural context (unchanged from earlier drafts)

**Xorg + glamor.** Each XRender / XCopyArea / XPutImage records GL
commands into glamor's lazy command buffer. No GPU submission until
`glFlush` / SwapBuffers. Per frame: one GL flush → one
`queue_submit2` → one pageflip.

**wlroots.** Scene-graph walked once per frame, one render pass,
one submit. DMA-buf passthrough where possible. 1–3 submits/frame.

**yserver v2 (today).** Each X11 paint primitive → its own engine
op → its own CB → its own submit + fence. `cow_batch` /
`render_batch` aggregate within an op class (5–8 ops/batch) but
break on any non-batched op or class change. Scene compose is a
separate CB + submit. ~55 submits/frame on bee MATE drag.

## Phase A — multi-CB single-submit (this spec)

Keep per-op-class CBs but batch multiple CBs into a single
`vkQueueSubmit2`. **Same flush ordering as today**, **same
per-class batching**, **same fence + CB lifecycle** — the only
change is that consecutive submits with no intervening
synchronisation requirement collapse into one ioctl.

### Concrete scope

- New struct `SubmitGroup` on `PlatformBackend`: queues up
  `vk::CommandBufferSubmitInfo` entries plus their `FenceTicket`s.
  Submits all queued CBs in a single `vkQueueSubmit2` call when
  flushed.
- Existing `submit_paint_cb` (the one used by `end_and_submit_op`)
  changes from "build a SubmitInfo2 + queue_submit2 now" to
  "append to the SubmitGroup". A separate `flush_submit_group()`
  method calls the actual `queue_submit2`.
- `SubmitGroup` flushes when ANY of these happens, whichever
  arrives first:
  1. **Synchronisation boundary** — anything that needs a fence
     wait (`FenceTicket::wait` from `get_image`, scene compose's
     IN_FENCE_FD export, etc.) flushes first so the wait observes
     the submit.
  2. **PRESENT completion signal-only submit** — Task 6.1's
     `submit_present_completion_signal` is a same-queue submit
     that signals the per-PRESENT completion semaphore. It relies
     on prior paint already being submitted; otherwise the
     semaphore can signal before the paint CB exists in the
     queue. The signal-only submit MUST flush the group first.
     (Codex's third-pass review flagged this as a missing trigger
     in the earlier draft.)
  3. **Scene compose submit** — compose's submit is what hands
     off to KMS; it flushes the group then submits compose's CB
     last in its own `queue_submit2` (preserves current "compose
     observes all prior submits" invariant).
  4. **Pageflip retire** — frame boundary.
  5. **Maximum group size** — guard against unbounded growth.
     Tentative: 16 CBs per group (re-tune from Phase A telemetry).

### Fence ownership model

This is the highest-risk decision in Phase A per codex's review.
Today every op has its own `FenceTicket`. With multi-CB
single-submit, `vkQueueSubmit2` only takes one VkFence per submit.
Two viable models:

**Model A1 — shared batch fence.** The SubmitGroup acquires one
`FenceTicket` at append time of the first CB; every CB in the
group references that ticket; the fence is the one passed to
`vkQueueSubmit2`. Drawables' `last_render_ticket` gets a clone of
this shared ticket. Retirement happens for all CBs together when
the shared fence signals.

- Pros: simple; one fence per submit; trivial CPU-side
  bookkeeping.
- Cons: longer retention. Per codex's review the math is
  **max group size × WORST submitted op footprint**, not
  best-case per-CB. The full retention set per group includes:
  descriptor sets, staging buffers, scratch images + scratch
  buffers, glyph-upload staging, and any retained atlas
  dependency tickets. Worst case at max-group 16 = 16× the
  per-op active set for whichever op has the largest footprint
  (typically `composite_glyphs` or `render_composite` with
  self-alias readback). Mitigated by the existing pool ring's
  growth behaviour, but worth instrumenting.
- **Decision: Model A1 ships.** The retention pressure is real but
  bounded by max group size; the alternative needs a per-CB
  timeline-semaphore model that's more invasive than the win
  justifies. Document the retention math as a Phase A success
  criterion AND add high-water telemetry (see § Phase A
  telemetry).

**Model A2 — per-CB timeline semaphores.** Each CB signals a
distinct timeline-semaphore value; the submit signals the highest
value at the end. CPU-side per-resource retirement consults the
semaphore counter to know which CBs are done.

- Pros: keeps per-CB granularity; descriptor pool slots retire as
  individual CBs complete.
- Cons: complex Vulkan code path; needs careful spec compliance
  (timeline values must be monotonic per submit); current
  `FenceTicket` API doesn't model timeline values; large
  refactor risk for a Phase-A "safer first" task.
- **Decision: rejected for Phase A.** Reconsider in Phase B if
  retention pressure becomes load-bearing.

### Ordering invariants

Batching heterogeneous CBs into one `vkQueueSubmit2` is correct
only if the **submitted CB order exactly preserves today's flush
order**. Specifically:

- `cow_batch` flush submits its CB BEFORE the next non-cow op.
  Today this happens via the per-op `flush_cow_batch_if_needed`
  hook. Under SubmitGroup, the cow_batch's CB must be appended
  before the subsequent op's CB.
- `render_batch` flush: same shape, must precede the subsequent
  non-render op.
- Scene compose's CB must be submitted AFTER all paint CBs in the
  current SubmitGroup observe each other; this is implicit if
  compose uses its own `queue_submit2` after flushing the group.
- `get_image` readback requires a `FenceTicket::wait` — flushes
  the group first, then issues its own waited submit.

The spec's invariant: **SubmitGroup append order = today's
chronological submit order.** No reordering across the group.

### Frame-submit error propagation

If `queue_submit2` fails, ALL CBs in the failed group are affected
together. CPU-side state mutated between the first CB's append
and the failed submit must roll back as a unit:

- Drawables' `last_render_ticket` — Model A1 sets all of them to
  the shared ticket at append time of each op. Rollback: restore
  the prior `last_render_ticket` snapshot.
- **Drawables' `storage.current_layout`** — `Drawable::record_
  layout_transition` (store.rs:506) mutates the CPU-side layout
  tracker BEFORE the GPU executes the barrier. Phase A relevant:
  if a group submit fails after layout was recorded into a CB
  but before the queue saw it, CPU layout diverges from GPU.
  Rollback: restore pre-group layout snapshot per drawable, OR
  declare layout inconsistency fatal and set `renderer_failed`
  (simpler, acceptable for Phase A).
- Damage accumulation — `Drawable::damage` mutates damage state
  per op. Rollback: reset to pre-op snapshot.
- Descriptor generation counters — `DescriptorPoolRing` advances
  on each acquire. Rollback: descriptor sets allocated but never
  used become orphaned but still get released when their pool
  retires; acceptable. **Hard constraint:**
  `DescriptorPoolRing::release_up_to` MUST NOT consider grouped
  work retired until the group's shared fence signals, even if
  later ops in the same group have higher `acquire_generation`
  values that would normally let earlier pools recycle.
- `RenderEngineInner.acquire_generation` + `submitted` queue —
  every op increments `acquire_generation` and inserts a
  `SubmittedOp`. Defer the `submitted.push_back` until after
  group submit returns `Ok`; on failure, drop the would-be
  entries.
- Staging buffers — allocated per upload op; if the submit fails
  they're never executed. Free them at rollback.
- **Scratch images / scratch buffers** — `DstReadback`,
  `src_alias_readback`, solid color scratch images. Allocated
  per op; on failure, return to their pools without marking
  used.
- **Glyph atlas state** — `atlas_last_upload_ticket` plus the
  atlas pack/insert state. If the upload submit fails after
  atlas entries were inserted, cached glyph pixels refer to
  never-executed work. **Phase A choice:** either rollback the
  atlas entries (complex), or declare atlas-upload failure
  fatal and set `renderer_failed`. **Decision: fatal.** Atlas
  consistency under partial rollback is too risky for Phase A.
- **Flush-record queues** — `cow_flush_records` /
  `render_flush_records` accumulate per-batch records consumed
  by `maybe_composite`. If pushed before group submit succeeds,
  the records describe work that never happened. Defer push
  until after group submit returns `Ok`; on failure drop them.
- **Pending COW / render batch state** — if a `PendingCowBatch`
  or `PendingRenderBatch` is `take()`n + appended to the group
  + group submit fails, the engine has no batch to reopen.
  Snapshot the `take()`n batch in the SubmitGroup; on rollback,
  put it back as `inner.pending_cow_batch = Some(snapshot)`.
- PRESENT completion semaphores (Task 6.1 inheritance) —
  semaphores attached to the failed submit need cleanup; pending
  PRESENT events stay in the queue and retry on next submit.
- `platform.renderer_failed` — set if rollback can't restore
  consistent state.

**Phase A acceptance: a concrete mutation inventory is part of the
implementation plan.** Every CPU-side mutation that happens
between SubmitGroup append-start and submit needs a documented
rollback path.

### Expected impact

Predicted on bee:

- Today: ~55 submits/frame, `queue_submit2/s` peak 3304.
- Post-Phase-A: ~15-25 submits/frame (most consecutive
  same-frame submits collapse), `queue_submit2/s` peak ~900-1500.
- Drag-lag relief: substantial (Xorg-on-bee is at ~1-5
  submits/frame, so we're closing ~60 % of the gap with Phase A
  alone).

Phase B closes the remaining gap by moving to per-frame deferred
op-list recording.

### Phase A scope (LOC + timeline)

- ~800-1500 LOC: `SubmitGroup` struct in `platform.rs`, append API
  in `ops.rs`, ordering invariant tests, telemetry counters.
- 1-2 weeks focused work.
- Touches `kms::v2::platform`, `kms::v2::engine` (lots of small
  edits at the `end_and_submit_op` call sites),
  `kms::vk::ops::OpsCommandPool` (the submit helper), `kms::v2::
  scene` (compose's flush ordering).

### Phase A telemetry

Existing `queue_submit2/s` and `paint_submits/s` keep their
meanings ("actual queue_submit2 calls" and "paint CBs appended").
New counters:

- `submit_group_size_avg` / `submit_group_size_max` / histogram —
  CBs per `vkQueueSubmit2`. Histogram (not just avg/max) is
  load-bearing for tuning the max-group-size threshold.
- `submit_group_flush_reason` (lifetime counts: `sync_boundary`,
  `present_completion_signal`, `scene_compose`, `pageflip_retire`,
  `max_size`).
- `submit_group_aborts` (lifetime, for the Phase-A error path).
- **`active_descriptor_pool_count`** (gauge — high-water across
  groups). Per codex's pass-3 review, retention is sized by
  worst-op footprint × max group size. Need to know if the
  descriptor pool ring is being pressured beyond its growth
  envelope.
- **`active_staging_bytes`** + **`active_scratch_bytes`** (gauge —
  high-water). Same reasoning: track in-flight retention so we
  can size pools defensively.

### Phase A acceptance tests

- `submit_group_collapses_consecutive_paint_cbs` — three
  consecutive `render_composite` calls produce ONE
  `vkQueueSubmit2` call.
- `submit_group_flushes_on_get_image` — a paint op followed by
  `get_image` produces two submits (paint group → wait + read).
- `submit_group_flushes_before_scene_compose` — paint group +
  compose = at most two submits (paint, then compose).
- `submit_group_flushes_before_non_cow_present_completion_signal`
  — PRESENT::Pixmap to a non-COW dst with prior paint appended
  must flush the group BEFORE its signal-only submit. Otherwise
  the completion semaphore can signal before the paint CB
  exists. (Codex pass-3 ordering fix.)
- `submit_group_preserves_cow_batch_ordering` — `cow_copy_area`
  + non-cow op + `cow_copy_area` again flushes the cow_batch
  before the non-cow op (matches today's behaviour).
- `submit_group_preserves_glyph_upload_before_draw` — a glyph
  upload + glyph draw within one group always submits the
  upload CB before the draw CB, including at the max-group-size
  boundary (upload as CB 16, draw forces flush + lands in next
  group as CB 1).
- `submit_group_descriptor_ring_does_not_reset_in_use_group` —
  if op N+1 in a group acquires from a pool the ring would
  normally consider safe to recycle, the recycle is gated on
  the group's shared fence signal, NOT on individual op
  acquire_generation.
- `submit_group_failure_handles_layout_state` — synthetic
  `queue_submit2` failure either restores per-drawable
  `current_layout` from snapshot OR sets `renderer_failed`
  (whichever the implementer chose). Assert both branches don't
  leave layouts inconsistent.
- `submit_group_rolls_back_drawable_render_tickets_on_failure`
  — synthetic `queue_submit2` failure restores the prior
  `last_render_ticket` on every drawable touched by the group.
- `submit_group_max_size_caps_growth` — 17 consecutive paint ops
  produce TWO submits (one of 16, one of 1).
- `submit_group_mixed_sequence_smoke` — a realistic ordering:
  COW batch open + cow_copy_area + render_batch open +
  render_composite + glyph_upload + composite_glyphs +
  get_image + scene_compose. Asserts the count of
  `vkQueueSubmit2` calls matches the expected flush trigger
  ordering.
- `submit_group_renderer_failed_path` — failure that can't roll
  back marks `renderer_failed`; subsequent ops short-circuit.

### Phase A open questions

- Max group size: 16 is a guess. Re-tune from bee/yoga captures.
  Too small wastes ioctl overhead; too large pressures descriptor
  pools.
- Scene compose's exact placement: today it's its own
  `queue_submit2` for the IN_FENCE_FD handoff; should it stay
  separate or join the paint group? Likely stay separate — the
  KMS handoff semantics are simpler with compose as its own
  submit.
- `wait_for_drawable_idle` was deleted in Task 15. Are there any
  remaining sync-wait paths that need to drive a SubmitGroup
  flush? Audit + document.

---

## Phase B — deferred op-list FrameScheduler (sketched, not detailed)

**Status: not plan-ready.** Needs another design pass before
moving to a plan. This section is a sketch so we don't lose the
overall direction; the actual spec for Phase B lands as a separate
document AFTER Phase A ships + we see what's left in the submit
budget.

### Sketch

Replace per-op-class CBs with a **per-frame op list**. Paint ops
record into a compact `RecordedOp` enum during frame-open; layout
transitions are *planned* but not yet barriered; at frame close
(scene compose), all planned barriers + paint draws + compose pass
get recorded into one primary CB and submitted once with
IN_FENCE_FD to KMS.

### Why deferred recording over open CB

Codex's review correctly noted that an "open CB on first paint op"
model can't work because scene compose chooses scanout BO +
descriptor pool slot + repaint mode at compose time — the frame
CB wouldn't yet know which BO/output/repaint path it targets.
Deferred recording into a compact op list keeps the BO/repaint
decision at frame-close where it belongs. Rollback is trivial
(drop the op list); no half-recorded Vulkan CB to clean up.

### Open design questions (the ones Phase B must answer)

These are why Phase B isn't plan-ready:

1. **Op representation.** `render_composite` isn't just params +
   draw — it resolves cached image views, descriptor sets,
   generation accounting, synthetic solid clears, gradient paint
   state, self-alias readback, dst-readback, clip scissors, layout
   mutation. Phase B must choose:
   - **Symbolic recording** (store protocol-ish inputs, resolve
     at close): risks resources changed/freed between append and
     close.
   - **Prepared recording** (snapshot + pin resources at append,
     replay at close): more lifetime pressure, partially defeats
     deferred-recording's simplicity.
2. **Glyph upload handling.** `composite_glyphs` submits glyph-
   upload CBs BEFORE the glyph draw. Phase B must answer "missing
   glyph during open frame": upload immediately (sub-submit), or
   batch atlas uploads into the frame submit, or force a frame
   flush before atlas mutation. Each has correctness + perf
   consequences.
3. **Transactional layout state.** `store.rs::record_layout_
   transition` mutates `Drawable::storage.current_layout` BEFORE
   the GPU executes the barrier. Phase B needs a tentative-state
   overlay during frame-open with commit/rollback at close. The
   concrete mutation inventory must be in the Phase B plan.
4. **Multi-output.** Per-output pending acks + BO rings today.
   "1 CB / 1 pageflip per frame" is wrong with multiple outputs.
   Phase B picks between (a) one frame CB signaling N output-
   specific semaphores or (b) N CBs in one `vkQueueSubmit2`.
   (b) generalises Phase A nicely.
5. **Idle / no-pageflip trigger model.** When does a frame close
   with no scene-dirty + no ready output? Phase B picks a
   millisecond-granularity timer + readiness gate.
6. **Frame-wide resource pinning.** Task 6.1 solved Arc-pinning
   for xshmfence/syncobj specifically. Phase B's frame builder
   must pin every image view, descriptor set, staging buffer,
   atlas page, sync object referenced by any op in the open
   frame. Larger surface than Task 6.1.

### Expected impact (Phase B, sketch)

- Phase B brings bee to ~5-10 submits/frame (approaching
  Xorg/glamor's 1-5).
- Predicted LOC: ~3000-5000 net, ~3-4 weeks focused work, on top
  of Phase A's foundation.
- Phases inside Phase B (sequence based on submit-trace ranking):
  RENDER widget paths first (75 % of budget), then COW + scene
  compose, then fills + PutImage, then readbacks as explicit
  flush points.

---

## Dependencies / unlock conditions

- Task 6.1 (deferred PRESENT completion) landed — PRESENT timing
  decoupled from per-op submit; Arc-pinning machinery is in place
  to lift to Phase B's frame builder.
- Task 3 (cow_batch aggregation) and Task 4 (DescriptorPoolRing)
  provide the foundations; this builds on both.
- **Phase A is plan-ready.** Should be its own Stage 5 sub-task
  (e.g. "Task 7 — multi-CB single-submit") with its own plan
  round.
- **Phase B is not plan-ready.** Re-spec it after Phase A ships
  + we have new submit-source ranking from the post-Phase-A bee
  capture.

## Out of scope (intentional)

- Direct scanout for fullscreen GL clients (wlroots-style).
- Plane composition (cursor + scene on different planes).
- Refactoring v1.
- "Open CB on first paint op" model — codex's review correctly
  identified that compose can't choose the scanout BO at op-record
  time, so deferred recording via an op list (Phase B) is the
  actual approach.
- Per-CB timeline-semaphore retirement (Model A2 — rejected for
  Phase A; reconsider in Phase B if Model A1 retention turns out
  to bind).

## References

- bee 2026-05-22 perf-branch capture: `docs/status.md` § "Bee
  hardware capture 2026-05-22" — establishes the per-`queue_submit2`
  kernel-round-trip baseline (~470 µs per submit, ~35
  submits/frame, ~2119 submits/s).
- bee 2026-05-23 post-Task-6.1 capture: `docs/status.md` §
  "2026-05-23 bee hardware close — Task 6.1 functionally fixed"
  — `queue_submit2/s` peak 3304, drag still laggy.
- bee 2026-05-23 submit-trace top sources: `yserver-mate.submit.
  tsv` on the May 23 run, captured separately on bee.
- Glamor design history: `gl-renderer.c` in xserver. The 2014 EXA
  → glamor migration is the closest analogue to Phase B.
- Task 6.1 spec:
  `docs/superpowers/specs/2026-05-23-deferred-present-completion-design.md`
  — the Arc-pinning + completion-semaphore-export machinery this
  spec builds on.
