# KMS Teardown Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix `KmsBackend::disable_output` so that exiting yserver does not leave the kernel with framebuffers still bound to CRTCs (which currently breaks Wayland host compositors and emits `atomic remove_fb failed with -22` kernel warnings).

**Architecture:** Replace the current sequence (`vkDeviceWaitIdle` → force-reset BO state → atomic disable) with codex's 6-step recipe: stop new composites → flush PaintBatch → wait Vulkan → drain DRM pageflip-completes (without lying BOs to Free) → atomic disable → only THEN drop scanout resources. A new `shutting_down: bool` field gates `composite_and_flip` / `try_vulkan_composite_flip`; a new `drain_pending_pageflips_for_shutdown` helper polls the DRM fd until no BO is in `BoPhase::Pending`.

**Tech Stack:** Rust, ash (Vulkan), DRM atomic-modeset (via `drm` crate), `nix` for fd polling.

---

## Phase context — why this needs to ship

Phase 3D's hardware smoke surfaced this bug as a P0: the user's normal Wayland session (`labwc` + `dms`) does not recover after yserver exits. Codex pinpointed the root cause: `KmsBackend::disable_output` at `backend.rs:7996` calls `ScanoutBoPool::drain_all_pending(vk)`, which does `vkDeviceWaitIdle` and **force-resets all BO state to `Free`** (`scanout.rs:548-562`). It does NOT wait for or drain kernel page-flip completion events.

So userspace decides scanout BOs are reusable while KMS may still have framebuffers bound or flips pending. The subsequent atomic disable_output commit fails with `-EINVAL`, and RAII later destroys framebuffers KMS still knows about. The kernel emits `WARNING atomic remove_fb failed with -22` and the host compositor sees `qt.qpa.wayland: There are no outputs`.

X-based hosts (Xorg + lightdm) survived because Xorg's startup runs a more aggressive DRM reset before grabbing outputs. Wayland hosts (anything wlroots-based or QtWayland) surface the leak.

This blocks safe hardware testing of phase 3E onward. Until landed, every smoke run loses the user's desktop session.

Read `docs/known-issues.md` (the "P0: KMS teardown..." entry, at the end of the `## KMS backend (Phase 6.4 / 6.5)` section) for the full diagnosis.

## File structure

| File | Role | Touched in |
|---|---|---|
| `crates/yserver/src/kms/backend.rs` | Add `shutting_down: bool` field; gate `composite_and_flip` + `try_vulkan_composite_flip`; rewrite `disable_output` (~line 7996); add `drain_pending_pageflips_for_shutdown` helper method | T1, T2 |
| `crates/yserver/src/kms/vk/scanout.rs` | Add `pub fn has_pending_pageflip(&self) -> bool` on `ScanoutBoPool` — semantic predicate the shutdown helper polls until false. `drain_all_pending` keeps its body but its CALL SITE moves from before-atomic to after-atomic. | T1 |
| `docs/superpowers/plans/2026-05-13-kms-teardown-fix-results.md` | Results doc | T3 |

## Pre-task notes (read before starting)

1. **The recipe is codex's 6 steps**, in order:
   1. Stop submitting new composites.
   2. Flush + retire the open `PaintBatch`.
   3. Wait for submitted Vulkan work (`vkDeviceWaitIdle`).
   4. Drain DRM page-flip completions per output, **without** force-resetting BO state.
   5. Issue atomic `disable_output` per output.
   6. Drop / destroy scanout framebuffers and BOs (via RAII or post-atomic force-reset).

2. **The current code crosses steps 3 and 4** (it does Vk waitidle + force-resets BO state) and then jumps straight to step 5. The fix is to insert a real step 4 (drain kernel pageflip events until no `BoPhase::Pending`) BEFORE step 5, and move the force-reset (`drain_all_pending`) to AFTER step 5 — at that point KMS has released its hold and the force-reset is safe.

3. **Why the existing `drain_all_pending` exists at all**: it's safe AFTER atomic disable_output succeeds (modeset reset semantics — kernel has dropped the binding, so userspace can force-clean its BO state machine). The bug is only in calling it BEFORE.

4. **There is already `Drop for ScanoutBo`** at `scanout.rs:418` that defensively closes fence fds via the same `transition_to_free_after_modeset_reset` path. So if we skip `drain_all_pending` entirely on the success path, RAII handles it when `KmsBackend` drops shortly afterwards. We keep `drain_all_pending` post-atomic as belt-and-braces.

5. **`vkDeviceWaitIdle` is the only currently-deployed wait.** After step 3, GPU work is done. The pending state from step 4 is purely KMS — pageflips submitted to the kernel are waiting for VBlank. VBlank fires at ~16.7 ms (60 Hz). A 500 ms timeout on step 4 is comfortably above worst-case while still bailing fast if something is genuinely stuck.

6. **The DRM fd polling pattern**: `drain_events` (at `drm/page_flip.rs:98`) is non-blocking — reads whatever events are immediately available. To wait for VBlank we need `poll(POLLIN, timeout)` on the DRM fd, then drain. Use `nix::poll::poll` (already used elsewhere in the codebase for epoll).

7. **`BatchFlushReason::Shutdown`** already exists (`paint_batch.rs:91`) as a flush reason. It's a non-strict reason — best-effort, swallows the error rather than surfacing `DEVICE_LOST`. Correct for shutdown.

8. **Gate the composite path on `shutting_down` AND `renderer_failed`** at the same call sites. Both make `composite_and_flip` a no-op. Don't combine them into a single flag — `renderer_failed` is recoverable-via-restart while `shutting_down` is terminal-by-design.

9. **No new tests required for hardware behavior** (no DRM mock in the codebase). One small unit test for `ScanoutBoPool::has_pending_pageflip` predicate is reasonable. Hardware smoke is the validation gate.

10. **clippy**: project preference is plain `cargo clippy`. 5 pre-existing `doc_lazy_continuation` warnings remain; no new ones.

---

## Task 1: Add the infrastructure (gate flag + predicate + drain helper) — no behavior change yet

**Goal:** Add the building blocks. Existing `disable_output` still uses the old sequence at the end of T1; T2 flips it.

**Files:**
- Modify: `crates/yserver/src/kms/vk/scanout.rs` — add `pub fn has_pending_pageflip(&self) -> bool`
- Modify: `crates/yserver/src/kms/backend.rs` — add `shutting_down: bool` field + initializers; add `drain_pending_pageflips_for_shutdown` helper

### Step 1: Add `ScanoutBoPool::has_pending_pageflip`

- [ ] **Step 1a: Locate the `impl ScanoutBoPool` block**

Run: `grep -n "impl ScanoutBoPool" crates/yserver/src/kms/vk/scanout.rs`

Insert the new method directly above `pub fn drain_all_pending` (~line 547) so they sit next to each other (related concerns).

- [ ] **Step 1b: Add the predicate**

```rust
    /// True if any bo in this pool is in `BoPhase::Pending` —
    /// i.e. an atomic flip was accepted by KMS and the kernel
    /// hasn't yet emitted its pageflip-complete event for that
    /// flip. Used by the shutdown sequence to wait until KMS
    /// quiesces before issuing `disable_output`. Calling
    /// `disable_output` while a Pending bo exists is what
    /// produces the `atomic remove_fb failed with -22` kernel
    /// warning that leaves Wayland host compositors stranded.
    pub fn has_pending_pageflip(&self) -> bool {
        self.bos
            .iter()
            .any(|b| b.state.phase == BoPhase::Pending)
    }
```

- [ ] **Step 1c: Add a unit test**

Find the existing `#[cfg(test)] mod tests` block in `scanout.rs` (around line 900+). Add at the end of `mod tests`:

```rust
    #[test]
    fn has_pending_pageflip_reports_pending_state() {
        // We can't construct a full ScanoutBoPool without Vk + DRM,
        // but BoState alone is enough to exercise the predicate
        // logic by reaching into the inner state machine.
        let mut state = BoState::default();
        assert_eq!(state.phase, BoPhase::Free);

        // Walk to Submitted then Pending (via the actual transitions
        // so we don't bypass the state machine).
        state.transition_to_recording();
        state.transition_to_submitted(/* fake fd */ 42);
        let in_fd = state.transition_to_pending(/* fake out fence fd */ 43);
        assert!(matches!(in_fd, Some(42)));
        assert_eq!(state.phase, BoPhase::Pending);

        // The predicate's logic is `bos.iter().any(phase == Pending)`.
        // Confirm the Pending arm is what we expect to detect.
        let pending = state.phase == BoPhase::Pending;
        assert!(pending);

        // Walk on; should leave Pending.
        state.transition_to_on_screen();
        assert_eq!(state.phase, BoPhase::OnScreen);
        let on_screen_is_pending = state.phase == BoPhase::Pending;
        assert!(!on_screen_is_pending);

        // Closing the captured fds is fence-fd hygiene only; they're
        // not real fds here. Forget them to silence the SAFETY assumptions.
        std::mem::forget(in_fd);
    }
```

(If `BoState::default()` doesn't give phase = `Free`, the test will fail and you should read the `Default` impl to understand the starting state. The plan author confirmed `Free` is the `#[default]` variant at `scanout.rs:51`.)

- [ ] **Step 1d: Build + tests + clippy**

Run: `cargo check -p yserver` → expect clean.
Run: `cargo test -p yserver --lib has_pending_pageflip` → expect 1 passed.
Run: `cargo test -p yserver --lib` → expect 139 passed (was 138 before).
Run: `cargo +nightly fmt --check` → expect clean.
Run: `cargo clippy -p yserver 2>&1 | tail -5` → expect 5 pre-existing warnings, no new ones.

### Step 2: Add `shutting_down` field to `KmsBackend`

- [ ] **Step 2a: Locate the `KmsBackend` struct + its two initializer sites**

Run: `grep -n "renderer_failed:" crates/yserver/src/kms/backend.rs`
Expected: one field declaration + two initializers (in `open_with_commit` and in the test constructor).

- [ ] **Step 2b: Add the field**

Insert immediately after the `renderer_failed: bool` field declaration:

```rust
    /// Set by `disable_output` so the rest of teardown can run
    /// without `composite_and_flip` racing in and resubmitting a
    /// new frame. Latched once; never cleared.
    ///
    /// Distinct from `renderer_failed` (which models in-process
    /// Vk failure that an external supervisor could in principle
    /// restart around): `shutting_down` is terminal-by-design,
    /// triggered when `lib.rs` is unwinding the backend.
    pub(crate) shutting_down: bool,
```

- [ ] **Step 2c: Initialize at both constructor sites**

Run: `grep -n "renderer_failed: false" crates/yserver/src/kms/backend.rs`
Expected: two hits — one in `open_with_commit` (~line 1346), one in another constructor (~line 2207, the test path).

At each site, insert immediately after `renderer_failed: false,`:

```rust
            shutting_down: false,
```

Match the trailing-comma style of neighbouring fields (the struct literal uses trailing commas).

- [ ] **Step 2d: Build + tests**

Run: `cargo check -p yserver` → clean.
Run: `cargo test -p yserver --lib` → 139 passed.

### Step 3: Gate `composite_and_flip` and `try_vulkan_composite_flip` on `shutting_down`

- [ ] **Step 3a: Read existing gates**

Run: `grep -n "if self.renderer_failed" crates/yserver/src/kms/backend.rs | head -10`

Each site looks like:

```rust
if self.renderer_failed {
    return Ok(());        // or `return None` etc.
}
```

- [ ] **Step 3b: Extend the gates**

At each existing `self.renderer_failed` gate inside `composite_and_flip` (~line 6775) and `try_vulkan_composite_flip` (~line 7023), change to also include `shutting_down`. Concretely:

In `composite_and_flip`:

```rust
    pub fn composite_and_flip(&mut self) -> io::Result<()> {
        if self.renderer_failed || self.shutting_down {
            // Renderer is in fatal state or backend is shutting down;
            // skip paint+composite. The backend is alive enough to
            // drain pageflip-completes and process input — clients
            // still see the X server, they just see the last good
            // frame on screen.
            return Ok(());
        }
        // ... rest unchanged ...
```

In `try_vulkan_composite_flip`:

```rust
    fn try_vulkan_composite_flip(
        &mut self,
        layout_idx: usize,
        visible: &[u32],
    ) -> Option<(usize, usize)> {
        if self.renderer_failed || self.shutting_down {
            return None;
        }
        // ... rest unchanged ...
```

- [ ] **Step 3c: Build + tests**

Run: `cargo check -p yserver` → clean.
Run: `cargo test -p yserver --lib` → 139 passed.

### Step 4: Add the `drain_pending_pageflips_for_shutdown` helper

- [ ] **Step 4a: Locate the existing `drain_page_flips_and_composite`**

Run: `grep -n "fn drain_page_flips_and_composite\|fn disable_output" crates/yserver/src/kms/backend.rs`

The new helper goes immediately ABOVE `disable_output` so it's adjacent to its caller.

- [ ] **Step 4b: Add the helper method**

Add this method to the same `impl KmsBackend` block as `disable_output`:

```rust
    /// Shutdown step 4 (per docs/known-issues.md "P0: KMS teardown..."):
    /// drain DRM pageflip-complete events until no scanout BO is in
    /// `BoPhase::Pending` (i.e., the kernel has finished honouring
    /// every flip we submitted before shutdown began). The atomic
    /// `disable_output` commit in step 5 only succeeds when KMS has
    /// no in-flight flips on the connector.
    ///
    /// Polls the DRM fd with `nix::poll::poll` (POLLIN, 50 ms timeout
    /// per iteration), drains via the existing
    /// `drm::page_flip::drain_events`, and re-checks. Bails after
    /// `MAX_WAIT_MS` total elapsed with a warn log — at that point
    /// something is genuinely stuck and proceeding to the atomic
    /// disable is the least-bad option (it may still fail, but we
    /// avoid hanging the shutdown path indefinitely).
    ///
    /// Caller MUST NOT have called `ScanoutBoPool::drain_all_pending`
    /// before this — that force-resets BO state to Free and would
    /// make `has_pending_pageflip` lie.
    fn drain_pending_pageflips_for_shutdown(&mut self) -> io::Result<()> {
        use ::drm::control::crtc;
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
        use std::os::fd::AsFd;
        use std::time::Instant;

        const POLL_INTERVAL_MS: u8 = 50;
        const MAX_WAIT_MS: u128 = 500;

        let started = Instant::now();
        loop {
            let any_pending = self
                .scanout_pools
                .iter()
                .filter_map(|p| p.as_ref())
                .any(|p| p.has_pending_pageflip());
            if !any_pending {
                return Ok(());
            }
            if started.elapsed().as_millis() >= MAX_WAIT_MS {
                log::warn!(
                    "shutdown: drain_pending_pageflips_for_shutdown timed out after {MAX_WAIT_MS} ms; \
                     proceeding to atomic disable_output anyway (it may fail)"
                );
                return Ok(());
            }

            // Wait for the DRM fd to have an event ready. Crucial:
            // `drm::Device::receive_events()` is a blocking `read()`,
            // so we MUST only call drain_events when poll reports
            // POLLIN. A bare `Ok(_)` would include Ok(0) (timeout, no
            // readiness) and a subsequent blocking read could hang
            // past the 500 ms ceiling.
            let fd_borrow = self.device.as_fd();
            let mut fds = [PollFd::new(fd_borrow, PollFlags::POLLIN)];
            let timeout = PollTimeout::try_from(POLL_INTERVAL_MS)
                .unwrap_or(PollTimeout::from(50u8));
            let ready = match poll(&mut fds, timeout) {
                Ok(0) => false,
                Ok(_) => fds[0]
                    .revents()
                    .map(|r| r.contains(PollFlags::POLLIN))
                    .unwrap_or(false),
                Err(nix::errno::Errno::EINTR) => false,
                Err(e) => {
                    log::warn!("shutdown: drain_pending_pageflips_for_shutdown poll failed: {e}");
                    return Ok(());
                }
            };
            if !ready {
                continue;
            }

            // Drain whatever events are available; transition Pending → OnScreen
            // for each completing CRTC. Re-uses the existing per-event handler
            // from drain_page_flips_and_composite, but without the composite-and-flip
            // tail-call (shutting_down gates that out anyway).
            let mut flipped: Vec<crtc::Handle> = Vec::new();
            if let Err(e) = drm::page_flip::drain_events(&self.device, |c| flipped.push(c)) {
                log::warn!("shutdown: drain_events failed: {e}");
                return Ok(());
            }
            for c in flipped {
                let Some(output_idx) =
                    self.outputs.iter().position(|o| o.output.crtc == c)
                else {
                    continue;
                };
                if let Some(pool) = self
                    .scanout_pools
                    .get_mut(output_idx)
                    .and_then(|p| p.as_mut())
                {
                    advance_pool_on_pageflip_complete(pool);
                }
            }
        }
    }
```

- [ ] **Step 4c: Verify `nix::poll::poll` and `nix::poll::PollTimeout` are available**

Run: `grep -rn "nix::poll\|use nix.*poll" crates/yserver/src/`
Expected: at least one existing use (likely in `input_thread.rs` or `core_loop`). Confirms the import path. If not, check `crates/yserver/Cargo.toml` for the `nix` features — should include `"poll"` (looking at the workspace `Cargo.toml`, `nix` is configured with `["event", "fs", "ioctl", "mman", "poll", "signal", "term"]` per the workspace-deps).

- [ ] **Step 4d: Build**

Run: `cargo check -p yserver`
Expected: clean.

If `Device::as_fd` is not in scope: import `use std::os::fd::AsFd;` (already in the helper body, but the `Device` type must implement `AsFd`). If not, use `device.fd()` or look up the existing pattern in `input_thread.rs` for how it polls fds.

If `nix::poll::poll` signature differs from what's written: in `nix` 0.31 the signature is `poll(fds: &mut [PollFd], timeout: PollTimeout) -> Result<i32>`. Check the nix version in `Cargo.toml`.

- [ ] **Step 4e: Run tests**

Run: `cargo test -p yserver --lib` → 139 passed.

### Step 5: Commit T1

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/vk/scanout.rs crates/yserver/src/kms/backend.rs
git commit -m "$(cat <<'EOF'
feat(kms): add shutting_down gate + has_pending_pageflip + shutdown drain helper

T1 of the KMS teardown fix. No behavior change yet: this just adds
the infrastructure T2 will rewire disable_output to use.

- ScanoutBoPool::has_pending_pageflip: predicate exposing whether
  any bo is in BoPhase::Pending (KMS-accepted flip with no
  pageflip-complete event yet).
- KmsBackend::shutting_down: bool field, latched in disable_output
  (wired in T2). composite_and_flip and try_vulkan_composite_flip
  early-out on the flag, alongside the existing renderer_failed
  gate.
- KmsBackend::drain_pending_pageflips_for_shutdown: poll-loop
  helper that drains kernel pageflip events until no bo is
  Pending, with a 500 ms ceiling. Will be called from disable_output
  in T2.

Reference: docs/known-issues.md "P0: KMS teardown..." entry.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Rewire `disable_output` to follow the 6-step sequence

**Goal:** Replace the bad sequence (force-reset BOs → atomic disable → broken) with codex's correct 6-step sequence. This is the load-bearing fix.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (`disable_output`, ~line 7996)

### Step 1: Read the current `disable_output`

- [ ] **Step 1: Read backend.rs lines 7996–8020**

The current body is:

```rust
    pub fn disable_output(&mut self) -> io::Result<()> {
        // Drain in-flight scanout-bo state first: vkDeviceWaitIdle +
        // close any held fence fds. Without this, mid-flight Vulkan
        // submits could race the DRM disable_output ioctl and leak
        // fds. No-op when no Vulkan-fed pool exists for an output.
        if let Some(vk) = self.vk.as_ref() {
            for pool in &mut self.scanout_pools {
                if let Some(p) = pool.as_mut() {
                    p.drain_all_pending(vk);
                }
            }
        }

        let mut last_err: Option<io::Error> = None;
        for layout in &self.outputs {
            if let Err(e) = drm::modeset::disable_output(&self.device, &layout.output) {
                log::warn!(
                    "disable_output failed for {}: {e}",
                    layout.output.connector_name
                );
                last_err = Some(e);
            }
        }
        last_err.map_or(Ok(()), Err)
    }
```

The bug: `drain_all_pending` runs BEFORE the atomic disable. It does `vkDeviceWaitIdle` + force-resets every BO to `Free`. After this, `has_pending_pageflip` would lie (would say "no pending" even when KMS is still mid-flip). The atomic disable then fails with EINVAL because KMS still has FBs bound.

### Step 2: Replace the body

- [ ] **Step 2: Apply the 6-step replacement**

Replace the entire body of `pub fn disable_output(&mut self) -> io::Result<()>` (the function declaration line stays unchanged):

```rust
    pub fn disable_output(&mut self) -> io::Result<()> {
        // 6-step teardown per codex pinpoint (docs/known-issues.md
        // "P0: KMS teardown..."). The previous implementation
        // collapsed steps 3+4 by force-resetting BO state via
        // drain_all_pending BEFORE the atomic disable, which lied
        // BOs to Free while KMS still had FBs bound → atomic disable
        // EINVAL → kernel `atomic remove_fb failed with -22` warning
        // and Wayland host compositors saw no outputs.

        // Step 1: Stop submitting new composites. composite_and_flip
        // and try_vulkan_composite_flip both early-return when this
        // is true.
        self.shutting_down = true;

        // Step 2: Flush + retire the open PaintBatch (best-effort —
        // BatchFlushReason::Shutdown is a non-strict reason; failures
        // are logged but don't abort the rest of shutdown).
        if let Err(e) = self.flush_if_needed(
            crate::kms::scheduler::paint_batch::BatchFlushReason::Shutdown,
        ) {
            log::warn!("shutdown: PaintBatch flush failed: {e:?}");
        }

        // Step 3: Wait for any submitted Vulkan work to complete.
        // After this, GPU is idle; KMS may still have pageflips
        // pending in its own queue, but no new Vk submits can race
        // shutdown (step 1 stopped them) and no in-flight Vk work
        // can race the upcoming atomic commit.
        if let Some(vk) = self.vk.as_ref() {
            if let Err(e) = unsafe { vk.device.device_wait_idle() } {
                log::warn!("shutdown: vkDeviceWaitIdle: {e}");
            }
        }

        // Step 4: Drain DRM pageflip completions per output until no
        // bo is in BoPhase::Pending. Bounded by a 500 ms ceiling so a
        // genuinely stuck kernel doesn't hang shutdown. DO NOT
        // force-reset BO state here — has_pending_pageflip must
        // observe the real KMS state, not a userspace lie.
        if let Err(e) = self.drain_pending_pageflips_for_shutdown() {
            log::warn!("shutdown: drain_pending_pageflips: {e}");
        }

        // Step 5: Now safe to issue the atomic disable_output per
        // output. With no Pending flips, the kernel will accept the
        // commit instead of returning EINVAL. Track per-output
        // success so step 6 can skip force-reset on outputs whose
        // disable failed (those still have KMS bindings).
        let mut last_err: Option<io::Error> = None;
        let mut disable_ok: Vec<bool> = Vec::with_capacity(self.outputs.len());
        for layout in &self.outputs {
            match drm::modeset::disable_output(&self.device, &layout.output) {
                Ok(()) => disable_ok.push(true),
                Err(e) => {
                    log::warn!(
                        "disable_output failed for {}: {e}",
                        layout.output.connector_name
                    );
                    disable_ok.push(false);
                    last_err = Some(e);
                }
            }
        }

        // Step 6: For each output whose atomic disable succeeded, KMS
        // has released its hold on the framebuffer; force-resetting
        // BO state (close any straggler fence fds) is now safe and
        // RAII drops the scanout pool when KmsBackend itself drops.
        // For an output whose disable FAILED, KMS may still hold the
        // FB — force-resetting that pool would re-introduce the
        // exact UAF this fix is meant to prevent. Skip it; the
        // straggler fences leak into the kernel's lifecycle (sync_file
        // refs survive our fd close until the DRM device closes).
        // This is codex's per-output-success gating.
        if let Some(vk) = self.vk.as_ref() {
            for (idx, pool) in self.scanout_pools.iter_mut().enumerate() {
                let success = disable_ok.get(idx).copied().unwrap_or(false);
                if !success {
                    continue;
                }
                if let Some(p) = pool.as_mut() {
                    p.drain_all_pending(vk);
                }
            }
        }

        last_err.map_or(Ok(()), Err)
    }
```

- [ ] **Step 3: Build**

Run: `cargo check -p yserver`
Expected: clean.

- [ ] **Step 4: Run tests**

Run: `cargo test -p yserver --lib`
Expected: 139 passed (same as T1 end).

- [ ] **Step 5: fmt + clippy**

Run: `cargo +nightly fmt --check` → clean.
Run: `cargo clippy -p yserver 2>&1 | tail -5` → 5 pre-existing warnings, no new ones.

### Step 3: Commit T2

- [ ] **Step 6: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "$(cat <<'EOF'
fix(kms): rewire disable_output to codex's 6-step teardown sequence

The OLD sequence force-reset all scanout BO state to Free (via
drain_all_pending) BEFORE the atomic disable_output commit. That
lied BO state to userspace while KMS still had FBs bound. The
atomic commit then failed with EINVAL, and host Wayland
compositors saw `qt.qpa.wayland: There are no outputs` after
yserver exited. Reboot was required to recover.

The NEW sequence (per docs/known-issues.md P0 entry, pinpointed
by codex):

1. Set shutting_down = true so composite_and_flip stops submitting
   new work.
2. Flush + retire the open PaintBatch
   (BatchFlushReason::Shutdown).
3. vkDeviceWaitIdle so no Vk submit races the upcoming atomic
   commit.
4. Drain DRM pageflip-completes per output until no bo is in
   BoPhase::Pending. Bounded by a 500 ms ceiling.
5. Issue the atomic disable_output per output. With no Pending
   flips, the kernel accepts the commit.
6. NOW it's safe to force-reset BO state (close straggler fence
   fds) and let RAII handle the rest.

Hardware-test this from a separate TTY (Ctrl+Alt+F3 → run →
Ctrl+Alt+F1 to return) in case the fix is incomplete and we still
need to reboot — but the goal is that the user's normal Wayland
session (dms + labwc) recovers cleanly when yserver exits.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Validation + hardware smoke + results doc

**Goal:** Confirm tree green, run targeted hardware smoke, write the results doc.

**Files:**
- Create: `docs/superpowers/plans/2026-05-13-kms-teardown-fix-results.md`

### Step 1: Static verification

- [ ] **Step 1: Tree green**

```bash
cargo +nightly fmt --check
cargo clippy -p yserver 2>&1 | tail -5
cargo test --workspace 2>&1 | tail -15
```

Expected:
- fmt: no diff.
- clippy: 5 pre-existing `doc_lazy_continuation` warnings; no new ones.
- tests: yserver lib 139 passed (138 prior + 1 new `has_pending_pageflip_*` test), workspace green.

- [ ] **Step 2: Greps confirm 6-step structure**

```bash
rg -n 'self.shutting_down = true' crates/yserver/src/kms/backend.rs
# Expected: exactly 1 hit, inside disable_output

rg -n 'shutting_down' crates/yserver/src/kms/backend.rs
# Expected: field declaration + 2 initializer sites + composite_and_flip
# gate + try_vulkan_composite_flip gate + disable_output write = 6+ hits

rg -n 'drain_pending_pageflips_for_shutdown\|has_pending_pageflip' crates/yserver/src/kms/
# Expected: 1 helper definition + 1 disable_output call + 1 has_pending_pageflip
# definition + 1 use inside the helper + 1 in the unit test = 5+ hits
```

### Step 2: Hardware smoke (REQUIRED)

- [ ] **Step 3: Run from a separate TTY**

This is the key smoke. The goal is verifying the user's normal Wayland session (`dms` + `labwc`) recovers cleanly when yserver exits, not that yserver itself runs.

**Setup**: switch to a free TTY (Ctrl+Alt+F3) so the normal `dms` session on F1 stays intact even if the fix is incomplete.

```bash
# On the F3 TTY:
sudo systemctl stop kmscon@tty3   # if kmscon is grabbing F3
just yserver-mate-hw-release      # or whatever recipe boots yserver
```

Exercise briefly (one xterm + one mate-file-manager scroll is enough).

Then exit yserver via the normal path (Ctrl-C / SIGINT — the same path that hit the bug). Watch:

1. **`yserver-hw.log` tail**: NO `disable_output failed for ... Invalid argument` warning. If it appears, the fix didn't land or the 6-step sequence has a bug. Investigate before declaring success.
2. **`journalctl -k --since "10 seconds ago"`**: NO `atomic remove_fb failed with -22` warning. If the kernel warning is gone, the fix worked at the kernel level.
3. **Switch back to F1 (Ctrl+Alt+F1)**: dms + labwc session should be intact (outputs visible, can move mouse, click windows).

If F1 is OK after yserver exit on F3 — the fix is validated.

If F1 is still broken — `journalctl -b -k | tail` to see what the kernel said. The most likely failure modes:
- `drain_pending_pageflips_for_shutdown` timed out (the 500 ms ceiling fired) → kernel is genuinely stuck. Investigate why.
- atomic disable_output still EINVAL → the 6-step sequence is correct but `disable_output` in `drm/modeset.rs:387` itself needs different property handling (e.g., MODE_ID = 0 requires the blob property unset specifically, not just the property = 0). Read kernel's atomic-modeset state machine docs.
- Something else: report the journal lines and we figure it out.

### Step 3: Write results doc

- [ ] **Step 4: Create `docs/superpowers/plans/2026-05-13-kms-teardown-fix-results.md`**

Follow the 3A/3B/3C/3D template. Sections:

1. **Header**: title, date, plan ref, branch, predecessor (this is independent of phase 3D-results.md but worth pointing back to that as the discovery context).
2. **Scope landed**: paragraph + bullets — `shutting_down` gate, `has_pending_pageflip` predicate, `drain_pending_pageflips_for_shutdown` helper, 6-step `disable_output` rewrite.
3. **Preflight checks**: fmt, clippy, test counts.
4. **Verification greps**: confirm 6-step structure, gate placement.
5. **Done conditions**: enumerated below.
6. **Hardware smoke**: report actual result. Hostname, what was exercised, what `journalctl -k` showed after yserver exit, whether F1 session recovered. This is the load-bearing validation.
7. **Plan bugs caught (folded back into plan)**: any recipe issues hit during execution. If none, write "None — recipe applied cleanly."
8. **Commit summary** table: T1, T2, T3.
9. **Known deferred items**: longer-term batch-adopt model for scratch images, broader teardown polish (e.g., signal-handler-driven cleanup, panic-hook), nothing must follow immediately.
10. **What's next**: phase 3E (text + render-composite) is now safe to hardware-test again.

- [ ] **Step 5: Commit**

```bash
git add docs/superpowers/plans/2026-05-13-kms-teardown-fix-results.md
git commit -m "$(cat <<'EOF'
docs(plans): KMS teardown fix — validation results

disable_output now follows codex's 6-step sequence: stop new
composites → flush PaintBatch → vkDeviceWaitIdle → drain DRM
pageflip-completes → atomic disable → force-reset BO state. The
old order's bug (force-reset BOs BEFORE atomic disable → EINVAL
→ kernel `atomic remove_fb failed -22` → Wayland host saw no
outputs) is gone.

Hardware smoke on <host>: dms + labwc session recovered cleanly
after yserver exit on a separate TTY. No kernel WARN, no
disable_output failure log.

Unblocks phase 3E hardware testing.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Done conditions

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` produces 5 pre-existing `doc_lazy_continuation` warnings; no new warnings.
3. `cargo test --workspace` green; yserver lib 139 passed (was 138 pre-T1).
4. `KmsBackend::shutting_down: bool` exists; both constructors initialize it; both composite paths (`composite_and_flip`, `try_vulkan_composite_flip`) gate on it.
5. `ScanoutBoPool::has_pending_pageflip` exists with the obvious "any bo in `BoPhase::Pending`" body and a unit test.
6. `drain_pending_pageflips_for_shutdown` exists; polls the DRM fd with a 50 ms timeout per iteration; bounded by 500 ms total; calls `advance_pool_on_pageflip_complete` for each completing CRTC; does NOT force-reset BO state. **Crucial**: only calls `drain_events` when `poll` reports `POLLIN` in `revents`. `drm::Device::receive_events()` is a blocking `read()`; calling it without confirmed readiness can hang past the timeout.
7. `disable_output` follows the 6-step sequence in the documented order. `drain_all_pending` runs AFTER the atomic disable, not before — AND it runs only for outputs whose atomic disable succeeded (per-output `disable_ok` gating). Outputs whose disable failed are skipped on the force-reset path because KMS may still hold their FB; force-resetting would reintroduce the original UAF.
8. Hardware smoke: yserver exit on a separate TTY leaves the dms + labwc session on F1 intact; `journalctl -k` shows no `atomic remove_fb failed with -22` warning after yserver exit; `yserver-hw.log` shows no `disable_output failed ... Invalid argument` warning.

## Verification greps (post-fix)

```
$ rg -n 'self.shutting_down = true' crates/yserver/src/kms/backend.rs
# Expected: 1 hit, inside disable_output.

$ rg -n 'shutting_down' crates/yserver/src/kms/backend.rs
# Expected: ≥ 6 hits — field declaration, 2 initializers, composite_and_flip
# gate, try_vulkan_composite_flip gate, disable_output write.

$ rg -n 'drain_pending_pageflips_for_shutdown' crates/yserver/src/kms/backend.rs
# Expected: 2 hits — definition + 1 disable_output call site.

$ rg -n 'has_pending_pageflip' crates/yserver/src/kms/
# Expected: ≥ 3 hits — definition, use in drain_pending_pageflips_for_shutdown,
# unit test.

$ rg -n 'drain_all_pending' crates/yserver/src/kms/backend.rs
# Expected: 1 hit, inside disable_output, AFTER the atomic disable_output
# call (step 6, post-atomic force-reset). The pre-atomic call from before
# T2 must be gone.
```

## Notes for the implementer

- **The bug class is "wait for the kernel before lying to userspace about resource state."** Same pattern showed up in 3B's drawable-destruction barriers (don't drop a VkImage referenced by an open batch CB) and 3D's CopyScratch resize (don't `ensure_size`'s queue_wait_idle while a batch CB references the old image). Future scratch-lifetime work in phase 4 will probably want a `trait BatchableResource { fn quiesce_before_destroy(); }` shape; out of scope here.
- **The 500 ms ceiling is generous.** 60 Hz VBlank fires every ~16.7 ms; a stuck flip is almost certainly a software bug elsewhere (driver hang, scheduler quirk). Logging + proceeding is the right behavior — at least we don't hang shutdown.
- **`nix::poll::PollTimeout` constructor**: in nix 0.31 it's `PollTimeout::from(50u8)` or `PollTimeout::try_from(50i32).unwrap()`. Read the existing usage in the codebase (likely `crates/yserver/src/input_thread.rs`'s epoll loop) — it uses `epoll` not `poll` so the helper may be slightly different. If `nix::poll::poll` isn't a great fit, fall back to a manual `libc::poll` call or use `std::os::fd::AsFd` + `nix::sys::epoll::Epoll::new` (already used in `input_thread.rs`).
- **The hardware smoke is the load-bearing validation.** Unit tests can verify the predicate and the 6-step structure but cannot verify the kernel's atomic-modeset state machine accepts the disable. Don't skip the smoke.
- **The fix is small (~80 lines) but high-leverage.** It unblocks every future hardware test of phase 3E and beyond.
