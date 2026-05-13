# Phase 4 — sync rework: retire close-time `vkQueueWaitIdle` from `PaintBatch`

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `PaintBatch::submit_and_wait`'s `vkQueueWaitIdle` with a real `VkFence`. T1 narrows the wait scope to a single fence (still blocking). T2–T3 add async retirement so best-effort flushes (`VisibleComposite`, `SizeLimit`, `LatencyLimit`, `Shutdown`) return immediately and the batch retires later when its fence signals on the composite poll. Strict flushes (`Readback`, `ExternalSync`, `ProtocolBarrier`) keep their synchronous wait contract via `vkWaitForFences` on the specific batch fence.

**Architecture:** Three structural changes on top of the 3F-2 baseline:

1. **`PaintBatch` owns a `vk::Fence`.** Created lazily on first submit, destroyed at retirement (or leaked at the device-lost path that the existing path-2 documentation already mandates). Replaces `vk::Fence::null()` passed to `queue_submit2` today.

2. **`RenderScheduler` gains a submitted-batch queue.** `submitted_paint_batches: VecDeque<PaintBatch>` — analogous to the existing `InFlight` queue for output frames. Best-effort `close_and_submit` moves the current batch into this queue without waiting; a poll method drains retired batches.

3. **`flush_if_needed` branches by reason.** Strict reasons take the blocking path: submit + `wait_for_fences` on the returned fence. Best-effort reasons take the async path: submit + queue for poll. The strict/best-effort split is already declared in `BatchFlushReason` (3A-era infrastructure); this phase finally uses it for what it was designed for.

**Tech Stack:** Rust, ash (Vulkan), existing 3A–3F infrastructure (`PaintBatch`, `RenderScheduler`, `InFlight`, `BatchFlushReason`).

---

## Prerequisite — confirm post-3F-2 baseline

Before T1, verify the tree state:

```bash
cd /home/jos/Projects/yserver
git log --oneline graphics-followups | head -20
rg -n 'run_one_shot_op\(' crates/yserver/src/kms/backend.rs | wc -l
rg -n 'queue_wait_idle' crates/yserver/src/kms/
```

Expected:
- 3F-2 commits landed up through `31bb0bb` (status flip) at minimum. Branch tip at least at `50f1dc6` (hardware smoke results).
- `run_one_shot_op(` returns ~7 sites in `backend.rs` (readback handlers, dump_scanout_one, open_with_commit, run_legacy_paint_op body). NONE inside `try_vk_render_composite` or `try_vk_render_traps_or_tris`.
- `queue_wait_idle` appears in: `paint_batch.rs::submit_and_wait` (the Phase 4 target), all `Drop` impls in `vk/*.rs` (backend teardown — not in hot path), `mask_scratch.rs::ensure_image_size`, `dst_readback.rs::ensure`, `copy_scratch.rs::ensure_size` (grow paths — pre-flush gated since 3D/3F-1/3F-2), `run_one_shot_op` body. The Phase 4 scope is **only** the `submit_and_wait` site.
- `cargo test --workspace`, `cargo clippy -p yserver`, `cargo +nightly fmt --check` all green.

If any of the above don't hold, STOP.

## Phase context

Read `docs/superpowers/specs/2026-05-12-rendering-rearchitecture.md` and `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase3f-2-results.md` for the predecessor state. Critically, the post-3F-2 hardware-smoke results (the adapta-nokto + mate-cc reproducer) showed that `submit_and_wait`'s wait is **0.09% children in the perf profile** — Phase 4 will NOT close the adapta-nokto + mate-cc lag on `bee`. Phase 4 is justified independently:

- **Wait scope narrowing**: `vkQueueWaitIdle(queue)` waits for *all* submissions to the graphics queue, including composite-side submissions. `vkWaitForFences([fence], …)` waits only for our specific submission. This narrows the wait without changing semantics for our submission, and avoids serializing on unrelated composite work.

- **Input-loop fluidity**: best-effort flushes (`VisibleComposite` at composite-and-flip; `Shutdown`) currently block the single-threaded core loop on a wait that doesn't need to block — there's no completion guarantee anyone is reading. Removing the wait for those reasons lets the loop service input events while the GPU drains.

- **Foundation for Phase 5**: the per-glyph one-shot `queue_wait_idle` in `GlyphAtlas::intern` is a separate retire-this-path-too target. After Phase 4 lands the fence + poll-retirement infrastructure, Phase 5 can plug `GlyphAtlas::intern` into the same retirement queue rather than inventing its own fence machinery.

### Strict vs best-effort flush reasons (already split in `BatchFlushReason`)

From `paint_batch.rs:57-92`:

| Reason | Strict / Best-effort | Why |
|---|---|---|
| `Readback` | strict | Synchronous-reply request needs CPU-visible pixels (GetImage, MIT-SHM GetImage). |
| `ExternalSync` | strict | External sync export pending (DRI3 Present fence handoff, SYNC ext fence trigger). |
| `ProtocolBarrier` | strict | Explicit protocol barrier requested it. Drawable destruction, gradient create, pre-resize-flush for scratch grow. |
| `VisibleComposite` | best-effort | Top of `composite_and_flip`'s per-output loop. After Phase 4 it returns immediately; composite-side mirror sampling is ordered by same-queue submission order + paint recorders' ending barriers to `SHADER_READ_ONLY_OPTIMAL` (see Pre-task note 5 for the spec rule). |
| `SizeLimit` | best-effort | Batch hit a size limit. Phase 4+ uses this if we start enforcing one. |
| `LatencyLimit` | best-effort | Batch hit a latency limit. Same. |
| `Shutdown` | best-effort | Server shutdown / hot teardown. Cleanly waits at the end. |

After Phase 4, strict reasons go through `submit_and_wait` (or its renamed equivalent — see T2) and block on `vkWaitForFences`. Best-effort reasons go through `submit_async` and push to the submitted-batch queue.

**`VisibleComposite` going async is the load-bearing best-effort case.** Composite samples drawable mirrors that prior paint ops wrote. Today the close-time `queue_wait_idle` makes the sample safe by being broad. After Phase 4 the safety comes from a narrower mechanism: each paint recorder ends its CB with an explicit image-layout-and-access barrier transitioning the mirror to `SHADER_READ_ONLY_OPTIMAL`, and composite's CB is submitted later on the same graphics queue. Per the Vulkan spec (`vkCmdPipelineBarrier2` + same-queue submission ordering), that combination is the correct memory dependency for "paint writes happen-before composite reads" — no `queue_wait_idle`, no semaphore handoff, no holder wiring needed. Pre-task note 5 has the full framing.

### Key invariants Phase 4 inherits

1. **Drop-order**: `KmsBackend.scheduler` before `KmsBackend.ops_command_pool`. Don't reorder.
2. **`renderer_failed` gate**: every paint entry still goes through `paint_resources()`.
3. **Path-2 (device-lost) semantics**: `paint_batch.rs::submit_and_wait` documents the existing leak-rather-than-UB behavior on wait failure. Phase 4 preserves it for `wait_for_fences`. New equivalent: if `wait_for_fences` returns Err, the CB is in flight and resources are abandoned (state stays `Submitted` forever; `Drop` no-ops; `renderer_failed = true` at the caller).
4. **CPU-side layout tracking at record time** (3F-2 #8): unchanged. A poisoned batch's resources are released synchronously the same way; an async-retired batch's resources release when the fence signals (which only happens for batches that successfully submitted). No new race.
5. **Single-threaded core loop**: still single-threaded. Fence polling happens on the same thread that records and submits.

### Out of scope (deferred)

- `run_one_shot_op`'s `vkQueueWaitIdle` (Phase 5).
- `GlyphAtlas::intern` per-glyph wait (Phase 5).
- `record_get_image` readback-handler waits (Phase 5).
- AMD-specific investigation (separate phase; profile data shows submit_and_wait isn't the bee/RDNA2 bottleneck).
- Timeline semaphores instead of binary fences (rejected: binary `VkFence` is sufficient for the close→retire model; no cross-frame dependency that needs a timeline value).
- Batched fence destruction / fence pool (Phase 6 — refcounted-handles territory).

## File structure

| File | Role | Touched in |
|---|---|---|
| `crates/yserver/src/kms/scheduler/paint_batch.rs` | Add `fence: Option<vk::Fence>` field. `submit_and_wait` swaps `vkQueueWaitIdle` for fence-creation + `wait_for_fences`. Add `submit_async` (returns the fence handle). Add `try_retire_if_signaled` (non-blocking status check; retires if signaled). Update `Drop` for `Submitted` to leak fence too. Update path-2 doc. | T1 + T2 |
| `crates/yserver/src/kms/scheduler/mod.rs` | Add `submitted_paint_batches: VecDeque<PaintBatch>`. Add `RenderScheduler::submit_async`, `RenderScheduler::poll_retired_paint_batches`, `RenderScheduler::pending_paint_batches`. | T2 + T3 |
| `crates/yserver/src/kms/backend.rs` | `flush_if_needed` branches strict vs best-effort. Call `poll_retired_paint_batches` from `poll_in_flight` (or alongside). Add bounded-queue backpressure check. Drain `submitted_paint_batches` after `vkDeviceWaitIdle()` in the shutdown path. | T3 + T4 + T5 |
| `crates/yserver/src/kms/scheduler/mod.rs` (drain helper) | Add `drain_submitted_paint_batches`. | T5 |
| `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase4-results.md` | Results doc. | T6 |

## Pre-task notes (read before starting)

1. **The fence is owned by the `PaintBatch`.** `vkCreateFence` happens lazily on first submit (T1 keeps a single fence per batch; create at the top of `submit_and_wait`). `vkDestroyFence` happens at `retire_now()` (after the fence has been waited on / observed signaled). On the device-lost path-2 the fence is leaked alongside the CB and resources — same handle-abandonment rule.

2. **`wait_for_fences` and `get_fence_status` semantics.** `vkWaitForFences([fence], wait_all=true, UINT64_MAX)` blocks until signaled. `vkGetFenceStatus(fence)` returns `vk::Result::SUCCESS` if signaled, `vk::Result::NOT_READY` if not, or an error. The poll path uses `get_fence_status`; the strict path uses `wait_for_fences`.

3. **`submitted_paint_batches` ordering**: `VecDeque<PaintBatch>` (FIFO). `submit_async` pushes back. `poll_retired_paint_batches` walks the front, retiring signaled batches. Retirement is strict-prefix-FIFO: stop at the first non-signaled batch (resource lifetimes are layered on submission order, same convention as `InFlight::drain_retired`).

4. **Backpressure**: if `submitted_paint_batches.len()` exceeds a small bound (proposal: `MAX_IN_FLIGHT_PAINT_BATCHES = 4` — about one composite period at 60 Hz), block on the oldest fence before pushing the new one. This prevents unbounded growth under chatty clients (the listener-starvation pattern from `docs/known-issues.md` is the worst-case driver).

5. **`VisibleComposite` was the only best-effort caller blocking on `queue_wait_idle` historically.** After Phase 4 it goes async — `flush_if_needed(VisibleComposite)` returns immediately and the paint batch retires on the composite-tick poll.

   **Why no semaphore handoff to composite is needed in this phase**: per the Vulkan spec (see [Fundamentals](https://docs.vulkan.org/spec/latest/chapters/fundamentals.html) and [Synchronization](https://docs.vulkan.org/spec/latest/chapters/synchronization.html)), same-queue submission order is the **ordering basis** for cross-submission dependencies, but it does NOT by itself guarantee execution ordering or memory visibility. Cross-submission memory visibility requires `vkCmdPipelineBarrier2` (image/memory barriers) or explicit semaphore signal/wait.

   What makes Phase 4 safe **without** adding a semaphore: each paint recorder already ends its CB with an explicit image-layout-and-access barrier transitioning the mirror to `SHADER_READ_ONLY_OPTIMAL` (see `vk/ops/fill.rs:130`, `vk/ops/render.rs:71`, `vk/ops/image.rs:15`, plus the trap/text/composite paths). Composite's CB is then submitted **later** on the **same** graphics queue (paint at `backend.rs:~1586`, composite at `backend.rs:~7088` / `vk/compositor.rs:~195`). The combination — same-queue submission order PLUS explicit per-CB ending barriers on the mirror image — is the proper Vulkan memory dependency for "paint writes happen-before composite reads."

   This is the precise framing. Drop the older "single-queue serialization guarantees visibility" shorthand; it's true in practice on all current implementations but is not the actual spec rule and would mask a real bug if a future paint recorder forgot its ending barrier.

   **`PaintBatch::fence` is for CPU-side retirement only.** No GPU-side semaphore handoff. The host-side fence keeps the paint batch's resources alive on the CPU until the GPU has signaled completion; composite's GPU-side reads are ordered by the per-CB barriers, not by the fence.

   **`OutputFrame::holders` / `PaintBatch::holders` stay at 0 in Phase 4.** Phase 3A landed `acquire_holder` / `release_holder` for shape but Phase 4 does NOT wire them — the same-queue + per-CB-barrier story above is sufficient on its own. Wiring `holders` for cross-batch reference-counted retirement is Phase 6 (batch-owned refcounted handles) territory; it'd be needed if/when we share a paint batch's images across multiple subsequent composite cycles in flight, which Phase 4 does not introduce.

6. **The path-2 (device-lost) wait-failure path stays.** Today: `queue_wait_idle` failure leaks CB + resources + (with Phase 4) fence. Same behavior, narrower trigger (now `wait_for_fences` on the specific fence rather than the broader idle wait).

7. **Test coverage**: state-machine tests in `paint_batch.rs` extend (fence-allocated-and-destroyed transitions). Recorder tests unaffected. Coverage for the new async behavior is hardware smoke (T6) — yserver in a normal MATE session, plus rendercheck to confirm no regressions.

8. **clippy / fmt**: plain `cargo clippy -p yserver`, `cargo +nightly fmt`. 5 pre-existing `doc_lazy_continuation` warnings; no new ones.

---

## Task 1: Narrow `submit_and_wait`'s wait — `vkWaitForFences` instead of `vkQueueWaitIdle`

**Goal:** Replace the all-queue-idle wait with a single-fence wait. Behavior-equivalent for our submission (still blocking) but no longer serializes on unrelated composite-side work. **This task alone closes the headline Phase 4 scope** — the `vkQueueWaitIdle` site documented in `docs/status.md` Phase 4 entry. T2–T4 add the async-retirement story on top.

**Files:**
- Modify: `crates/yserver/src/kms/scheduler/paint_batch.rs`

### Step 1: Add `fence` field

- [ ] **Step 1: Add `fence: Option<vk::Fence>` field to `PaintBatch`**

In `paint_batch.rs`, modify the struct definition:

```rust
pub struct PaintBatch {
    pub frame_id: u64,
    pub dirty_outputs: Vec<usize>,
    pub state: BatchState,
    pub holders: u32,
    cb: Option<vk::CommandBuffer>,
    pool: vk::CommandPool,
    vk: Arc<VkContext>,
    retire_resources: Vec<Box<dyn BatchResource>>,
    upload_arena: Option<BatchUploadArena>,
    descriptor_arena: Option<BatchDescriptorArena>,
    /// Allocated on first submit. Signaled when the submitted CB
    /// completes on the GPU. Destroyed in `retire_now()` after the
    /// fence has been observed signaled (or waited on). On the
    /// path-2 device-lost path, leaked alongside the CB.
    fence: Option<vk::Fence>,
}
```

Update `PaintBatch::new`:

```rust
pub fn new(frame_id: u64, vk: Arc<VkContext>, pool: vk::CommandPool) -> Self {
    Self {
        frame_id,
        dirty_outputs: Vec::new(),
        state: BatchState::Idle,
        holders: 0,
        cb: None,
        pool,
        vk,
        retire_resources: Vec::new(),
        upload_arena: None,
        descriptor_arena: None,
        fence: None,
    }
}
```

### Step 2: Use `wait_for_fences` in `submit_and_wait`

- [ ] **Step 2: Edit `submit_and_wait` body**

Find the existing `submit_and_wait` (lines ~383–442). Three changes:

A. Create the fence before `queue_submit2`. Pass it instead of `vk::Fence::null()`.
B. Replace `queue_wait_idle` with `wait_for_fences([fence], wait_all=true, UINT64_MAX)`.
C. Destroy the fence in `retire_now` (the success-and-already-Submitted path).

Replace:

```rust
        let cb = self.cb.expect("Closed implies cb was allocated");
        let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let submit = [vk::SubmitInfo2::default().command_buffer_infos(&cb_info)];

        // Path 1: submit fails. CB never queued; safe to free + retire.
        if let Err(e) = unsafe {
            self.vk
                .device
                .queue_submit2(self.vk.graphics_queue, &submit, vk::Fence::null())
        } {
            unsafe { self.vk.device.free_command_buffers(self.pool, &[cb]) };
            self.cb = None;
            self.state = BatchState::Closed; // back to a poisonable state
            self.poison();
            return Err(BatchError::Vk(e));
        }

        // Now Submitted — CB is in flight.
        self.state = BatchState::Submitted;

        // Path 2 / 3: wait. On wait failure the CB and our resources
        // may still be referenced by the GPU. Leak rather than UB.
        match unsafe { self.vk.device.queue_wait_idle(self.vk.graphics_queue) } {
            Ok(()) => {
                // Path 3: clean retirement.
                unsafe { self.vk.device.free_command_buffers(self.pool, &[cb]) };
                self.cb = None;
                self.retire_now();
                Ok(())
            }
            Err(e) => {
                // Path 2: device-lost or similar. Intentionally do
                // NOT free the CB, do NOT release retire_resources
                // — those handles are abandoned. The batch stays
                // in `Submitted` forever; its `Drop` does nothing.
                //
                // Upper layers MUST treat this as a fatal
                // KMS-renderer condition (see method doc above).
                log::error!(
                    "PaintBatch::submit_and_wait: queue_wait_idle failed ({e:?}); \
                     CB and resources abandoned. KMS renderer is in an \
                     unrecoverable state — caller MUST tear down or disable."
                );
                Err(BatchError::Vk(e))
            }
        }
    }
```

With:

```rust
        let cb = self.cb.expect("Closed implies cb was allocated");

        // 4-T1: create a fence and use it as the submit's signal.
        // Then wait_for_fences on it instead of the broad
        // queue_wait_idle. This narrows the wait to OUR submission
        // — composite-side submissions on the same queue no
        // longer serialize the paint-side wait.
        let fence_info = vk::FenceCreateInfo::default();
        let fence = match unsafe { self.vk.device.create_fence(&fence_info, None) } {
            Ok(f) => f,
            Err(e) => {
                // Fence creation failed before any submit happened.
                // CB hasn't queued; safe to free + retire.
                unsafe { self.vk.device.free_command_buffers(self.pool, &[cb]) };
                self.cb = None;
                self.state = BatchState::Closed;
                self.poison();
                return Err(BatchError::Vk(e));
            }
        };
        self.fence = Some(fence);

        let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let submit = [vk::SubmitInfo2::default().command_buffer_infos(&cb_info)];

        // Path 1: submit fails. CB never queued; safe to free + retire.
        if let Err(e) = unsafe {
            self.vk
                .device
                .queue_submit2(self.vk.graphics_queue, &submit, fence)
        } {
            unsafe {
                self.vk.device.destroy_fence(fence, None);
                self.vk.device.free_command_buffers(self.pool, &[cb]);
            }
            self.cb = None;
            self.fence = None;
            self.state = BatchState::Closed; // back to a poisonable state
            self.poison();
            return Err(BatchError::Vk(e));
        }

        // Now Submitted — CB is in flight.
        self.state = BatchState::Submitted;

        // Path 2 / 3: wait. On wait failure the CB and our resources
        // may still be referenced by the GPU. Leak rather than UB.
        let fences = [fence];
        match unsafe { self.vk.device.wait_for_fences(&fences, true, u64::MAX) } {
            Ok(()) => {
                // Path 3: clean retirement. retire_now destroys the
                // fence alongside the CB and arenas.
                unsafe { self.vk.device.free_command_buffers(self.pool, &[cb]) };
                self.cb = None;
                self.retire_now();
                Ok(())
            }
            Err(e) => {
                // Path 2: device-lost or similar. Intentionally do
                // NOT free the CB, do NOT destroy the fence, do NOT
                // release retire_resources — those handles are
                // abandoned. The batch stays in `Submitted` forever;
                // its `Drop` does nothing.
                //
                // Upper layers MUST treat this as a fatal
                // KMS-renderer condition (see method doc above).
                log::error!(
                    "PaintBatch::submit_and_wait: wait_for_fences failed ({e:?}); \
                     CB / fence / resources abandoned. KMS renderer is in an \
                     unrecoverable state — caller MUST tear down or disable."
                );
                Err(BatchError::Vk(e))
            }
        }
    }
```

### Step 3: Destroy fence in `retire_now`

- [ ] **Step 3: Edit `retire_now`**

Add fence destruction to the resource-release block. Find:

```rust
    fn retire_now(&mut self) {
        debug_assert!(
            matches!(self.state, BatchState::Closed | BatchState::Submitted),
            "retire_now from {:?}",
            self.state
        );
        if let Some(arena) = self.upload_arena.take() {
            Box::new(arena).release(&self.vk);
        }
        if let Some(arena) = self.descriptor_arena.take() {
            Box::new(arena).release(&self.vk);
        }
        for r in self.retire_resources.drain(..) {
            r.release(&self.vk);
        }
        self.state = BatchState::Retired;
    }
```

Change to:

```rust
    fn retire_now(&mut self) {
        debug_assert!(
            matches!(self.state, BatchState::Closed | BatchState::Submitted),
            "retire_now from {:?}",
            self.state
        );
        if let Some(fence) = self.fence.take() {
            unsafe { self.vk.device.destroy_fence(fence, None) };
        }
        if let Some(arena) = self.upload_arena.take() {
            Box::new(arena).release(&self.vk);
        }
        if let Some(arena) = self.descriptor_arena.take() {
            Box::new(arena).release(&self.vk);
        }
        for r in self.retire_resources.drain(..) {
            r.release(&self.vk);
        }
        self.state = BatchState::Retired;
    }
```

### Step 4: Update path-2 doc comment to mention the fence

- [ ] **Step 4: Edit the doc comment on `submit_and_wait`**

The existing doc (~lines 348–382) talks about queue_wait_idle. Update to reflect the fence:

Replace lines starting with `/// Submit + idle-wait + retire.` through `/// 3. **Both succeed**: free CB, retire resources, return Ok.` with:

```rust
    /// Submit + fence-wait + retire. Phase 3 collapses
    /// Closed→Submitted→Retired into this one call. Phase 4
    /// narrows the wait from `queue_wait_idle` (all queue) to
    /// `wait_for_fences` on this batch's own fence — composite-
    /// side submissions on the same queue no longer serialize
    /// the paint-side wait. Phase 4 T2+ further splits this into
    /// `submit_async` + `try_retire_if_signaled` for best-effort
    /// flushes; the strict-flush path keeps using this blocking
    /// entry.
    ///
    /// On Idle (no CB allocated): no submit; transitions directly
    /// to Retired. On Poisoned: returns `BatchError::Poisoned`
    /// without touching the queue.
    ///
    /// **Four distinct failure paths**, with different retirement
    /// semantics — DO NOT collapse them:
    ///
    /// 1a. **Fence creation fails** (`create_fence` before
    ///     submit): the CB never entered the queue and no fence
    ///     was created. Free the CB, retire resources, return
    ///     the error. Mechanically identical to 1b but listed
    ///     separately so reviewers don't have to infer it.
    /// 1b. **Submit fails** (`queue_submit2` returns Err): the
    ///     CB never entered the queue. Destroy the fence
    ///     (created in 1a's step but not yet given to the queue),
    ///     free the CB, retire resources, return the error.
    /// 2.  **Wait fails** (`queue_submit2` Ok, `wait_for_fences`
    ///     Err): the CB IS in flight or the device is lost. The
    ///     GPU may still be reading our resources. We must NOT
    ///     free the CB, must NOT destroy the fence, and must NOT
    ///     call `BatchResource::release` — those Vulkan handles
    ///     are abandoned until device destruction. The batch
    ///     stays in `Submitted` forever (its `Drop` honours the
    ///     same leak; see `Drop` impl).
    ///
    ///    **This is not a recoverable state.** Callers that get
    ///    `BatchError::Vk` from `submit_and_wait` MUST treat the
    ///    KMS renderer as failed: tear the backend down (which
    ///    triggers `VkContext::Drop` → global `device_wait_idle`
    ///    if the device is still responsive, otherwise driver
    ///    cleanup at process exit) or mark it permanently
    ///    disabled. Continuing to call `record_paint_op` /
    ///    `flush_if_needed` after a leaked Submitted batch is
    ///    not a supported steady state — it produces more
    ///    abandoned CBs each cycle.
    ///
    /// 3.  **Both succeed**: free CB, retire resources
    ///     (including fence destroy), return Ok.
```

### Step 5: Update `Drop` for `Submitted` state

- [ ] **Step 5: Edit the `Drop` impl**

The existing `Drop` for `BatchState::Submitted` logs and leaks. No code change there — the existing path is already correct (the leak now implicitly includes the fence handle since we never destroy it). But update the log message to mention the fence:

Find:

```rust
            BatchState::Submitted => {
                log::error!(
                    "PaintBatch::drop while Submitted — abandoned resources \
                     (CB + arenas + descriptor pools). KMS renderer is in an \
                     unrecoverable state."
                );
            }
```

Change to:

```rust
            BatchState::Submitted => {
                log::error!(
                    "PaintBatch::drop while Submitted — abandoned resources \
                     (CB + fence + arenas + descriptor pools). KMS renderer \
                     is in an unrecoverable state."
                );
            }
```

### Step 6: Build

- [ ] **Step 6: `cargo check -p yserver`**

Expected: clean.

### Step 7: Tests + fmt + clippy

- [ ] **Step 7: `cargo test -p yserver --lib 2>&1 | tail -5`**

Expected: 138 passed, 0 failed, 3 ignored (state-machine unit tests don't exercise Vulkan; the fence change is transparent at that level).

- [ ] **Step 8: `cargo +nightly fmt --check`**

Expected: no diff.

- [ ] **Step 9: `cargo clippy -p yserver 2>&1 | tail -10`**

Expected: 5 pre-existing warnings, no new ones.

### Step 8: Commit T1

- [ ] **Step 10: Commit**

```bash
git add crates/yserver/src/kms/scheduler/paint_batch.rs
git commit -m "$(cat <<'EOF'
refactor(kms): replace queue_wait_idle with per-batch VkFence in submit_and_wait

Phase 4 T1: narrow the wait scope from vkQueueWaitIdle (all queue,
including composite-side submissions) to wait_for_fences on the
PaintBatch's own fence. Behavior-equivalent for our submission
(still blocking); no longer serializes on unrelated composite
work.

The fence is allocated lazily inside submit_and_wait (before
queue_submit2; failures here treat the CB as never-queued, same
as queue_submit2 failure). On wait_for_fences failure (device-
lost / path-2), the fence is leaked alongside the CB and
resources — same handle-abandonment rule the existing path-2
documents. retire_now destroys the fence in the success case.

This closes the headline Phase 4 scope: the queue_wait_idle site
documented as the Phase 4 target in docs/status.md is gone. T2-T4
add the async-retirement story on top (best-effort flushes return
immediately; submitted batches retire on composite-tick poll).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Add async submit + per-batch retire-if-signaled

**Goal:** Add the building blocks for async retirement without changing existing behavior. `submit_async` is a new method that submits the batch with the fence but does NOT wait. `try_retire_if_signaled` checks `get_fence_status` and retires if signaled. Nothing calls these yet — T3 wires them up.

**Files:**
- Modify: `crates/yserver/src/kms/scheduler/paint_batch.rs`

### Step 1: Add `submit_async`

- [ ] **Step 1: Add the method to `impl PaintBatch`**

Append (immediately after `submit_and_wait`):

```rust
    /// Submit + return immediately (no wait). The batch transitions
    /// to `Submitted`; retirement happens later via
    /// `try_retire_if_signaled` (non-blocking) or the caller's
    /// explicit `wait_for_fences` on the returned fence.
    ///
    /// Mirrors `submit_and_wait`'s path-1 (submit failure) handling.
    /// Path-2 (wait failure) is not applicable here — there is no
    /// wait. The caller is responsible for either:
    ///   - Polling `try_retire_if_signaled` until it returns true,
    ///     OR
    ///   - Calling `wait_for_completion()` explicitly (which blocks
    ///     on the fence and retires on success, equivalent to
    ///     `submit_and_wait` from a Submitted state).
    ///
    /// Returns the fence handle on success (Copy `vk::Fence`,
    /// not a borrow), so callers that want to issue a synchronous
    /// `wait_for_fences` themselves can do so without going back
    /// through `&mut self`.
    pub fn submit_async(&mut self) -> Result<vk::Fence, BatchError> {
        match self.state {
            BatchState::Poisoned => return Err(BatchError::Poisoned),
            BatchState::Retired => return Err(BatchError::InvalidState(BatchState::Retired)),
            BatchState::Submitted => return Err(BatchError::InvalidState(BatchState::Submitted)),
            BatchState::Idle => {
                // No CB; transition straight to Retired and return
                // a null fence so the caller's poll never blocks.
                self.state = BatchState::Closed;
                self.retire_now();
                return Ok(vk::Fence::null());
            }
            BatchState::Recording => self.close()?,
            BatchState::Closed => {}
        }
        let cb = self.cb.expect("Closed implies cb was allocated");

        let fence_info = vk::FenceCreateInfo::default();
        let fence = match unsafe { self.vk.device.create_fence(&fence_info, None) } {
            Ok(f) => f,
            Err(e) => {
                unsafe { self.vk.device.free_command_buffers(self.pool, &[cb]) };
                self.cb = None;
                self.state = BatchState::Closed;
                self.poison();
                return Err(BatchError::Vk(e));
            }
        };
        self.fence = Some(fence);

        let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
        let submit = [vk::SubmitInfo2::default().command_buffer_infos(&cb_info)];

        if let Err(e) = unsafe {
            self.vk
                .device
                .queue_submit2(self.vk.graphics_queue, &submit, fence)
        } {
            unsafe {
                self.vk.device.destroy_fence(fence, None);
                self.vk.device.free_command_buffers(self.pool, &[cb]);
            }
            self.cb = None;
            self.fence = None;
            self.state = BatchState::Closed;
            self.poison();
            return Err(BatchError::Vk(e));
        }

        self.state = BatchState::Submitted;
        Ok(fence)
    }
```

### Step 2: Add `try_retire_if_signaled`

- [ ] **Step 2: Add the method to `impl PaintBatch`**

Append (immediately after `submit_async`):

```rust
    /// Non-blocking poll. If the batch is in `Submitted` and its
    /// fence is signaled, retire it (free CB, destroy fence,
    /// release resources) and return `true`. Otherwise return
    /// `false` (still in flight, or already in a terminal state).
    ///
    /// Treats fence-status query errors (other than `NOT_READY`)
    /// the same as a wait failure: the batch is left in
    /// `Submitted` and resources are leaked. The caller must
    /// surface this as a renderer-failed condition.
    pub fn try_retire_if_signaled(&mut self) -> Result<bool, BatchError> {
        if self.state != BatchState::Submitted {
            return Ok(false);
        }
        let Some(fence) = self.fence else {
            // Submitted without fence is a bug (T1 always allocates a
            // fence before transitioning to Submitted).
            debug_assert!(false, "Submitted without fence");
            return Ok(false);
        };
        let status = unsafe { self.vk.device.get_fence_status(fence) };
        match status {
            Ok(true) => {
                // Signaled.
                let cb = self.cb.expect("Submitted implies cb was allocated");
                unsafe { self.vk.device.free_command_buffers(self.pool, &[cb]) };
                self.cb = None;
                self.retire_now();
                Ok(true)
            }
            Ok(false) => Ok(false),
            Err(e) => {
                // Device-lost or similar. Same leak-rather-than-UB
                // rule as path-2.
                log::error!(
                    "PaintBatch::try_retire_if_signaled: get_fence_status failed \
                     ({e:?}); CB / fence / resources abandoned. KMS renderer is \
                     in an unrecoverable state — caller MUST tear down or disable."
                );
                Err(BatchError::Vk(e))
            }
        }
    }

    /// Blocking equivalent of `try_retire_if_signaled` — waits on the
    /// fence and retires on success. For strict-flush callers that
    /// `submit_async`'d earlier and now need synchronous completion.
    ///
    /// Same path-2 semantics as `submit_and_wait`'s wait-failure
    /// branch: on `wait_for_fences` error, the batch is left in
    /// `Submitted` and resources are leaked.
    pub fn wait_for_completion(&mut self) -> Result<(), BatchError> {
        if self.state != BatchState::Submitted {
            return Err(BatchError::InvalidState(self.state));
        }
        let Some(fence) = self.fence else {
            debug_assert!(false, "Submitted without fence");
            return Err(BatchError::InvalidState(self.state));
        };
        let fences = [fence];
        match unsafe { self.vk.device.wait_for_fences(&fences, true, u64::MAX) } {
            Ok(()) => {
                let cb = self.cb.expect("Submitted implies cb was allocated");
                unsafe { self.vk.device.free_command_buffers(self.pool, &[cb]) };
                self.cb = None;
                self.retire_now();
                Ok(())
            }
            Err(e) => {
                log::error!(
                    "PaintBatch::wait_for_completion: wait_for_fences failed \
                     ({e:?}); CB / fence / resources abandoned. KMS renderer is \
                     in an unrecoverable state — caller MUST tear down or disable."
                );
                Err(BatchError::Vk(e))
            }
        }
    }
```

### Step 3: Build

- [ ] **Step 3: `cargo check -p yserver`**

Expected: clean. The new methods have no callers yet; clippy may warn `unused` — that's fine, T3 wires them up.

If clippy complains about `dead_code` on `submit_async`/`try_retire_if_signaled`/`wait_for_completion`, **don't** add `#[allow(dead_code)]`. T3 lands callers in the next commit — the lint is correct for the intermediate state but resolves after T3.

If you'd rather keep T2 clean: add `#[expect(dead_code, reason = "wired up in Phase 4 T3")]` and remove the attribute in T3.

### Step 4: Tests + fmt + clippy

- [ ] **Step 4: `cargo test -p yserver --lib 2>&1 | tail -5`**

Expected: 138 passed.

- [ ] **Step 5: `cargo +nightly fmt --check`**

Expected: no diff.

- [ ] **Step 6: `cargo clippy -p yserver 2>&1 | tail -10`**

Expected: 5 pre-existing warnings + possibly 3 `dead_code` warnings for the new methods. The dead-code ones disappear in T3.

### Step 5: Commit T2

- [ ] **Step 7: Commit**

```bash
git add crates/yserver/src/kms/scheduler/paint_batch.rs
git commit -m "$(cat <<'EOF'
refactor(kms): add submit_async + try_retire_if_signaled + wait_for_completion

Phase 4 T2: building blocks for async PaintBatch retirement.

  - submit_async: submits the batch with a fence and returns
    immediately. Same path-1 (submit-failure) handling as
    submit_and_wait; no path-2 (no wait).
  - try_retire_if_signaled: non-blocking poll. Calls
    get_fence_status; if signaled, retires (free CB, destroy
    fence, release resources). On non-NOT_READY status error,
    treats as path-2 leak.
  - wait_for_completion: blocking equivalent of
    try_retire_if_signaled for strict-flush callers that
    submit_async'd earlier. Same path-2 semantics as
    submit_and_wait's wait branch.

No callers yet — T3 wires these into RenderScheduler and
flush_if_needed. Intermediate state may surface dead_code warnings
on the new methods; they resolve at T3.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Wire async retirement into the scheduler and `flush_if_needed`

**Goal:** Add the submitted-batch queue to `RenderScheduler`. Route best-effort `flush_if_needed` calls through the async path (submit + queue, return). Strict reasons stay synchronous (`submit_and_wait` or `submit_async + wait_for_completion`). The composite tick polls retired batches.

**Files:**
- Modify: `crates/yserver/src/kms/scheduler/mod.rs`
- Modify: `crates/yserver/src/kms/backend.rs`

### Step 1: Add the submitted-batch queue

- [ ] **Step 1: Edit `RenderScheduler` struct in `scheduler/mod.rs`**

Find:

```rust
#[derive(Debug, Default)]
pub struct RenderScheduler {
    pub in_flight: InFlight,
    pub current_paint_batch: Option<PaintBatch>,
}
```

Change to:

```rust
#[derive(Debug, Default)]
pub struct RenderScheduler {
    pub in_flight: InFlight,
    pub current_paint_batch: Option<PaintBatch>,
    /// FIFO queue of submitted-but-not-yet-retired paint batches.
    /// Pushed by `close_and_submit_async`; drained by
    /// `poll_retired_paint_batches` when each batch's fence
    /// signals. Retirement is strict-prefix-FIFO (stop at first
    /// non-signaled batch — resource lifetimes layer on submission
    /// order, same as `InFlight::drain_retired`).
    pub submitted_paint_batches: std::collections::VecDeque<PaintBatch>,
}
```

### Step 2: Add `close_and_submit_async` to `RenderScheduler`

- [ ] **Step 2: Add the method to `impl RenderScheduler`**

Append (immediately after the existing `close_and_submit`):

```rust
    /// Async sibling of `close_and_submit`. Submits the current
    /// paint batch and moves it to `submitted_paint_batches` for
    /// later poll-driven retirement. Returns the fence handle so
    /// callers that need synchronous completion (strict flushes)
    /// can `wait_for_fences` on it themselves.
    ///
    /// Behavior on no-batch / Idle / Poisoned matches
    /// `close_and_submit`:
    ///   - no batch open → `Ok(vk::Fence::null())`.
    ///   - Idle batch (no CB) → retires directly, returns null fence.
    ///   - Poisoned → `Ok(vk::Fence::null())` (best-effort
    ///     swallows; caller of strict flush separately surfaces).
    ///
    /// Path-1 (submit failure): same as `close_and_submit` — the
    /// returned `Err(BatchError::Vk)` is fatal to the KMS renderer.
    #[allow(clippy::missing_errors_doc)]
    pub fn close_and_submit_async(
        &mut self,
        dirty_outputs: Vec<usize>,
    ) -> Result<vk::Fence, BatchError> {
        let Some(mut batch) = self.current_paint_batch.take() else {
            return Ok(vk::Fence::null());
        };
        batch.dirty_outputs = dirty_outputs;
        match batch.submit_async() {
            Ok(fence) => {
                if batch.state() == BatchState::Submitted {
                    self.submitted_paint_batches.push_back(batch);
                }
                // else: state was Idle and submit_async retired
                // synchronously; nothing to push. fence is null.
                Ok(fence)
            }
            Err(BatchError::Poisoned) => {
                drop(batch); // poison()'s side effects already ran inside submit_async
                Ok(vk::Fence::null())
            }
            Err(e) => Err(e),
        }
    }
```

### Step 3: Add `poll_retired_paint_batches`

- [ ] **Step 3: Add the method to `impl RenderScheduler`**

Append:

```rust
    /// Non-blocking poll over the submitted-paint-batch queue.
    /// Walks the front-to-back, calls `try_retire_if_signaled` on
    /// each, and removes any that successfully retired. Stops at
    /// the first non-signaled batch (strict-prefix-FIFO).
    ///
    /// Returns the number of batches retired this call. On a
    /// fence-status error, the batch is left in the queue in
    /// `Submitted` (leaked resources) and the error propagates —
    /// the caller MUST treat this as a renderer-failed condition.
    #[allow(clippy::missing_errors_doc)]
    pub fn poll_retired_paint_batches(&mut self) -> Result<usize, BatchError> {
        let mut retired = 0;
        while let Some(batch) = self.submitted_paint_batches.front_mut() {
            match batch.try_retire_if_signaled()? {
                true => {
                    self.submitted_paint_batches.pop_front();
                    retired += 1;
                }
                false => break,
            }
        }
        Ok(retired)
    }

    /// Total in-flight paint batches (Submitted, not yet Retired).
    /// Used by backpressure logic in T4.
    pub fn pending_paint_batches(&self) -> usize {
        self.submitted_paint_batches.len()
    }
```

### Step 4: Branch `flush_if_needed` by strict vs best-effort

- [ ] **Step 4: Edit `flush_if_needed` in `backend.rs:1566-1620`**

Replace the body:

```rust
    pub fn flush_if_needed(
        &mut self,
        reason: crate::kms::scheduler::paint_batch::BatchFlushReason,
    ) -> Result<(), ash::vk::Result> {
        use crate::kms::scheduler::paint_batch::{BatchError, BatchFlushReason};
        if self.renderer_failed {
            return match reason {
                BatchFlushReason::Readback
                | BatchFlushReason::ExternalSync
                | BatchFlushReason::ProtocolBarrier => Err(ash::vk::Result::ERROR_DEVICE_LOST),
                _ => Ok(()),
            };
        }
        log::trace!("flush_if_needed: reason={reason:?}");
        let dirty_outputs: Vec<usize> = (0..self.outputs.len())
            .filter(|&i| self.outputs[i].damage.needs_composite())
            .collect();
        let strict = matches!(
            reason,
            BatchFlushReason::Readback
                | BatchFlushReason::ExternalSync
                | BatchFlushReason::ProtocolBarrier
        );

        // 4-T3: strict reasons use the blocking submit (close_and_submit
        // → submit_and_wait → wait_for_fences on the batch's own fence).
        // Best-effort reasons use the async path: close_and_submit_async
        // moves the batch to the submitted-paint-batches queue; the
        // composite-tick poll retires it later when its fence signals.
        let result = if strict {
            self.scheduler.close_and_submit(dirty_outputs)
        } else {
            self.scheduler
                .close_and_submit_async(dirty_outputs)
                .map(|_fence| ())
        };

        match result {
            Ok(()) => Ok(()),
            Err(BatchError::Vk(r)) => {
                log::error!(
                    "flush_if_needed({reason:?}): submit returned fatal {r:?}; \
                     latching renderer_failed — KMS renderer disabled until restart"
                );
                self.renderer_failed = true;
                Err(r)
            }
            Err(BatchError::Poisoned) if strict => {
                log::warn!(
                    "flush_if_needed({reason:?}): batch was Poisoned; \
                     caller's completion guarantee cannot be honoured"
                );
                Err(ash::vk::Result::ERROR_DEVICE_LOST)
            }
            Err(BatchError::InvalidState(s)) if strict => {
                log::error!(
                    "flush_if_needed({reason:?}): batch in invalid state {s:?}; \
                     caller's completion guarantee cannot be honoured"
                );
                Err(ash::vk::Result::ERROR_UNKNOWN)
            }
            Err(_) => Ok(()),
        }
    }
```

(One structural change: the `let result = self.scheduler.close_and_submit(...)` line now branches on `strict`. Best-effort goes async; strict stays blocking.)

### Step 5: Wire `poll_retired_paint_batches` into the composite tick

- [ ] **Step 5: Edit `poll_in_flight` in `backend.rs` around line 6891**

Find the start of `poll_in_flight` (the function that polls output-frame retirement). Add a paint-batch poll at the top:

```rust
    fn poll_in_flight(&mut self) {
        // 4-T3: drain any signaled paint batches first. Paint
        // batches retire independently of output frames — they
        // can signal even when no composite is in flight (e.g.,
        // a ProtocolBarrier flush in the middle of a paint cycle
        // that completes before the next composite).
        if let Err(e) = self.scheduler.poll_retired_paint_batches() {
            log::error!(
                "poll_in_flight: paint-batch retirement poll failed: {e:?}; \
                 latching renderer_failed"
            );
            self.renderer_failed = true;
            // Continue with output-frame polling — the latched
            // renderer_failed flag stops new paint work, but
            // existing scanout frames still need their pageflip
            // tracking to complete cleanly.
        }

        let n = self.scheduler.in_flight.len();
        // … existing body unchanged …
```

(Just prepend the poll-retired-paint-batches block at the top of the existing function body. The rest of `poll_in_flight` is unchanged.)

### Step 6: Build

- [ ] **Step 6: `cargo check -p yserver`**

Expected: clean. The `dead_code` warnings from T2 should now resolve (callers exist in `RenderScheduler::close_and_submit_async` and `poll_retired_paint_batches`).

### Step 7: Tests + fmt + clippy

- [ ] **Step 7: `cargo test -p yserver --lib 2>&1 | tail -5`**

Expected: 138 passed.

- [ ] **Step 8: `cargo +nightly fmt --check`**

Expected: no diff.

- [ ] **Step 9: `cargo clippy -p yserver 2>&1 | tail -10`**

Expected: 5 pre-existing warnings, no new ones.

### Step 8: Commit T3

- [ ] **Step 10: Commit**

```bash
git add crates/yserver/src/kms/scheduler/mod.rs crates/yserver/src/kms/backend.rs
git commit -m "$(cat <<'EOF'
refactor(kms): wire async paint-batch retirement into scheduler + flush_if_needed

Phase 4 T3: RenderScheduler gains submitted_paint_batches
(VecDeque<PaintBatch>) plus close_and_submit_async,
poll_retired_paint_batches, and pending_paint_batches.

flush_if_needed branches by reason:
  - strict (Readback / ExternalSync / ProtocolBarrier):
    close_and_submit (blocking; wait_for_fences inside
    PaintBatch::submit_and_wait). Caller-completion contract
    preserved.
  - best-effort (VisibleComposite / SizeLimit / LatencyLimit /
    Shutdown): close_and_submit_async (submit + queue; return
    immediately). Batches retire on composite-tick poll.

poll_in_flight (called at composite tick top + pageflip-complete
handler) now polls submitted_paint_batches first and drains
signaled ones FIFO. Fence-status errors latch renderer_failed,
same path-2 semantics as the synchronous wait.

No backpressure yet — T4 adds the queue-depth bound. Under heavy
clients (mate-cc + adapta-nokto pattern) the queue could grow
unbounded; T4 caps it.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Bound the submitted-paint-batches queue (backpressure)

**Goal:** Cap the number of in-flight paint batches at a small constant. When `close_and_submit_async` would push past the cap, block on the oldest batch first. Prevents unbounded queue growth under chatty clients.

**Files:**
- Modify: `crates/yserver/src/kms/scheduler/mod.rs`

### Step 1: Add the cap constant

- [ ] **Step 1: Add at the top of `scheduler/mod.rs` (after imports, before pub mod declarations)**

```rust
/// Maximum number of paint batches that can be Submitted but
/// not yet Retired. Phase 4 T4 backpressure: when
/// `close_and_submit_async` would push past this, the oldest
/// batch is blocking-waited first.
///
/// 4 ≈ one composite period at 60 Hz worth of paint cycles; high
/// enough to absorb bursty paint without blocking, low enough to
/// bound GPU-side queue depth and CPU-side resource lifetime.
const MAX_IN_FLIGHT_PAINT_BATCHES: usize = 4;
```

### Step 2: Wire backpressure into `close_and_submit_async`

- [ ] **Step 2: Edit `close_and_submit_async`**

Insert the backpressure check at the top of the method (before taking `current_paint_batch`):

```rust
    pub fn close_and_submit_async(
        &mut self,
        dirty_outputs: Vec<usize>,
    ) -> Result<vk::Fence, BatchError> {
        // 4-T4: backpressure. If the in-flight queue is at cap,
        // synchronously wait on the oldest batch before pushing
        // a new one. Prevents unbounded growth under chatty
        // clients (e.g. mate-cc + adapta-nokto's pixmap-churn
        // pattern that fires hundreds of ProtocolBarrier flushes
        // per second).
        while self.submitted_paint_batches.len() >= MAX_IN_FLIGHT_PAINT_BATCHES {
            let Some(mut oldest) = self.submitted_paint_batches.pop_front() else {
                break;
            };
            oldest.wait_for_completion()?;
            drop(oldest);
        }

        let Some(mut batch) = self.current_paint_batch.take() else {
            return Ok(vk::Fence::null());
        };
        // … rest unchanged …
```

### Step 3: Build

- [ ] **Step 3: `cargo check -p yserver`**

Expected: clean.

### Step 4: Tests + fmt + clippy

- [ ] **Step 4-6: standard verification gates**

```bash
cargo test -p yserver --lib 2>&1 | tail -5
cargo +nightly fmt --check
cargo clippy -p yserver 2>&1 | tail -10
```

Expected: 138 passed, no fmt diff, 5 pre-existing warnings.

### Step 5: Commit T4

- [ ] **Step 7: Commit**

```bash
git add crates/yserver/src/kms/scheduler/mod.rs
git commit -m "$(cat <<'EOF'
refactor(kms): bound submitted_paint_batches queue (backpressure)

Phase 4 T4: when close_and_submit_async would push past
MAX_IN_FLIGHT_PAINT_BATCHES (4), synchronously wait on the
oldest batch first via wait_for_completion. Prevents unbounded
queue growth under chatty clients.

4 ≈ one composite period at 60 Hz; high enough to absorb bursty
paint without blocking, low enough to bound GPU-side queue depth
and CPU-side resource lifetime.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Shutdown drain — block on remaining `submitted_paint_batches` before backend teardown

**Goal:** Phase 4 makes `flush_if_needed(Shutdown)` best-effort, so paint batches can still be in `submitted_paint_batches` when the backend tears down. Without an explicit drain, `PaintBatch::drop` for `Submitted` fires the leak warning (and actually leaks the CB / fence / arenas / descriptor pools per the path-2 contract). Add a final drain so the shutdown path retires every queued batch before the scheduler drops.

**Files:**
- Modify: `crates/yserver/src/kms/scheduler/mod.rs` (add `drain_submitted_paint_batches`).
- Modify: `crates/yserver/src/kms/backend.rs` (call it from the shutdown path, after `vkDeviceWaitIdle`).

### Step 1: Add `drain_submitted_paint_batches` to the scheduler

- [ ] **Step 1: Add the method to `impl RenderScheduler`** in `scheduler/mod.rs`, immediately after `poll_retired_paint_batches`:

```rust
    /// Synchronously retire every batch currently in the
    /// submitted-paint-batches queue. For shutdown only — best-
    /// effort `flush_if_needed(Shutdown)` would leave batches in
    /// the queue and `PaintBatch::drop` would leak them.
    ///
    /// Order: FIFO. Calls `wait_for_completion` on each batch.
    /// On any failure, the remaining batches are left in the
    /// queue (their `Drop` will fire the leak warning); the
    /// error propagates. Caller MUST already have latched a
    /// renderer-failed condition before calling, OR be prepared
    /// to handle the leak (acceptable at shutdown).
    ///
    /// Should be called AFTER `vkDeviceWaitIdle()` in the
    /// teardown sequence — at that point every fence is
    /// guaranteed signaled, so each `wait_for_completion`
    /// returns immediately. Calling before `vkDeviceWaitIdle`
    /// would block on each fence in turn (still correct, just
    /// pessimal).
    #[allow(clippy::missing_errors_doc)]
    pub fn drain_submitted_paint_batches(&mut self) -> Result<(), BatchError> {
        while let Some(mut batch) = self.submitted_paint_batches.pop_front() {
            batch.wait_for_completion()?;
            drop(batch);
        }
        Ok(())
    }
```

### Step 2: Find the shutdown path in `backend.rs`

- [ ] **Step 2: Find where the backend tears down**

```bash
rg -n 'fn disable_output\|fn shutdown\|device_wait_idle' crates/yserver/src/kms/backend.rs | head
```

Look for the function called at process-shutdown / `disable_output` time. The KMS teardown fix (`a693255`) added the 6-step `disable_output` that ends in `vkDeviceWaitIdle()` (search for `device_wait_idle` in `backend.rs`). The drain belongs **after** the `vkDeviceWaitIdle()` (every fence will already be signaled then) and **before** anything else that could drop the scheduler.

### Step 3: Insert the drain call

- [ ] **Step 3: Add the drain call immediately after `vkDeviceWaitIdle()`** in the shutdown sequence.

Find the `vkDeviceWaitIdle()` call site (approximate location: `backend.rs:~8246` from the codex-review citation; verify with grep before editing). Add:

```rust
            // 4-T5: drain any paint batches that didn't finish
            // retiring through the composite-tick poll. After
            // vkDeviceWaitIdle their fences are signaled, so
            // each wait_for_completion returns immediately —
            // we're just running the CB-free + resource-release
            // + fence-destroy sequence on the host side.
            if let Err(e) = self.scheduler.drain_submitted_paint_batches() {
                log::warn!(
                    "shutdown: drain_submitted_paint_batches failed ({e:?}); \
                     remaining batches will fire the leak warning on Drop"
                );
            }
```

(The exact location depends on the shutdown function's structure — slot it in immediately after the `device_wait_idle` call in whatever function does the teardown.)

### Step 4: Build

- [ ] **Step 4: `cargo check -p yserver`** — clean.

### Step 5: Tests + fmt + clippy

- [ ] **Step 5-7: standard verification**:
```bash
cargo test -p yserver --lib 2>&1 | tail -5    # 138 passed
cargo +nightly fmt --check                     # no diff
cargo clippy -p yserver 2>&1 | tail -10        # 5 pre-existing
```

### Step 6: Commit T5

- [ ] **Step 8: Commit**:

```bash
git add crates/yserver/src/kms/scheduler/mod.rs crates/yserver/src/kms/backend.rs
git commit -m "$(cat <<'EOF'
refactor(kms): drain submitted_paint_batches on shutdown

Phase 4 T5: Phase 4 makes flush_if_needed(Shutdown) best-effort,
so paint batches stay in submitted_paint_batches across the
shutdown handoff. Without an explicit drain, PaintBatch::drop
for Submitted fires its leak warning and abandons the CB /
fence / arenas / descriptor pools per the path-2 contract.

Add RenderScheduler::drain_submitted_paint_batches —
synchronously retires every queued batch via
wait_for_completion. Called immediately after vkDeviceWaitIdle
in the shutdown sequence so each fence is already signaled and
each wait returns instantly; we're just running the host-side
free + release + destroy sequence per batch.

Caught by codex review of the Phase 4 plan (P2).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Validation + phase-4 results doc

**Goal:** End-to-end verification + results doc following the 3F-1/3F-2 template.

**Files:**
- Create: `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase4-results.md`

### Step 1: Static verification

- [ ] **Step 1: Cutover greps**

```bash
cd /home/jos/Projects/yserver

# Phase 4 retired queue_wait_idle from submit_and_wait.
rg -n 'queue_wait_idle' crates/yserver/src/kms/scheduler/paint_batch.rs
# Expected: ZERO hits. The grow paths in mask_scratch / dst_readback /
# copy_scratch still use it; that's Phase 5 / Phase 6 scope.

# wait_for_fences is now the wait path.
rg -n 'wait_for_fences' crates/yserver/src/kms/scheduler/paint_batch.rs
# Expected: 2 hits (submit_and_wait + wait_for_completion).

# get_fence_status is the non-blocking probe.
rg -n 'get_fence_status' crates/yserver/src/kms/scheduler/paint_batch.rs
# Expected: 1 hit (try_retire_if_signaled).

# Async-retirement queue exists.
rg -n 'submitted_paint_batches' crates/yserver/src/kms/scheduler/
# Expected: field decl in mod.rs + ≥3 usages (push in close_and_submit_async,
# pop in poll + backpressure).

# flush_if_needed branches by strict.
rg -n 'close_and_submit_async\|close_and_submit\(' crates/yserver/src/kms/backend.rs
# Expected: both called from flush_if_needed (strict vs best-effort).

# Poll wired into composite tick.
rg -n 'poll_retired_paint_batches' crates/yserver/src/kms/backend.rs
# Expected: 1 call site (inside poll_in_flight).
```

- [ ] **Step 2: Tree green**

```bash
cargo +nightly fmt --check
cargo clippy -p yserver 2>&1 | tail -10
cargo test --workspace 2>&1 | tail -20
```

Expected: clean fmt, 5 pre-existing warnings, yserver lib 138 passed, workspace green.

### Step 2: Hardware smoke (REQUIRED)

- [ ] **Step 3: From a separate TTY**

Per the 3D-results.md teardown workflow. `just yserver-mate-hw-release` boots MATE; expectations:

1. **rendercheck full suite passes** (or matches pre-4 baseline). The fence-based wait is functionally equivalent for our submission.

2. **No `wait_for_fences failed`** / **no `get_fence_status failed`** in `yserver-hw.log`. Either is a renderer_failed latch.

3. **No `paint-batch retirement poll failed`** in `poll_in_flight`.

4. **No `PaintBatch::drop while Submitted`** warnings — every submitted batch should retire cleanly in steady state.

5. **Subjective input fluidity** — under non-adapta-nokto themes, cursor + key responsiveness should be **noticeably better than pre-4** because the core loop no longer blocks on `queue_wait_idle` per composite. Best-effort flushes (VisibleComposite at composite-and-flip; Shutdown) return immediately now.

6. **adapta-nokto + mate-cc on `bee`** — capture the lag character. Per the profile data, Phase 4 should not materially close the bee/RDNA2 lag (the bottleneck is amdgpu ioctl rate, not the close-time wait). If it does help — great, but it's not the expected outcome. If unchanged, this confirms the AMD-investigation phase is still needed and Phase 4 didn't address that workload.

7. **`silence` test if available** — same procedure on `silence` (Polaris10 + Arch recent kernel). The Friday office-trip data point.

8. **No regressions on `fuji`** — Intel was fine pre-Phase 4; should stay fine.

### Step 3: Write results doc

- [ ] **Step 4: Create `docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase4-results.md`**

Follow the 3F-2 template. Sections:

1. **Header**: title `Phase 4 — sync rework: retire close-time vkQueueWaitIdle — results`, date `2026-05-14` (or actual implementation date), plan ref `phase4.md`, branch `graphics-followups`, predecessor `phase3f-2-results.md`.

2. **Scope landed**: T1 (fence-based wait, narrows scope), T2 (async building blocks), T3 (wire async retirement + branch flush_if_needed), T4 (backpressure cap). Note the **headline win**: `queue_wait_idle` is gone from `paint_batch.rs`; submit_and_wait now uses `wait_for_fences` on a per-batch fence.

3. **Preflight checks**: real fmt / clippy / test counts.

4. **Cutover greps**: actual `rg` output.

5. **Done conditions**: enumerated below.

6. **Hardware smoke results**: TBD placeholder per the 3F-2 pattern. Include subjective-input-fluidity assessment + adapta-nokto on bee + silence test if completed + fuji regression check.

7. **Plan bugs caught**: any recipe-level issues during T1–T4.

8. **Commit summary** table.

9. **Known deferred items**: Phase 5 (per-glyph + readback-handler wait-idle retirement); Phase 6 (refcounted handles, batched fence destruction); AMD-investigation phase (resource pooling + ftrace).

10. **What's next**: per `docs/status.md`, the next phase decision after Phase 4 depends on the Friday silence + Nvidia test results (per `project_amd_lag_investigation.md` memory). Provisional: Phase 5 narrow to readback + glyph atlas, OR AMD-investigation phase if the silence test reveals broad recent-amdgpu regression.

### Step 4: Commit T6

- [ ] **Step 5: Commit**

```bash
git add docs/superpowers/plans/2026-05-13-rendering-rearchitecture-phase4-results.md
git commit -m "$(cat <<'EOF'
docs(plans): phase-4 validation results

T1 swapped vkQueueWaitIdle for a per-batch VkFence +
wait_for_fences in PaintBatch::submit_and_wait. T2 added
submit_async + try_retire_if_signaled + wait_for_completion as
async-retirement building blocks. T3 wired the building blocks
into RenderScheduler (submitted_paint_batches queue +
close_and_submit_async + poll_retired_paint_batches) and routed
flush_if_needed by reason (strict blocks; best-effort returns
immediately). T4 capped the queue at MAX_IN_FLIGHT_PAINT_BATCHES
= 4 with backpressure to wait_for_completion on the oldest.

Hardware smoke: <result>. Input fluidity vs pre-4: <better/same>.
adapta-nokto + mate-cc on bee: <unchanged as expected, or
surprise change>. silence + adapta-nokto: <result if tested>.

Next phase decision deferred to the Friday silence + Nvidia
data point.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Done conditions

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` produces 5 pre-existing warnings; no new warnings.
3. `cargo test --workspace` green; yserver lib 138 passed.
4. `vkQueueWaitIdle` does NOT appear in `paint_batch.rs` (grep `queue_wait_idle` returns ZERO in that file). Other call sites (Drop impls, scratch grow paths, run_one_shot_op) stay — they're Phase 5/Phase 6 scope.
5. `PaintBatch` has a `fence: Option<vk::Fence>` field allocated in `submit_and_wait` / `submit_async` before submit, destroyed in `retire_now`, leaked on path-2 wait failure.
6. `wait_for_fences` is called in exactly two places in `paint_batch.rs` (submit_and_wait + wait_for_completion).
7. `get_fence_status` is called in exactly one place in `paint_batch.rs` (try_retire_if_signaled).
8. `RenderScheduler` has `submitted_paint_batches: VecDeque<PaintBatch>`. `close_and_submit_async`, `poll_retired_paint_batches`, `pending_paint_batches` all exist.
9. `flush_if_needed` branches by strict vs best-effort and calls `close_and_submit` (strict) or `close_and_submit_async` (best-effort).
10. `poll_in_flight` calls `poll_retired_paint_batches` at its top, before the existing output-frame polling.
11. Backpressure: `close_and_submit_async` blocks on the oldest fence if the queue is at `MAX_IN_FLIGHT_PAINT_BATCHES = 4`.
12. `drain_submitted_paint_batches` exists on `RenderScheduler` and is called from the shutdown path **after** `vkDeviceWaitIdle()`. After shutdown returns, `submitted_paint_batches.is_empty()` (no leaked-Submitted warnings).
13. Hardware smoke green per T6 step 3.

## Cutover greps (post-4 — semantic, not numeric)

```
$ rg -n 'queue_wait_idle' crates/yserver/src/kms/scheduler/paint_batch.rs
# ZERO (the Phase 4 target is gone from this file).

$ rg -n 'queue_wait_idle' crates/yserver/src/kms/vk/
# Still present in Drop impls (mask_scratch, dst_readback, copy_scratch,
# solid_color_image, gradient — all backend-teardown only), and inside
# run_one_shot_op body (Phase 5 scope) plus the ensure_size grow paths.
# None on the per-frame paint hot path.

$ rg -n 'wait_for_fences' crates/yserver/src/kms/scheduler/paint_batch.rs
# 2 hits (submit_and_wait + wait_for_completion).

$ rg -n 'get_fence_status' crates/yserver/src/kms/
# 2 hits: paint_batch.rs (try_retire_if_signaled) + backend.rs
# (existing composite-fence poll in poll_in_flight).

$ rg -n 'submitted_paint_batches' crates/yserver/src/kms/scheduler/
# Field decl in mod.rs + push in close_and_submit_async + pop in
# poll_retired_paint_batches + pop in backpressure check + len in
# pending_paint_batches.

$ rg -n 'close_and_submit_async\|close_and_submit\(' crates/yserver/src/kms/backend.rs
# Both called from flush_if_needed (strict / best-effort branch).

$ rg -n 'poll_retired_paint_batches' crates/yserver/src/kms/
# Method definition in scheduler/mod.rs + 1 call site in
# backend.rs (top of poll_in_flight).

$ rg -n 'drain_submitted_paint_batches' crates/yserver/src/kms/
# Method definition in scheduler/mod.rs + 1 call site in
# backend.rs (in the shutdown path, after vkDeviceWaitIdle()).
```

## Notes for the implementer

- **The headline Phase 4 win lands in T1.** `queue_wait_idle` is gone from `submit_and_wait` after a single commit. T2–T4 are infrastructure for the async path; they're valuable but separable.
- **`Drop` for `PaintBatch::Submitted` stays a leak.** Don't add fence destruction there — the path-2 contract is that all Vulkan handles (CB, fence, arenas, resources) are leaked on wait failure.
- **No new tests to write.** The state-machine tests in `paint_batch.rs` don't exercise Vulkan; the new methods are covered by the hardware smoke. If you want a unit-testable invariant, a `BatchState`-only test for "Submitted requires fence" via a no-op mock could land — but the existing test pattern (commented `#[ignore]` tests waiting for a VkContext harness) suggests the project hasn't built that mock yet; don't start here.
- **Backpressure constant**: `MAX_IN_FLIGHT_PAINT_BATCHES = 4` is a starting point. If hardware smoke shows it's too low (frequent backpressure waits) or too high (excessive GPU queue depth), tune. The constant is private to `scheduler/mod.rs` so changes are local.
- **The composite-side semaphore handoff is NOT done in this phase.** `OutputFrame::holders` and `PaintBatch::holders` stay at 0; the composite path still depends on single-queue-submission-order GPU serialization. Phase 6 (batch-owned refcounted handles) is the right place to wire holders if/when needed.
- **Watch for**: if rendercheck regresses on Readback cases (GetImage, MIT-SHM GetImage), the strict-flush wait_for_fences path may have a bug — check that `close_and_submit` (NOT `_async`) is called for Readback. The grep at T6 Step 1 catches this if both paths exist.
