# Deferred PRESENT completion — replacement for the bee use-after-free fix

**Date:** 2026-05-23
**Stage:** 5 (perf), Task 6.1
**Scope:** Replace commit `8ca552a` (bee render-batch use-after-free + PRESENT wait deadlock fix) with a design that closes the use-after-free, restores Task 3 aggregation depth, removes the per-PRESENT synchronous GPU wait, and eliminates the shutdown-hang side effect.

## Motivation

Stage 5 Task 3 (paint-submit aggregation) introduced batched cow_copy_area + render_composite submissions. Each `vkCmdCopyImage` / `vkCmdDraw` is recorded into a pending CB that flushes at the next `maybe_composite` tick (or when an unrelated engine op forces a flush via the auto-flush hooks). On bee 2026-05-22 mate-panel's MIT-SHM PutImage churn surfaced a use-after-free: between append and flush, a client `FreePixmap` could destroy the VkImage that the batched CB still held a descriptor on. RADV's `addr_binding_report` named it: 256×256 d32 pixmap rebound 115 µs after FreePixmap, RDNA2 TCP boundary check faulted on the dangling sample.

Commit `8ca552a` patched this with two layers:

1. **Eager-touch at append time** — every `cow_copy_area` / `render_composite_open` / `render_composite` append calls `store.touch_render_fence(...)` on every drawable whose VkImageView is bound into the batch CB. Closes the use-after-free window: a subsequent `FreePixmap` on a touched src/mask/dst sees the (pending-batch) ticket and defers destruction.
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

- Keeps the use-after-free closed across all hardware classes (bee, silence, yoga, fuji).
- Restores Task 3 aggregation depth on yoga and matches or beats silence's measured numbers.
- Removes the per-PRESENT synchronous GPU wait from the request-handler thread.
- Removes the shutdown-hang side effect on yoga.
- Fires `IdleNotify`, `CompleteNotify { mode: Copy }`, and the xshmfence trigger at the spec-correct moment ("GPU has finished reading the source pixmap"), asynchronously.

## Non-goals

- **Pageflip-side IN_FENCE_FD changes.** The scene-compose path at `kms/vk/compositor.rs:184` already exports `SYNC_FD` and consumes it on atomic commit; KMS waits in-kernel for the scene-compose GPU work before flipping. Because same-queue submission order guarantees cow_batch CB completes before scene-compose CB, the existing fence covers the cow_batch transitively. No new KMS plumbing.
- **Flip-mode PRESENT.** Path selection still returns Copy mode (the BO bridge for Flip hasn't landed). Flip-mode `CompleteNotify` timing (tied to pageflip-complete) is the same problem space but out of scope here.
- **Eager-touch redesign.** Layer 1 of the bee fix is correct and stays as-is. The use-after-free closure mechanism is not under reconsideration.
- **`wait_for_drawable_idle` callers outside PRESENT request paths.** Only the two PRESENT call sites are in scope — `Pixmap` at `process_request.rs:5055` and `PixmapSynced` at `process_request.rs:5284`. Both paths are covered by this design (see §"PRESENT handler change" below). The method itself stays in the trait surface for future non-PRESENT callers if any emerge; today, after this change, it has no live callers in tree.

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

Two PRESENT request paths feed the same queue: `Pixmap` (async wake via xshmfence) and `PixmapSynced` (explicit-sync wake via DRM syncobj timeline point). They share the same deferred-completion mechanism with different fields carrying the wake target.

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
    /// Original PRESENT options byte (PresentOption*). Echoed in events.
    pub options: u32,
    /// Wake target. Carries the per-path wake mechanism the request
    /// originally specified.
    pub wake: PresentWake,
}

pub enum PresentWake {
    /// `Pixmap` path. `idle_fence` is the xshmfence resource id;
    /// `0` means no xshmfence was attached. `dri3_trigger_fence` fires
    /// after GPU completion.
    Pixmap { idle_fence_xid: u32 },
    /// `PixmapSynced` path. `release_syncobj` + `release_value` name
    /// the DRM syncobj timeline point to signal after GPU completion
    /// via `dri3_signal_syncobj`. `release_syncobj == 0` means no
    /// release object was attached.
    PixmapSynced { release_syncobj: u32, release_value: u64 },
}
```

### `PendingPresentEntry` (new struct, internal to yserver)

```rust
// In yserver — pairs the public event with the gating ticket plus
// the resources the deferred drain needs to keep alive past
// client-side destroy (FreeSyncobj / XFixesDestroyFence).
struct PendingPresentEntry {
    /// Cow_batch ticket the just-appended copy_area participates in.
    /// `FenceTicket` is `Clone` (Arc-internally); cloning is cheap.
    /// Entry remains in the queue until `poll_signaled()` returns
    /// true.
    ticket: FenceTicket,
    /// Wake-target lifetime pin. At enqueue time we take an internal
    /// `Arc` clone of the actual underlying primitive (the VkSemaphore
    /// backing the DRM syncobj, or the xshmfence-shared-memory
    /// segment), independent of the X11 resource id in the `event`
    /// payload. This survives a mid-flight `FreeSyncobj` /
    /// `XFixesDestroyFence` so the deferred signal at drain time
    /// always has a live target. The id in `event` is retained for
    /// logging only.
    wake_pin: PinnedWake,
    /// sync_file FD exported from `ticket.fence` via
    /// `vkGetFenceFdKHR(SYNC_FD)`. Registered with the backend's
    /// internal `present_completion_epfd` at enqueue, unregistered +
    /// closed at drain or on Drop. Linux sync_file semantics: poll
    /// reports `POLLIN` when the fence signals.
    fence_fd: OwnedFd,
    event: CompletedPresentEvent,
}

enum PinnedWake {
    Pixmap(Arc<XshmfenceSegment>),     // DRI3 xshmfence shared-memory
                                       // handle, refcounted in the
                                       // existing `state.sync_fences`
                                       // model; this Arc keeps it
                                       // alive past the resource id's
                                       // destruction.
    PixmapSynced(Arc<vk::Semaphore>),  // The VkSemaphore the DRM
                                       // syncobj wraps. The xid in the
                                       // event is for log lines only.
}
```

If the underlying primitives don't already exist as `Arc`-able handles, the implementation adds a thin Arc wrapper in the backend's syncobj/xshmfence registries (one alloc per fence; small).

### `KmsBackendV2.pending_present_events: VecDeque<PendingPresentEntry>`

New field on the backend. Owns the queue because the backend has the fence handles; pushing requires no cross-crate plumbing.

### `KmsBackendV2.present_completion_epfd: OwnedFd` + `KmsBackendV2.wakeup_eventfd: OwnedFd`

Two new FDs on the backend, both created at init:

- **`present_completion_epfd`** — an `epoll_create1(EPOLL_CLOEXEC)` FD. The backend uses it to aggregate the per-entry `fence_fd` sync_files: `epoll_ctl(EPOLL_CTL_ADD)` at enqueue, `EPOLL_CTL_DEL` + `close()` at drain. The main loop sees this single FD via `poll_fds()` and never sees the per-entry FDs directly. Mirrors the `wl_event_loop`-on-epoll pattern Wayland compositors use.

- **`wakeup_eventfd`** — an `eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK)` FD, **registered with `present_completion_epfd` once at init** under `EPOLLIN`. The backend writes 1 to it (with `write(2)`) whenever a drain needs to fire without a per-entry FD signal — specifically: (a) `vkGetFenceFdKHR` returned `-1` for an already-signaled fence at enqueue (no per-entry FD was registered), (b) any future force-wake condition. The drain handler reads from the eventfd to clear it after processing.

When any watched source becomes readable (per-entry sync_file OR wakeup_eventfd), the inner epoll FD itself becomes readable, which wakes the outer main-loop poller via the stable FD exposed in `poll_fds()`. This wake mechanism does not depend on `next_wakeup()` semantics and is robust against any execution context that calls `enqueue_present_completion` — including future async paths.

### `Backend::enqueue_present_completion(&mut self, event: CompletedPresentEvent)` (new trait method)

Called by the PRESENT request handler immediately after `backend.copy_area(...)`. The trait method takes the event struct by value; the backend implementation captures a clone of the **current cow_batch's FenceTicket** (the one the just-appended copy participates in) and pushes a `PendingPresentEntry { ticket, event }`.

Critical: this must read the same ticket the just-appended copy_area was joined to. The cow_batch's ticket is set when the batch is first opened (the open path constructs the CB + fence; subsequent appends reuse the same ticket). Implementation detail: expose an engine accessor `current_cow_batch_ticket() -> Option<FenceTicket>` that the backend reads inside `enqueue_present_completion`. If the cow_batch has no pending entry yet (would only happen if the copy_area went through the non-batched path, e.g. self-copy alias), enqueue with a synthetic "already-signaled" ticket so the entry drains on the next tick.

### `Backend::drain_completed_present_events(&mut self) -> Vec<CompletedPresentEvent>` (new trait method)

Returns the prefix of `pending_present_events` whose ticket has `poll_signaled() == true`. The backend implementation peels off `PendingPresentEntry` values, drops the ticket, and yields the event struct. Same-queue submission order means signals are monotone-by-position in the queue, so walk from the front and stop at the first unsignaled entry. Removed entries' events are returned by value to the caller; ownership transfers.

Called once per main loop iteration from `core_loop::run`, after the existing per-tick maintenance and after `on_page_flip_ready` handling. Also called inside the existing renderer-failed and shutdown paths (see §"Error handling").

### Main loop drain hook (`core_loop::run`)

After each loop iteration step where forward progress on GPU work might have happened (page-flip-ready dispatch, response to fd readiness, or wake-source readiness — see §"Wake source"), invoke the drain and fire each completed event via the existing emission helpers already in `process_request.rs`:

```rust
let completed = backend.drain_completed_present_events();
for entry in completed {
    match entry.wake {
        PresentWake::Pixmap { idle_fence_xid } if idle_fence_xid != 0 => {
            let _ = backend.dri3_trigger_fence(idle_fence_xid);
            if let Some(f) = state.sync_fences.get_mut(&idle_fence_xid) {
                f.triggered = true;
            }
        }
        PresentWake::PixmapSynced { release_syncobj, release_value }
            if release_syncobj != 0 =>
        {
            if let Err(e) = backend.dri3_signal_syncobj(release_syncobj, release_value) {
                log::warn!(
                    "PRESENT::PixmapSynced deferred signal {release_syncobj:#x}@{release_value} failed: {e}"
                );
            }
        }
        _ => {} // no wake object attached
    }
    // Existing fan-out helper (process_request.rs:~5384), extracted
    // so both legacy and new paths share emission logic.
    fire_present_completion_events(state, &entry);
}
```

The `fire_present_completion_events` helper is extracted from the existing inline emission (`process_request.rs:5113-5170` and the fan-out at line 5384) so both the legacy code path (until this design lands) and the new drain-hook path call the same emission logic. **Emission order is `IdleNotify` first, then `CompleteNotify { mode: Copy }`** — matches the existing site at `process_request.rs:5513` and Mesa's documented expectation. The helper consumes a `CompletedPresentEvent` plus `&mut ServerState` and emits to each subscribed client.

### PRESENT handler change

Both PRESENT call sites get the same shape — the synchronous wait is replaced with an enqueue. They diverge only on the wake variant.

**`PRESENT::Pixmap` (`process_request.rs:5055`):**

```diff
-                backend.wait_for_drawable_idle(dst.host_xid())?;
+                backend.enqueue_present_completion(CompletedPresentEvent {
+                    client_id,
+                    serial: req.serial,
+                    host_xid: host_xid.as_raw(),
+                    dst_host_xid: dst.host_xid(),
+                    options: req.options,
+                    wake: PresentWake::Pixmap { idle_fence_xid: req.idle_fence },
+                });
                 backend.note_present_pixmap(host_xid.as_raw(), dst.host_xid());
                 // damage accounting unchanged
                 // ...
-                // immediate dri3_trigger_fence + sync_fences.triggered=true
-                // immediate fan-out of IdleNotify + CompleteNotify
+                // (emission deferred — main loop drains the queue when
+                //  the cow_batch fence signals)
```

The `present_scheduler.enqueue` call at `process_request.rs:5141` stays — that's a separate flip-path mechanism unrelated to the Copy-mode completion timing.

**`PRESENT::PixmapSynced` (`process_request.rs:5284`):**

```diff
                 src_depth == dst.depth()
                     && backend.copy_area(/* ... */).is_ok()
-                    && backend.wait_for_drawable_idle(dst.host_xid()).is_ok()
+                    && {
+                        backend.enqueue_present_completion(CompletedPresentEvent {
+                            client_id,
+                            serial: req.serial,
+                            host_xid: host_xid.as_raw(),
+                            dst_host_xid: dst.host_xid(),
+                            options: masked_options,
+                            wake: PresentWake::PixmapSynced {
+                                release_syncobj: req.release_syncobj,
+                                release_value: req.release_value,
+                            },
+                        });
+                        true
+                    }
```

The immediate `dri3_signal_syncobj` call at `process_request.rs:5300-5308` is deleted; the signal fires deferred from the main loop drain hook with the same arguments.

`copied_to_dst` semantics preserved: the eager-touched cow_batch + the queued completion together replace the synchronous copy-then-wait that this flag previously asserted. The follow-on event fan-out at `process_request.rs:5310+` continues to fire `CompleteNotify`/`IdleNotify`; that fan-out is replaced by the deferred-drain emission, same as the `Pixmap` path.

## Wake source

A pending entry needs the main loop to iterate so its fence can be polled. The existing loop wakes on input fd readiness, DRM page-flip events, deferred requests, repeat-key deadlines, or `Backend::next_wakeup()`. None of these fire deterministically when only a GPU fence signals during an otherwise-idle period (e.g. a single PRESENT followed by quiet — no page-flip queued, no input, no timers).

The main loop's poll set is registered once at `run_core` startup; `poll_fds()` is not currently re-called per iteration, so per-entry FDs added dynamically cannot reach the outer epoll. The wake source therefore needs a **single stable FD** exposed via `poll_fds()` once at startup, behind which the backend dynamically multiplexes per-entry sync_files.

**Mechanism: backend-internal epoll FD aggregating per-entry sync_files.**

- At backend init, `KmsBackendV2` creates `present_completion_epfd = epoll_create1(EPOLL_CLOEXEC)`. Owned for the backend's lifetime.
- `poll_fds()` returns one new entry: `(present_completion_epfd.as_raw_fd(), BackendFdKind::PresentCompletion)`. Stable; never changes. No update path needed in the outer loop.
- `enqueue_present_completion` exports the `FenceTicket`'s underlying `VkFence` as a sync_file FD via `VK_KHR_external_fence_fd` + `vkGetFenceFdKHR(VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT_KHR)`. Two outcomes:
  - Valid FD returned: register with the inner epoll via `epoll_ctl(present_completion_epfd, EPOLL_CTL_ADD, fence_fd, EPOLLIN)`. The FD is stored on the entry.
  - `-1` returned (already-signaled fast path): no per-entry FD to register. Instead, write 1 to `wakeup_eventfd` (which is itself registered on the inner epoll). This makes the inner epoll readable immediately; the outer loop will drain on its next `poll()`.
- When any watched source on the inner epoll becomes readable (per-entry sync_file OR wakeup_eventfd), the inner epoll FD itself becomes readable. The outer mio poller in `run_core` wakes; the loop dispatches to `BackendFdKind::PresentCompletion` and calls `backend.drain_completed_present_events()`.
- Drain walks `pending_present_events` front-to-back, polling each entry's `ticket.poll_signaled()`. For signaled entries: `epoll_ctl(EPOLL_CTL_DEL)` the entry's FD (if it had one), `close()` it, pop the entry, return the event payload. Drain also reads from `wakeup_eventfd` to clear it if it fired (eventfd in non-blocking mode; `read` returns `EAGAIN` if not signaled — that's the no-op case).

This mirrors how Wayland compositors (mutter, kwin, sway) drive event loops on sync_file FDs — one inner epoll per compositor, registered once with the outer loop, dynamically updated as clients come and go.

**`BackendFdKind::PresentCompletion`** — new variant on the trait enum. Outer-loop dispatch routes readiness to the drain hook (same shape as the existing `BackendFdKind::Drm` arm dispatching to `on_page_flip_ready` at `core_loop::run::~330`).

**`VK_KHR_external_fence_fd` availability.** Already enabled on the v2 `VkContext` for the scanout-side `export_semaphore` path (`kms/vk/scanout.rs:1064`); the fence-variant of the same extension is part of the same Vulkan capability set. The plan verifies the extension at `VkContext::new()` and errors at init if absent.

**Sync_file caveats** (per `vkGetFenceFdKHR` docs): on Linux the SYNC_FD handle type yields a Linux Sync File which is poll-readable on signal — what we want. `vkGetFenceFdKHR` returns `-1` for a fence that is already signaled at export time (the kernel optimisation for the "already done" case); the enqueue path treats `-1` as "drain immediately on the next loop iteration" and skips the epoll registration. On Android `SYNC_FD` may export an Android-fence form rather than Linux Sync File — not a target for yserver, but worth flagging if a future port matters.

**Fallback (degraded mode, not part of the design but worth documenting for the plan):** if `VK_KHR_external_fence_fd` turns out unavailable on some Vk impl yserver supports, `Backend::next_wakeup()` returning `Some(now + 1 ms)` while the queue is non-empty is a polling-based fallback. The spec requires the FD path; the fallback is a known-acceptable degraded option if a porting need surfaces.

## Data flow

```
Client → PRESENT::Pixmap(serial, pixmap, window, options, ..., idle_fence)
  │
  ▼
process_request.rs handle_present_pixmap:
  1. backend.copy_area(src_host_xid, cow_host_xid, ...)        # appended to cow_batch
  2. backend.enqueue_present_completion(CompletedPresentEvent  # pushed onto queue
       { ..., wake: PresentWake::Pixmap { idle_fence } })       # backend exports
                                                                # fence as sync_file FD
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
sync_file FD becomes readable → epoll wakes main loop

main loop drain:
  ▼
  for entry in backend.drain_completed_present_events():
      match entry.wake:
        Pixmap { idle_fence_xid } if != 0:
          backend.dri3_trigger_fence(idle_fence_xid)
            → wakes Mesa WSI thread (xshmfence futex)
        PixmapSynced { release_syncobj, release_value } if != 0:
          backend.dri3_signal_syncobj(release_syncobj, release_value)
            → signals client's DRM syncobj timeline point
      fire_present_completion_events(state, &entry)
        → emits IdleNotify FIRST, then CompleteNotify { mode: Copy }
        → per-subscribed-client per-window event mask
```

## Lifetime invariants

**I1. Eager-touch keeps the source pixmap alive for the batched CB.** Unchanged from `8ca552a` layer 1. A `FreePixmap` between the cow_copy_area append and the cow_batch flush sees the touched ticket on the drawable and defers destruction via `store.decref → RetireDecision::Pending`. The batched CB always finds a live VkImageView.

**I2. The pending-events queue retires in submission order.** Each entry holds a clone of a cow_batch FenceTicket. Same-queue submission order (`kms/v2/engine.rs` paint queue) means batch N signals before batch N+1. `drain_completed_present_events` walks the queue front-to-back and stops at the first unsignaled entry — every signaled entry is at a contiguous prefix. No reordering, no priority inversion.

**I3. The main loop wakes when any pending entry's fence signals.** For Linux `SYNC_FD` exports, poll/epoll reports `POLLIN` when the fence transitions to signaled. The backend's inner `present_completion_epfd` (§"Wake source") aggregates every pending entry's sync_file FD plus a backend-owned `wakeup_eventfd`; the outer mio poller in `run_core` watches that one epoll FD. The kernel makes the inner epoll FD readable when any watched source becomes readable, which wakes the outer loop unconditionally. There is no path where a signaled entry sits in the queue without the loop iterating to drain it — even with zero other activity, the inner-epoll readiness is sufficient. Special case: `vkGetFenceFdKHR` returns `-1` when the fence is already signaled at export time; enqueue writes 1 to `wakeup_eventfd`, which makes the inner epoll readable immediately, which wakes the outer loop on the next `poll()` regardless of whether `next_wakeup()` has been re-consulted. This is the load-bearing wake mechanism — `next_wakeup` is not relied upon.

**I4. Mesa WSI wake order matches event emission order.** Both the xshmfence trigger and the X11 event emission fire from the same drain loop. For a given entry, the trigger fires immediately before the events. Mesa's WSI thread can wake on the futex before the X11 event reaches its dispatcher; the spec allows this (xshmfence is the load-bearing wake, X11 IdleNotify is the audit signal).

**I5. PRESENT events never get dropped.** All paths that abandon a queue entry (`disable_output`, `RendererFailed`) fire the events with the appropriate status before discarding (see §"Error handling"). No client is left blocked on a fence that will never signal.

## Error handling

### Renderer failure (`RendererFailed`)

`KmsBackendV2.platform.renderer_failed` flips to `true` on a Vk fatal-error path (device lost, validation hit). All subsequent engine paint paths short-circuit. The pending-events queue is then a list of clients waiting on fences that may never signal because no further GPU work will run.

Behaviour: on first `drain_completed_present_events` call after `renderer_failed == true`, drain the **entire** queue regardless of ticket signal state; fire all events with mode `Copy` (per the legacy semantics — completing-with-best-effort matches what the pre-bee-fix code did on the failure path). Log once.

### Shutdown (`KmsBackendV2::disable_output`)

`disable_output` currently drains the SubmittedOp queue via `engine.drain_all`, but the engine's drain only walks `submitted` — it does **not** flush an open `pending_cow_batch` / `pending_render_batch` that has been appended to but not yet submitted. A pending-events entry whose ticket names such an un-flushed batch would never signal during drain. The shutdown sequence must explicitly flush pending batches before walking submitted.

Order inside `disable_output`:
1. **`self.engine.flush_cow_batch(&mut self.store, &mut self.platform)`** — new; promotes any open cow batch into a SubmittedOp so `drain_all` will wait on it.
2. **`self.engine.flush_render_batch(&mut self.store, &mut self.platform)`** — new; same for the render batch.
3. `self.engine.drain_all(&self.platform)` — waits on each SubmittedOp ticket in order. After this returns, every cow_batch ticket in the pending-events queue has either signaled or the renderer is failed.
4. `self.sync_descriptor_pool_telemetry()` — existing.
5. `self.scene.drain_all(&mut self.platform)` — existing.
6. **`self.drain_pending_present_events_for_shutdown()`** — new; drain the queue in full regardless of remaining ticket signal state (post-drain_all they should all be signaled; defensive against a runaway). Returns the list to the caller.
7. `self.telemetry.flush_submit_trace()` — existing.
8. `self.platform.disable_output()` — existing.

Step 6 returns the entries to the caller (`lib.rs::run`), which emits them to clients before tearing down the socket. The emission has to happen before `fs::remove_file(&socket_path)` so the events make it onto the wire. Step 6 also closes any sync_file FDs still owned by the queue entries.

**Test gate**: `disable_output_flushes_pending_batches_before_drain_all` — open a cow_batch via `engine.cow_copy_area`, enqueue a pending PRESENT entry, call `disable_output`. Assert: no pending batches remain after step 2; the entry's ticket is signaled after step 3; the queue is empty after step 6.

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

### Integration (engine retirement + wake source)

`crates/yserver/tests/v2_acceptance.rs`:

- **`v2_present_pixmap_enqueues_pending_and_defers_emission`** — drive the v2 backend's `Backend::copy_area` + `enqueue_present_completion` with `PresentWake::Pixmap { idle_fence_xid: 0 }`, assert (a) no synchronous wait was made (latency on the entry call < 100 µs), (b) the entry sits in the queue, (c) after a forced maybe_composite tick + waiting on retirement, drain returns the entry.
- **`v2_present_pixmap_synced_enqueues_with_release_syncobj_wake`** — same shape but using `PresentWake::PixmapSynced`. Asserts the wake variant survives enqueue → drain unchanged; emission path is exercised in a separate unit test of `fire_present_completion_events`.
- **`v2_present_pixmap_drain_returns_in_fifo_order_against_live_vk`** — Vk-backed: issue 3 PRESENT-shaped sequences (copy_area + enqueue) targeting the same COW under lavapipe; flush + retire all; drain returns 3 entries in submission order.
- **`v2_present_aggregation_depth_unchanged_after_design`** — issue 10 PRESENT-shaped sequences without any intervening retirement, assert the cow_batch flushes ONCE (depth 10) — i.e. the deferred-completion design doesn't accidentally re-introduce a flush via the enqueue path.
- **`v2_present_completion_inner_epfd_exposed_via_poll_fds`** — at backend init, before any enqueue, `Backend::poll_fds()` already reports a `BackendFdKind::PresentCompletion` FD. The FD is stable across the backend's lifetime (same raw FD value across all enqueue/drain cycles).
- **`v2_present_completion_fence_fd_aggregates_and_signals`** — Vk-backed: enqueue two entries, assert the inner epoll FD is NOT readable; force one of the two batches to retire; the inner epoll FD becomes readable (poll with `POLLIN`, zero-timeout); drain returns the signaled entry and closes its per-entry FD; the other entry remains pending and its FD remains registered.
- **`v2_present_completion_already_signaled_fence_writes_wakeup_eventfd`** — pre-signal the cow_batch ticket before enqueue (test helper); enqueue an entry; `vkGetFenceFdKHR` returns `-1`; assert (a) no per-entry FD is registered in the inner epoll, (b) `wakeup_eventfd` has been written to (read with non-blocking `read`, receive a non-zero value), (c) the inner epoll FD is now readable (poll with `POLLIN`, zero-timeout), (d) the next drain returns the entry and the eventfd is cleared.
- **`v2_present_synced_pin_survives_free_syncobj`** — enqueue a PresentWake::PixmapSynced entry; immediately call the backend's syncobj-destroy path (simulating client FREE_SYNCOBJ); force the cow_batch to retire; drain dispatches to `dri3_signal_syncobj` via the pinned `Arc<vk::Semaphore>` and succeeds (the pin kept the semaphore alive past the resource destruction).
- **`v2_present_pixmap_pin_survives_destroy_xshmfence`** — same shape with `PresentWake::Pixmap` and `XshmfenceSegment` destroy.
- **`v2_disable_output_flushes_pending_batches_before_drain_all`** — open a cow_batch, enqueue a pending PRESENT entry against it; call `disable_output`; assert (a) no pending batches in the engine after the explicit flush step, (b) the entry's ticket is signaled after `drain_all`, (c) the queue is empty after the shutdown drain, (d) no leaked per-entry FDs (count FDs before/after).

### Regression (the bee use-after-free stays closed)

- **`v2_present_pixmap_then_free_pixmap_keeps_source_alive`** — issue a copy_area + enqueue + immediate FreePixmap on the source; the eager-touched ticket on the source pixmap defers destruction; force a CB flush; the CB still sees a valid VkImageView (no crash, no validation warning). The existing `8ca552a` regression tests stay valid:
  - `render_composite_open_marks_src_last_render_ticket_immediately`
  - `render_composite_open_then_decref_src_returns_pending_fence`
  - `cow_copy_area_open_marks_src_last_render_ticket_immediately`

### Hardware acceptance (user-driven smoke)

- **bee** (Ryzen 9 6900HX / RDNA2 / RADV): MATE drag must not wedge. The original use-after-free symptom (`ERROR_DEVICE_LOST` flood, addr_binding_report fault) must not reproduce. The lag-floor of the current bee fix (synchronous PRESENT wait stacking marco frame work on the request handler) should be substantially better.
- **yoga** (Snapdragon X1 / Adreno X1 / Turnip): MATE drag should match the user-perceived "snappy, low CPU, low Hz" state of the post-revert measurement. Telemetry: `cow_batches_flushed/s` average depth in the 5-10 range (silence reference 5.41, yoga reverted 8.5).
- **silence** (i9 13900k / rx580 / RADV): MATE drag perf characteristic unchanged from the existing post-Task-3 baseline. The deferred completion shouldn't change anything for silence (it was never on the flush-on-PRESENT path's binding side).
- **fuji** (i5-7200U / Intel HD 620 / ANV): sanity check that the design doesn't introduce a new failure on Intel.

### Smoke gate for regression

`xshmfence` triggers — the most subtle thing to break — are gated by Mesa WSI clients (xterm, xeyes, anything using DRI3 with explicit-sync). A drag of any GTK app where Mesa's vkAcquireNextImage is on the critical path will hang within 1-2 frames if the trigger doesn't fire. Smoke matrix already includes Mesa-WSI workloads.

## Risks

**R1. Both PRESENT paths covered.** `Pixmap` (xshmfence wake) and `PixmapSynced` (DRM syncobj wake) both flow through the same `enqueue_present_completion` + drain mechanism via the `PresentWake` enum. After this design lands, `wait_for_drawable_idle` has no callers in tree. The trait method is retained for forward compatibility but logically dead until a new use case appears.

**R2. `VK_KHR_external_fence_fd` availability.** The design requires the extension to export the cow_batch fence as a sync_file FD. RADV, Turnip, ANV, and Mesa's lavapipe all support it; the v2 `VkContext` already enables semaphore-FD export for the scanout path. The fence-FD variant is part of the same Vulkan capability set and is reasonable to require. Mitigation: `VkContext::new` verifies the extension at init time and errors out cleanly if absent. Fallback: `Backend::next_wakeup()` polling-based wake (1 ms bound) is a documented degraded-mode option for future ports.

**R3. Event ordering vs. wake-up race.** Mesa WSI threads wake on the xshmfence trigger or the DRM syncobj signal; they're racing the X11 event dispatcher (separate thread / fd). Per spec the wake object is the load-bearing trigger; the X11 events are audit signals. Fine to race. Both fire from the same drain-loop iteration. **Emission order within the drain loop is `IdleNotify` then `CompleteNotify`** matching the existing `process_request.rs:5513` site and Mesa expectation.

**R4. Test coverage of the bee scenario without bee hardware.** The eager-touch unit tests + the existing `8ca552a` regression suite gate the use-after-free closure logically. The fault would only reproduce on RDNA2 iGPU under heavy churn (per the platform analysis in the original commit message). The acceptance gate is logical — if the eager-touch path is exercised correctly, the use-after-free is closed on every platform that runs it.

**R5. Telemetry counters may show transient queue-depth spikes during warm-up.** Marco's startup issues a burst of PRESENTs before the first flush; queue depth could briefly exceed expected steady-state. Not a correctness issue; flag in the per-second emitter logs only as a record of warm-up shape.

**R6. `Backend` trait surface grows.** Two new methods + one new `BackendFdKind` variant + one new public type (`CompletedPresentEvent` + `PresentWake`). The v1 backend (still in tree per Stage 5 v1-deletion gates) needs stub implementations — return empty for `drain_completed_present_events`; no-op for `enqueue_present_completion`; no new FDs from `poll_fds()`. v1 still calls the synchronous wait at the same code path because v1 has no Task 3 batching (every paint submits its own CB; tickets always correspond to submitted work; no deadlock). v1 unchanged.

**R7. FD leak on abnormal exit.** Each pending entry owns a sync_file FD (`OwnedFd`). `OwnedFd::Drop` closes the FD on every unwind path, including the entry's own panic-driven destructor. The inner `present_completion_epfd` is itself an `OwnedFd` owned by `KmsBackendV2`, dropped on backend Drop. The kernel closes any remaining FDs at process exit regardless. No new leak surface vs the existing scanout-side sync_file usage.

**R8. Underlying primitives may not be `Arc`-able yet.** `PinnedWake::Pixmap(Arc<XshmfenceSegment>)` and `PinnedWake::PixmapSynced(Arc<vk::Semaphore>)` assume the backend's xshmfence + syncobj registries store handles in a way that can produce an `Arc` clone at enqueue time. If today they store raw `ash::vk::Semaphore` handles + manage destruction by xid lookup, the implementation needs to wrap them in `Arc<T>` first (one allocation per syncobj/fence creation; small). Plan-time work, not spec-time. Documented as part of the implementation contract.

## Implementation prerequisites

The design depends on three pieces of foundation work that don't exist in the codebase at spec-write time. The implementation plan executes these first (in order), then layers the deferred-completion machinery on top. None of these are externally visible behaviour changes by themselves.

1. **`BackendFdKind::PresentCompletion` variant** on the `BackendFdKind` enum at `crates/yserver-core/src/backend/trait_def.rs:31-41`. Outer-loop dispatch at `crates/yserver-core/src/core_loop/run.rs:328-335` (the existing `Libinput / Drm / HostX11` arm match) gets a fourth arm that routes readiness to a new `drain_completed_present_events` call. Both `KmsBackend` (v1) and `KmsBackendV2` stub `enqueue_present_completion` as a no-op + `drain_completed_present_events` as `Vec::new()`; v1 has no batching so the deferred-completion path is logically irrelevant for it.

2. **`Arc`-wrap the xshmfence + syncobj registries.** Today both backends store these as raw handles indexed by `u32` xid:
   - v1: `KmsBackend.dri3_xshmfences` and `KmsBackend.dri3_sync_resources` at `crates/yserver/src/kms/backend.rs:707-720`.
   - v2: equivalent fields on `KmsBackendV2` around `crates/yserver/src/kms/v2/backend.rs:219-225`; `dri3_trigger_fence` / `dri3_signal_syncobj` impls at `:8748-8833`.

   Wrap the value types in `Arc<XshmfenceSegment>` / `Arc<vk::Semaphore>` (or whatever the underlying primitive struct is). `dri3_trigger_fence` / `dri3_signal_syncobj` keep their existing xid-lookup signatures for the legacy synchronous call sites. New API: `Backend::dri3_xshmfence_handle(xid) -> Option<Arc<XshmfenceSegment>>` and `Backend::dri3_syncobj_handle(xid) -> Option<Arc<vk::Semaphore>>` for the enqueue path to grab a pinned clone. `FreeSyncobj` / `XFixesDestroyFence` drop the registry entry; outstanding `Arc` clones keep the primitive alive until the deferred drain releases them.

3. **`Backend::enqueue_present_completion` + `Backend::drain_completed_present_events`** trait methods. Default impls in the trait can be no-op + empty so the v1 path doesn't need to think about them; v2 overrides with the real implementation.

Once these three are in place, the deferred-completion design above lights up incrementally: backend internal state, then PRESENT handler replacements, then test gates. The plan sequences this so each commit leaves the tree green.

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

Plus: bee MATE drag must not wedge under the same workload that triggered the original use-after-free. The synchronous-wait lag floor on bee should drop substantially.

Capture via `just yserver-mate-hw-telemetry`. SSH from another machine + `pkill -TERM yserver` for clean exit (the kernel SysRq rebuild on yoga is in flight; don't zap until that lands).
