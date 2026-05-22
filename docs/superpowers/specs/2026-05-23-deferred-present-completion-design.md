# Deferred PRESENT completion — replacement for the bee UAF fix

**Date:** 2026-05-23
**Stage:** 5 (perf), Task 6.1
**Scope:** Replace commit `8ca552a` (bee render-batch UAF + PRESENT wait deadlock fix) with a design that closes the UAF, restores Task 3 aggregation depth, removes the per-PRESENT synchronous GPU wait, and eliminates the shutdown-hang side effect.

## Motivation

Stage 5 Task 3 (paint-submit aggregation) introduced batched cow_copy_area + render_composite submissions. Each `vkCmdCopyImage` / `vkCmdDraw` is recorded into a pending CB that flushes at the next `maybe_composite` tick (or when an unrelated engine op forces a flush via the auto-flush hooks). On bee 2026-05-22 mate-panel's MIT-SHM PutImage churn surfaced a use-after-free: between append and flush, a client `FreePixmap` could destroy the VkImage that the batched CB still held a descriptor on. RADV's `addr_binding_report` named it: 256×256 d32 pixmap rebound 115 µs after FreePixmap, RDNA2 TCP boundary check faulted on the dangling sample.

Commit `8ca552a` patched this with two layers:

1. **Eager-touch at append time** — every `cow_copy_area` / `render_composite_open` / `render_composite` append calls `store.touch_render_fence(...)` on every drawable whose VkImageView is bound into the batch CB. Closes the UAF window: a subsequent `FreePixmap` on a touched src/mask/dst sees the (pending-batch) ticket and defers destruction.
2. **Flush-before-wait in `wait_for_drawable_idle`** — the PRESENT request handler calls `wait_for_drawable_idle(dst)` to block until the GPU has finished reading the source pixmap. Post-eager-touch, the ticket on the COW corresponded to an un-submitted CB, so `ticket.wait(...)` blocked indefinitely (5 s timeout, screen black). Layer 2 flushed any pending render+cow batch before reading `last_render_ticket`, restoring the invariant "ticket exists ⇒ CB submitted."

Both layers shipped together because the eager-touch alone introduced the wait-deadlock.

### Three observed failure modes of the fix

Captured on yoga (Snapdragon X1 / Adreno X1 / Turnip, 1× scale, 58 one-second buckets, MATE drag workload) by running master without commit `8ca552a` and comparing against the silence reference numbers:

1. **Task 3 aggregation depth collapses.** Marco issues `PRESENT::Pixmap` at frame rate. Each PRESENT calls `wait_for_drawable_idle`, which flushes any pending cow_batch. The accumulation window becomes "between PRESENTs" — one frame. Without the fix, yoga's cow_batch averages **8.5 calls / batch** (peak 8.66). With the fix in place, the average would collapse toward 1.0–2.0. The Task 3 win on the marco compositor pump silently mostly evaporates.

2. **Synchronous CPU wait at the request handler.** Even after the flush, the request handler thread blocks waiting for one frame of GPU work to complete. Multiple times per second on marco's drag pump. On the yoga reverted run, peak `cpu_fence_wait_ns/s = 273 ms` (with the fix on, would be substantially higher; on bee pre-revert this was the visible drag-lag mechanism).

3. **Shutdown hangs userspace.** On yoga, Ctrl-Alt-Backspace with the fix in place reliably wedges the machine in userspace (no kernel dmesg). Plausible cause: the eager-touched tickets + accumulated synchronous-wait state at `disable_output` time put VkDevice teardown into a state where it blocks on a fence Mesa polls without notification. Reverting the fix makes zap exit cleanly.

The yoga capture (`yserver-hw-mate.log` 2026-05-22 23:41, 58 buckets, 1× MATE scale, fix reverted) measured:

| metric | observed | silence post-POC ref |
|---|---:|---:|
| `paint_submits/s` drag avg | 2,693 | 4,180 |
| `paint_submits/s` peak | 9,087 | 14,814 |
| cow batch depth avg | **8.5** | 5.41 |
| cow batch depth peak | 8.66 | 46 |
| render batch depth avg | **2.27** | 1.43 |
| `descriptor_pool_creates/s` | 0 in 57/58 buckets | ~0 |

User-subjective: no CPU spikes, low frequency, drag feels native. Both proves Task 3 delivers on yoga when not gated by the bee fix AND establishes the design target.

## Goal

Replace commit `8ca552a` with a design that:

- Keeps the UAF closed across all hardware classes (bee, silence, yoga, fuji).
- Restores Task 3 aggregation depth on yoga and matches or beats silence's measured numbers.
- Removes the per-PRESENT synchronous GPU wait from the request-handler thread.
- Removes the shutdown-hang side effect on yoga.
- Fires `IdleNotify`, `CompleteNotify { mode: Copy }`, and the xshmfence trigger at the spec-correct moment ("GPU has finished reading the source pixmap"), asynchronously.

## Non-goals

- **Pageflip-side IN_FENCE_FD changes.** The scene-compose path at `kms/vk/compositor.rs:184` already exports `SYNC_FD` and consumes it on atomic commit; KMS waits in-kernel for the scene-compose GPU work before flipping. Because same-queue submission order guarantees cow_batch CB completes before scene-compose CB, the existing fence covers the cow_batch transitively. No new KMS plumbing.
- **Flip-mode PRESENT.** Path selection still returns Copy mode (the BO bridge for Flip hasn't landed). Flip-mode `CompleteNotify` timing (tied to pageflip-complete) is the same problem space but out of scope here.
- **Eager-touch redesign.** Layer 1 of the bee fix is correct and stays as-is. The UAF closure mechanism is not under reconsideration.
- **Removing `wait_for_drawable_idle` from non-PRESENT callers.** There's a second caller at `process_request.rs:5284` (CopyArea path); review separately. PRESENT::Pixmap is the load-bearing case.

## Architecture

The PRESENT::Pixmap request handler stops blocking on the GPU. Three actions previously sequenced as "wait → fire" become a single deferred unit, queued at PRESENT time and emitted asynchronously when the cow_batch fence signals:

1. `dri3_trigger_fence(idle_fence_xid)` — xshmfence trigger, the Mesa WSI wake mechanism.
2. `IdleNotify` event emission to clients subscribed via `PresentEventMaskIdleNotify`.
3. `CompleteNotify { mode: Copy }` event emission to clients subscribed via `PresentEventMaskCompleteNotify`.

The eager-touch (`8ca552a` layer 1) stays. The synchronous-wait + flush-before-wait machinery (`8ca552a` layer 2) is deleted. The scene-compose KMS pageflip path is untouched. The PRESENT handler's call to `wait_for_drawable_idle` is replaced with an enqueue.

Steady-state shape: PRESENT::Pixmap returns to the client in O(µs) — append into cow_batch + queue a pending-events entry. At the next `maybe_composite` tick (within ~16 ms at 60 Hz) the batch flushes and the GPU starts work. When the fence signals (typically within one GPU frame), the main loop's per-tick drain wakes Mesa and fires the events.

## Components

### `CompletedPresentEvent` (new struct, defined in `yserver-core` trait crate)

The trait crate (yserver-core) sees only the event-emission data, never the `FenceTicket`. `FenceTicket` is `pub(crate)` within the yserver crate and must not leak into the trait surface.

```rust
// In yserver-core (the trait crate) — visible to the main loop.
pub struct CompletedPresentEvent {
    /// `state.clients` key for the PRESENT issuer.
    pub client_id: ClientId,
    /// PRESENT request serial — echoed in CompleteNotify.
    pub serial: u32,
    /// Source pixmap host xid — what the eager-touched ticket guards.
    pub host_xid: u32,
    /// Destination drawable host xid (typically COW).
    pub dst_host_xid: u32,
    /// Idle fence xid (xshmfence). Zero if the client passed none.
    pub idle_fence_xid: u32,
    /// Original PRESENT options byte (PresentOption*). Echoed in events.
    pub options: u32,
}
```

### `PendingPresentEntry` (new struct, internal to yserver)

```rust
// In yserver — pairs the public event with the gating ticket.
struct PendingPresentEntry {
    /// Cow_batch ticket the just-appended copy_area participates in.
    /// `FenceTicket` is `Clone` (Arc-internally); cloning is cheap.
    /// Entry remains in the queue until `poll_signaled()` returns
    /// true.
    ticket: FenceTicket,
    event: CompletedPresentEvent,
}
```

### `KmsBackendV2.pending_present_events: VecDeque<PendingPresentEntry>`

New field on the backend. Owns the queue because the backend has the fence handles; pushing requires no cross-crate plumbing.

### `Backend::enqueue_present_completion(&mut self, event: CompletedPresentEvent)` (new trait method)

Called by the PRESENT request handler immediately after `backend.copy_area(...)`. The trait method takes the event struct by value; the backend implementation captures a clone of the **current cow_batch's FenceTicket** (the one the just-appended copy participates in) and pushes a `PendingPresentEntry { ticket, event }`.

Critical: this must read the same ticket the just-appended copy_area was joined to. The cow_batch's ticket is set when the batch is first opened (the open path constructs the CB + fence; subsequent appends reuse the same ticket). Implementation detail: expose an engine accessor `current_cow_batch_ticket() -> Option<FenceTicket>` that the backend reads inside `enqueue_present_completion`. If the cow_batch has no pending entry yet (would only happen if the copy_area went through the non-batched path, e.g. self-copy alias), enqueue with a synthetic "already-signaled" ticket so the entry drains on the next tick.

### `Backend::drain_completed_present_events(&mut self) -> Vec<CompletedPresentEvent>` (new trait method)

Returns the prefix of `pending_present_events` whose ticket has `poll_signaled() == true`. The backend implementation peels off `PendingPresentEntry` values, drops the ticket, and yields the event struct. Same-queue submission order means signals are monotone-by-position in the queue, so walk from the front and stop at the first unsignaled entry. Removed entries' events are returned by value to the caller; ownership transfers.

Called once per main loop iteration from `core_loop::run`, after the existing per-tick maintenance and after `on_page_flip_ready` handling. Also called inside the existing renderer-failed and shutdown paths (see §"Error handling").

### Main loop drain hook (`core_loop::run`)

After each loop iteration step where forward progress on GPU work might have happened (page-flip-ready dispatch, response to fd readiness), invoke the drain and fire each completed event via the existing emission helpers already in `process_request.rs`:

```rust
let completed = backend.drain_completed_present_events();
for entry in completed {
    if entry.idle_fence_xid != 0 {
        let _ = backend.dri3_trigger_fence(entry.idle_fence_xid);
        if let Some(f) = state.sync_fences.get_mut(&entry.idle_fence_xid) {
            f.triggered = true;
        }
    }
    // Existing fan-out helper (process_request.rs:~5384):
    fan_out_present_complete_and_idle(state, &entry);
}
```

The `fan_out_present_complete_and_idle` helper is extracted from the existing inline emission (`process_request.rs:5113-5170` approximately) so both the legacy code path (until this design lands) and the new drain-hook path call the same emission logic. The helper consumes a `CompletedPresentEvent` plus `&mut state` and emits to each subscribed client.

### PRESENT handler change (`process_request.rs:5040-5170`)

```diff
-                backend.wait_for_drawable_idle(dst.host_xid())?;
+                backend.enqueue_present_completion(CompletedPresentEvent {
+                    client_id: req.client_id,
+                    serial: req.serial,
+                    host_xid: host_xid.as_raw(),
+                    dst_host_xid: dst.host_xid(),
+                    idle_fence_xid: req.idle_fence,
+                    options: req.options,
+                });
                 backend.note_present_pixmap(host_xid.as_raw(), dst.host_xid());
                 // damage accounting unchanged
                 // ...
-                // immediate dri3_trigger_fence + sync_fences.triggered=true
-                // immediate fan-out of CompleteNotify + IdleNotify
+                // (emission deferred — main loop drains the queue when
+                //  the cow_batch fence signals)
```

The `present_scheduler.enqueue` call at `process_request.rs:5141` stays — that's a separate flip-path mechanism unrelated to the Copy-mode completion timing.

The second `wait_for_drawable_idle` call at `process_request.rs:5284` (inside the CopyArea pre-PRESENT optimisation path) is out of this design's scope; flag for a follow-up review but don't touch in this change.

## Data flow

```
Client → PRESENT::Pixmap(serial, pixmap, window, options, ..., idle_fence)
  │
  ▼
process_request.rs handle_present_pixmap:
  1. backend.copy_area(src_host_xid, cow_host_xid, ...)        # appended to cow_batch
  2. backend.enqueue_present_completion(client, serial, ...)   # pushed onto queue
  3. backend.note_present_pixmap(...)                           # existing diagnostic
  4. accumulate damage / present_scheduler.enqueue              # existing
  5. handler returns Ok(())                                     # **no synchronous wait**

(later, ~µs to ~16 ms)
  ▼
maybe_composite tick:
  - engine.flush_cow_batch()  → cow_batch CB submitted, ticket bound
  - engine.flush_render_batch() → render_batch CB submitted (FIFO after cow)
  - scene tick → scene-compose CB submitted (FIFO after render+cow)
                 atomic-commit pageflip queued with scene-compose
                 SYNC_FD as IN_FENCE_FD

(later, GPU finishes; typically ≤ one frame)
  ▼
cow_batch FenceTicket.poll_signaled() → true on next main loop iter

main loop drain:
  ▼
  for entry in backend.drain_completed_present_events():
      backend.dri3_trigger_fence(entry.idle_fence_xid)
        → wakes Mesa WSI thread (xshmfence futex)
      fan_out_present_complete_and_idle(state, &entry)
        → emits CompleteNotify { mode: Copy } + IdleNotify
        → per-subscribed-client per-window event mask
```

## Lifetime invariants

**I1. Eager-touch keeps the source pixmap alive for the batched CB.** Unchanged from `8ca552a` layer 1. A `FreePixmap` between the cow_copy_area append and the cow_batch flush sees the touched ticket on the drawable and defers destruction via `store.decref → RetireDecision::Pending`. The batched CB always finds a live VkImageView.

**I2. The pending-events queue retires in submission order.** Each entry holds a clone of a cow_batch FenceTicket. Same-queue submission order (`kms/v2/engine.rs` paint queue) means batch N signals before batch N+1. `drain_completed_present_events` walks the queue front-to-back and stops at the first unsignaled entry — every signaled entry is at a contiguous prefix. No reordering, no priority inversion.

**I3. Drain is called at least once per main loop iteration.** The drain hook is on the iteration epilogue (after `on_page_flip_ready` and per-fd dispatch). Under steady load this fires within sub-ms of a fence signal. Under idle, the next event (input, timer, fd readiness) drives an iteration which drains. There is no path where a signaled entry sits in the queue without being drained on the next loop iter.

**I4. Mesa WSI wake order matches event emission order.** Both the xshmfence trigger and the X11 event emission fire from the same drain loop. For a given entry, the trigger fires immediately before the events. Mesa's WSI thread can wake on the futex before the X11 event reaches its dispatcher; the spec allows this (xshmfence is the load-bearing wake, X11 IdleNotify is the audit signal).

**I5. PRESENT events never get dropped.** All paths that abandon a queue entry (`disable_output`, `RendererFailed`) fire the events with the appropriate status before discarding (see §"Error handling"). No client is left blocked on a fence that will never signal.

## Error handling

### Renderer failure (`RendererFailed`)

`KmsBackendV2.platform.renderer_failed` flips to `true` on a Vk fatal-error path (device lost, validation hit). All subsequent engine paint paths short-circuit. The pending-events queue is then a list of clients waiting on fences that may never signal because no further GPU work will run.

Behaviour: on first `drain_completed_present_events` call after `renderer_failed == true`, drain the **entire** queue regardless of ticket signal state; fire all events with mode `Copy` (per the legacy semantics — completing-with-best-effort matches what the pre-bee-fix code did on the failure path). Log once.

### Shutdown (`KmsBackendV2::disable_output`)

`disable_output` already drains in-flight engine + scene submits. After those drains complete (the existing `engine.drain_all` walks `submitted` and waits on each ticket in order), every cow_batch ticket in the pending-events queue has either signaled or the renderer is failed. Drain the queue in full; fire all entries; if any ticket is still unsignaled, fire anyway (we're shutting down; clients receive their event and can clean up).

Order inside `disable_output`:
1. `engine.drain_all(&self.platform)` — waits on each in-flight SubmittedOp ticket.
2. `self.sync_descriptor_pool_telemetry()` — existing.
3. `self.scene.drain_all(&mut self.platform)` — existing.
4. **`self.drain_pending_present_events_for_shutdown()`** — new; drain unconditionally, return list to caller.
5. `self.telemetry.flush_submit_trace()` — existing.
6. `self.platform.disable_output()` — existing.

Step 4 returns the entries to the caller (`lib.rs::run`), which emits them to clients before tearing down the socket. The emission has to happen before `fs::remove_file(&socket_path)` so the events make it onto the wire.

### Fence wait failure / per-entry timeout

If `ticket.poll_signaled()` returns an error (Vk failure observing the fence), treat the entry as signaled — fire its events with a one-line `log::warn!` on the first occurrence, then continue. Stuck-fence pathology should never happen with engine retirement working correctly; if it does, falling back to "fire and warn" is better than blocking.

Optional: log once if an entry has been pending more than 1 second wallclock (sentinel against subtle livelock). Not load-bearing — engine retirement is the load-bearing mechanism.

## Telemetry

Three additions to `kms::v2::telemetry::Bucket` and the per-second emitter line:

- `pending_present_events_depth_max` — peak queue depth observed during the second.
- `pending_present_events_emitted_per_s` — count of entries drained + emitted per second.
- `pending_present_events_force_fired_per_s` — count of entries fired without ticket-signal (shutdown / renderer-failed / wait error). Should be 0 in healthy operation.

The first two confirm the deferred-completion path is firing at the cadence Task 3 expects (one batch per maybe_composite tick, so peak depth bounded by per-frame PRESENT count, typically single digits). The third is a health gate — non-zero in steady state means the engine isn't retiring tickets properly.

## Testing

### Unit (backend)

`crates/yserver/src/kms/v2/backend.rs` (or a new `present_completion.rs` module):

- **`enqueue_present_completion_records_entry_with_current_ticket`** — open a cow_batch via `engine.cow_copy_area`, call `enqueue_present_completion`, assert the queue grew by 1 and the entry's ticket equals the cow_batch's ticket.
- **`drain_signaled_returns_prefix_in_fifo_order`** — enqueue 3 entries against tickets T1 < T2 < T3 (mock — see §"Test fixtures" below); pre-signal T1 + T2 only; assert drain returns [E1, E2] and leaves E3.
- **`drain_unsignaled_returns_empty`** — enqueue 2 entries; signal neither; assert drain returns empty + queue unchanged.
- **`drain_handles_out_of_order_signal_safely`** — pathological: T1 unsignaled, T2 signaled. By I2 this shouldn't happen, but if it does, drain returns nothing (preserves FIFO discipline; T2 will fire when T1 fires). Defensive test.

### Test fixtures

`FenceTicket` doesn't currently have a test-only "force signaled" knob; add one as `#[cfg(test)] fn test_mark_signaled(&self)` that sets the cached-signal bool to true. Mirrors the existing `#[cfg(test)]` knobs on `DescriptorPoolRing`.

### Integration (engine retirement)

`crates/yserver/tests/v2_acceptance.rs`:

- **`v2_present_pixmap_enqueues_pending_and_defers_emission`** — drive the v2 backend's `Backend::copy_area` + `enqueue_present_completion`, assert (a) no synchronous wait was made (latency on the entry call < 100 µs), (b) the entry sits in the queue, (c) after a forced maybe_composite tick + waiting on retirement, drain returns the entry.
- **`v2_present_pixmap_drain_returns_in_fifo_order_against_live_vk`** — Vk-backed: issue 3 PRESENT-shaped sequences (copy_area + enqueue) targeting the same COW under lavapipe; flush + retire all; drain returns 3 entries in submission order.
- **`v2_present_aggregation_depth_unchanged_after_design`** — issue 10 PRESENT-shaped sequences without any intervening retirement, assert the cow_batch flushes ONCE (depth 10) — i.e. the deferred-completion design doesn't accidentally re-introduce a flush via the enqueue path.

### Regression (the bee UAF stays closed)

- **`v2_present_pixmap_then_free_pixmap_keeps_source_alive`** — issue a copy_area + enqueue + immediate FreePixmap on the source; the eager-touched ticket on the source pixmap defers destruction; force a CB flush; the CB still sees a valid VkImageView (no crash, no validation warning). The existing `8ca552a` regression tests stay valid:
  - `render_composite_open_marks_src_last_render_ticket_immediately`
  - `render_composite_open_then_decref_src_returns_pending_fence`
  - `cow_copy_area_open_marks_src_last_render_ticket_immediately`

### Hardware acceptance (user-driven smoke)

- **bee** (Ryzen 9 6900HX / RDNA2 / RADV): MATE drag must not wedge. The original UAF symptom (`ERROR_DEVICE_LOST` flood, addr_binding_report fault) must not reproduce. The lag-floor of the current bee fix (synchronous PRESENT wait stacking marco frame work on the request handler) should be substantially better.
- **yoga** (Snapdragon X1 / Adreno X1 / Turnip): MATE drag should match the user-perceived "snappy, low CPU, low Hz" state of the post-revert measurement. Telemetry: `cow_batches_flushed/s` average depth in the 5-10 range (silence reference 5.41, yoga reverted 8.5).
- **silence** (i9 13900k / rx580 / RADV): MATE drag perf characteristic unchanged from the existing post-Task-3 baseline. The deferred completion shouldn't change anything for silence (it was never on the flush-on-PRESENT path's binding side).
- **fuji** (i5-7200U / Intel HD 620 / ANV): sanity check that the design doesn't introduce a new failure on Intel.

### Smoke gate for regression

`xshmfence` triggers — the most subtle thing to break — are gated by Mesa WSI clients (xterm, xeyes, anything using DRI3 with explicit-sync). A drag of any GTK app where Mesa's vkAcquireNextImage is on the critical path will hang within 1-2 frames if the trigger doesn't fire. Smoke matrix already includes Mesa-WSI workloads.

## Risks

**R1. Stale ticket on the COW during the brief window between PRESENT enqueue and cow_batch flush.** Pre-fix code path: `wait_for_drawable_idle(COW)` read `COW.last_render_ticket` and returned immediately on `None` (the broken pre-bee-fix behaviour). Post-design: nobody reads `COW.last_render_ticket` on the PRESENT path. Other callers of `wait_for_drawable_idle` (the CopyArea optimisation path at `process_request.rs:5284`) still depend on it; flag for follow-up review.

**R2. Main loop iteration starvation under sustained busy-CPU.** If the loop iterates rarely (e.g. compute-bound libinput backlog), pending events sit unfired. Mitigation: drain is in the per-iteration epilogue; the loop iterates on every fd-ready event including the DRM page-flip fd, so under any visual workload (vblanks at 60 Hz minimum) the drain runs ≥ 60 times per second. Idle CPU isn't the failure surface — there are no PRESENTs queued during idle.

**R3. Event ordering vs. xshmfence wake-up race.** Mesa WSI threads wake on the xshmfence trigger; they're racing the X11 event dispatcher (separate thread / fd). Per spec the xshmfence trigger is the load-bearing wake; the X11 event is an audit signal. Fine to race. Both fire from the same drain loop iteration, so they're at least logically simultaneous.

**R4. Test coverage of the bee scenario without bee hardware.** The eager-touch unit tests + the existing `8ca552a` regression suite gate the UAF closure logically. The fault would only reproduce on RDNA2 iGPU under heavy churn (per the platform analysis in the original commit message). The acceptance gate is logical — if the eager-touch path is exercised correctly, the UAF is closed on every platform that runs it.

**R5. Telemetry counters may show transient queue-depth spikes during warm-up.** Marco's startup issues a burst of PRESENTs before the first flush; queue depth could briefly exceed expected steady-state. Not a correctness issue; flag in the per-second emitter logs only as a record of warm-up shape.

**R6. `Backend` trait surface grows.** Two new methods. The v1 backend (still in tree per Stage 5 v1-deletion gates) needs stub implementations — return empty for `drain_completed_present_events`; no-op for `enqueue_present_completion`. v1 still uses the synchronous wait at the same call site; that path is unchanged. (v1 doesn't have the Task 3 batching, so it never has a pending ticket without submit.)

## Out-of-scope follow-ups

- **`wait_for_drawable_idle` second caller at `process_request.rs:5284`.** Same conceptual hazard as the PRESENT path. Separate review and design — likely the same deferred-completion pattern, possibly the same queue, possibly a different mechanism if the CopyArea optimisation path's semantics differ.
- **Flip-mode PRESENT.** When the BO bridge lands and PRESENT can pick Flip instead of Copy, `CompleteNotify` timing for the Flip path needs to tie to pageflip-complete (different from Copy's "GPU done with source"). Same queue infrastructure can serve both with a mode discriminant on `CompletedPresentEvent`.
- **Removing `wait_for_drawable_idle` entirely.** Once both PRESENT and CopyArea callers move to deferred completion, the method has no remaining callers and can be deleted. Track once the second call site is migrated.

## Capture recipe (post-fix verification)

After landing, re-run the same yoga MATE-drag workload that produced the 2026-05-22 reverted-state telemetry. Expected on yoga (Adreno X1 / Turnip):

| metric | pre-design (with `8ca552a`) | post-design | change |
|---|---:|---:|---:|
| cow batch depth avg | ~1.5 (estimated; not captured) | **≥ 7** | restored |
| render batch depth avg | ~1.5 | **≥ 2.0** | restored |
| `cpu_fence_wait_ns/s` avg | high (synchronous PRESENT wait tax) | **near zero** | wait deleted |
| `paint_submits/s` drag avg | ~3500–4000 | **≤ 3000** | aggregation restored |
| Subjective drag | initial slow then speeds up | native-feeling | warm-up cost remains, native steady-state |

Plus: bee MATE drag must not wedge under the same workload that triggered the original UAF. The synchronous-wait lag floor on bee should drop substantially.

Capture via `just yserver-mate-hw-telemetry`. SSH from another machine + `pkill -TERM yserver` for clean exit (the kernel SysRq rebuild on yoga is in flight; don't zap until that lands).
