# Paint → composite → flip sync rework — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace per-paint-op `vkQueueWaitIdle` in the KMS backend's paint → composite → flip hot path with Vulkan-native sync, so a single mate-control-center hover or wezterm-open no longer spikes GPU to 100% on Polaris.

**Architecture (per spec v5):** All paint ops in a frame record into a single per-frame primary command buffer. At the next core-loop quiescent point that command buffer is submitted once, signalling **N binary `paint_done[X]` semaphores** in one go — one per output in `C(F)` (the set of outputs that will actually composite this flush, decided after pageflip-completion processing). Each output's composite waits on its dedicated `paint_done[X]`, signals an exported `composite_done[X]` SYNC_FD, and hands the fd to KMS as `IN_FENCE_FD`. Skipped outputs (vk_flip_pending) get neither a signal nor a wait — their later catch-up composite is correct by same-queue submission ordering, not by carried semaphores.

**Tech Stack:** Rust, ash (Vulkan 1.3), `synchronization2`, `VK_KHR_external_semaphore_fd` (SYNC_FD binary export), DRM atomic-modeset with `IN_FENCE_FD` / `OUT_FENCE_PTR`, mio core loop.

**Spec:** `docs/superpowers/specs/2026-05-12-paint-composite-sync-design.md` (v5).

**Branch:** work on feature branch (per AGENTS.md). Suggested name: `paint-sync-rework`. Squash-merge to master at the end, after user confirmation.

---

## File map

**New files**

- `crates/yserver/src/kms/vk/external_sem_probe.rs` — P0 capability probes (binary + timeline SYNC_FD export).
- `crates/yserver/src/kms/vk/semaphore_pool.rs` — `(slot_id, output_id)`-keyed binary `VkSemaphore` pool for `paint_done`; separate export-configured pool for `composite_done`.
- `crates/yserver/src/kms/vk/fence_pool.rs` — `VkFence` pool for CPU-side retirement (one per Frame, plus per-output composite fences, plus one-shot for `record_get_image`).
- `crates/yserver/src/kms/vk/frame.rs` — `FrameSlot`, `FramePool`. Frame state with `HashMap<OutputId, …>` for per-output semaphores/fences.
- `crates/yserver/src/kms/vk/composite_descriptor_ring.rs` — Tier 1b ring keyed on `(slot_id, output_id)` replacing `CompositorPipeline::descriptor_pool`'s single-pool design.
- `crates/yserver/src/kms/vk/frame_resource.rs` — `FrameScopedQueue<T>` for per-op scratch lifetime, slot-based (wrap-safe) retirement.
- `crates/yserver/src/kms/vk/legacy_dispatch.rs` — Rollout helper. During P3 the legacy submit+wait_idle is wrapped by this helper, which flushes any open frame and runs deferred composites first.
- `crates/yserver/src/kms/vk/bounded_wait.rs` — Single helper `wait_for_fences_bounded(fences, &Vk) -> Result<(), WaitErr>` with the ≤250 ms timeout, used at every hot-path fence-wait site.
- `tests/kms-vk-sync/` — `#[ignore]` integration smoke tests.

**Modified files**

- `crates/yserver/src/kms/vk/device.rs` — wire P0 probe into `VkContext` startup; carry per-device capability flags.
- `crates/yserver/src/kms/vk/ops/mod.rs` — `OpsCommandPool` and `run_one_shot_op` stay during P3 (legacy dispatch uses them) and are removed in P4. Add `record_into_frame(&mut self, |cb| …)` helper for new-path callers.
- `crates/yserver/src/kms/vk/ops/{fill,copy,image,render,text,traps}.rs` — recorders' signature is unchanged (they already take a `vk::CommandBuffer`). What changes is the *caller* in `backend.rs`. The recorders gain in-frame barriers (write→read, layout-prep) that the old per-op drain hid.
- `crates/yserver/src/kms/vk/compositor.rs` — `record_and_present_composite` adds an `Option<vk::Semaphore>` wait-paint-done param; descriptor pool moves to the new ring; alloc-failure policy unified to "abort the composite for this output this frame, keep dirty, retry next cycle" (replaces today's partial-scene path at `compositor.rs:149`).
- `crates/yserver/src/kms/vk/{copy_scratch,mask_scratch,gradient,glyph,dst_readback}.rs` — replace per-op queue drain with `FrameScopedQueue` lifetime; `record_get_image` gets its own one-shot `VkFence`.
- `crates/yserver/src/kms/vk/{pipeline,render_pipeline,text_pipeline,logic_fill_pipeline,target}.rs` — annotate remaining teardown `queue_wait_idle`s as load-bearing.
- `crates/yserver/src/kms/backend.rs` — add `frame_pool: Option<FramePool>`, `composite_descriptor_ring: Option<CompositeDescriptorRing>`, `current_frame_id: Option<u32>`. Rewrite `composite_and_flip` so the C(F) set is computed *after* pageflip processing and *at flush time*. Add `flush_frame_and_composite` helper used by the legacy dispatch.
- `crates/yserver-core/` — no changes; existing `maybe_composite` hook is the quiescent point.

**Reference reads (no changes)**

- `crates/yserver/src/kms/vk/scanout.rs` — `BoPhase` state machine, `vk_flip_pending` skip stays.
- `crates/yserver/src/drm/page_flip.rs` — `submit_flip_with_fences` unchanged.

---

## Naming conventions used throughout this plan

- `FrameSlot` — per-frame state (cmd buffer, per-output semaphores/fences).
- `FramePool` — fixed-size ring of `FrameSlot`s (depth `N_FRAMES = 3`).
- `slot_id: usize` — `frame_id as usize % N_FRAMES`, the load-bearing identifier for retirement (wrap-safe).
- `frame_id: u32` — monotonic, wraps; used only for logging and ring indexing via `slot_id`.
- `OutputId` — opaque newtype wrapping the index into `KmsBackend::outputs`. Stable across hotplug (we never reuse an index for a different physical output within one process lifetime).
- `paint_done[slot, X]` — binary `VkSemaphore` for the slot-output pair; signalled by the slot's paint submit, waited by output X's composite submit.
- `composite_done[slot, X]` — binary `VkSemaphore`, export-configured for SYNC_FD; signalled by composite, consumed by KMS via `IN_FENCE_FD`.
- `paint_fence[slot]: VkFence` — CPU-side paint retirement.
- `composite_fence[slot, X]: VkFence` — CPU-side per-output composite retirement.
- `release_fence[slot, X]` — KMS `OUT_FENCE_PTR`, unchanged.
- `C(F)` — set of `OutputId`s that will actually composite this flush; computed at `flush_frame_and_composite` time, **after** processing any pageflip completions for this iteration.

---

## Pre-flight

- [ ] **P0.0: Create feature branch and baseline**

```bash
git checkout -b paint-sync-rework
cargo build -p yserver
cargo test -p yserver
just xts-yserver scenario=Xproto timeout=600 2>&1 | tee /tmp/xts-baseline.log
just rendercheck-yserver timeout=600 2>&1 | tee /tmp/rc-baseline.log
```

Expected: clean build; record xts5/rendercheck baseline for later parity checks. No commits yet.

---

## Phase P0 — verify external-semaphore export semantics

### Task P0.1: Capability probe + caps struct

**Files:**
- Create: `crates/yserver/src/kms/vk/external_sem_probe.rs`
- Modify: `crates/yserver/src/kms/vk/mod.rs`

- [ ] **Step 1: Failing unit test**

```rust
// crates/yserver/src/kms/vk/external_sem_probe.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_default_unsupported() {
        let caps = ExternalSemaphoreCaps::default();
        assert!(!caps.binary_sync_fd_exportable);
        assert!(!caps.timeline_sync_fd_exportable);
    }
}
```

- [ ] **Step 2: Run, verify it fails**

```bash
cargo test -p yserver kms::vk::external_sem_probe::tests::caps_default_unsupported
```

Expected: FAIL (type not defined).

- [ ] **Step 3: Add struct + probe**

```rust
//! Per-device capability probe for external-semaphore SYNC_FD export.
//! Spec: docs/superpowers/specs/2026-05-12-paint-composite-sync-design.md §P0.

use ash::vk;

#[derive(Debug, Default, Clone, Copy)]
pub struct ExternalSemaphoreCaps {
    pub binary_sync_fd_exportable: bool,
    pub timeline_sync_fd_exportable: bool,
}

/// Query per-device caps for `(BINARY|TIMELINE, SYNC_FD)` export.
pub fn probe(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> ExternalSemaphoreCaps {
    let mut caps = ExternalSemaphoreCaps::default();

    // Binary
    let info = vk::PhysicalDeviceExternalSemaphoreInfo::default()
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let mut props = vk::ExternalSemaphoreProperties::default();
    unsafe {
        instance.get_physical_device_external_semaphore_properties(
            physical_device, &info, &mut props,
        );
    }
    caps.binary_sync_fd_exportable = props
        .external_semaphore_features
        .contains(vk::ExternalSemaphoreFeatureFlags::EXPORTABLE);

    // Timeline
    let mut type_info = vk::SemaphoreTypeCreateInfo::default()
        .semaphore_type(vk::SemaphoreType::TIMELINE);
    let info = vk::PhysicalDeviceExternalSemaphoreInfo::default()
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
        .push_next(&mut type_info);
    let mut props = vk::ExternalSemaphoreProperties::default();
    unsafe {
        instance.get_physical_device_external_semaphore_properties(
            physical_device, &info, &mut props,
        );
    }
    caps.timeline_sync_fd_exportable = props
        .external_semaphore_features
        .contains(vk::ExternalSemaphoreFeatureFlags::EXPORTABLE);

    caps
}
```

- [ ] **Step 4: Wire module, run test**

Add `pub mod external_sem_probe;` to `kms/vk/mod.rs`. Run:

```bash
cargo test -p yserver kms::vk::external_sem_probe::tests
cargo clippy -p yserver -- -D warnings
```

Expected: PASS, clean.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/vk/external_sem_probe.rs crates/yserver/src/kms/vk/mod.rs
git commit -m "feat(vk): per-device external-semaphore SYNC_FD export probe"
```

---

### Task P0.2: Wire probe into `VkContext` + log caps at startup

**Files:**
- Modify: `crates/yserver/src/kms/vk/device.rs`

- [ ] **Step 1: Add field to `VkContext`**

Just below the existing `external_semaphore_fd` field:

```rust
    /// Per-device caps from `external_sem_probe::probe`. Drives whether
    /// the new sync model can run on this device.
    pub external_sem_caps: super::external_sem_probe::ExternalSemaphoreCaps,
```

- [ ] **Step 2: Populate during construction**

Find where `external_semaphore_fd` is built (around line 251). Add right after:

```rust
        let external_sem_caps =
            super::external_sem_probe::probe(&instance, physical_device);
        log::info!(
            "vk: external semaphore export caps: binary_sync_fd={} timeline_sync_fd={}",
            external_sem_caps.binary_sync_fd_exportable,
            external_sem_caps.timeline_sync_fd_exportable,
        );
```

Include `external_sem_caps` in the `VkContext { … }` literal.

- [ ] **Step 3: Build + verify on hardware**

```bash
cargo build -p yserver
RUST_LOG=info just yserver-headless 2>&1 | grep "external semaphore export caps" | head -1
```

Expected: a single info-level line. Record results in `docs/status.md` under a new "Sync rework — probe results" heading (RADV-Polaris, RADV-Renoir, lavapipe minimum).

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/vk/device.rs docs/status.md
git commit -m "feat(vk): carry external-semaphore caps on VkContext + log at startup"
```

---

### Task P0.3: End-to-end SYNC_FD smoke

**Files:**
- Create: `crates/yserver/tests/sync_fd_smoke.rs` (or in the existing integration-test crate if one exists; check `crates/yserver/Cargo.toml` for a `[[test]]` section that fits).

- [ ] **Step 1: Locate the integration-test crate**

```bash
ls /home/jos/Projects/yserver/crates/yserver/tests/ 2>/dev/null
grep -A 3 "\[\[test\]\]" /home/jos/Projects/yserver/crates/yserver/Cargo.toml | head -20
```

If no integration-test directory exists, create `crates/yserver/tests/sync_fd_smoke.rs`. If a kms-tagged feature exists (`kms-hw` or similar), use it to gate.

- [ ] **Step 2: Write the smoke test**

```rust
//! End-to-end smoke: binary SYNC_FD export from a real submit.
//! Spec P0.3.

#![cfg(feature = "kms-hw")] // adjust to the actual feature name if different

use ash::vk;

#[test]
#[ignore = "needs live Vulkan ICD on a KMS-capable session"]
fn binary_sync_fd_export_signals_after_submit() {
    let vk = /* construct VkContext via the same path the backend uses */;
    if !vk.external_sem_caps.binary_sync_fd_exportable {
        eprintln!("device does not support binary SYNC_FD export, skipping");
        return;
    }

    // 1) Create a binary semaphore.
    let sem_info = vk::SemaphoreCreateInfo::default();
    let sem = unsafe { vk.device.create_semaphore(&sem_info, None) }.unwrap();

    // 2) Allocate a no-op CB.
    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(vk.graphics_queue_family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    let pool = unsafe { vk.device.create_command_pool(&pool_info, None) }.unwrap();
    let alloc = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let cb = unsafe { vk.device.allocate_command_buffers(&alloc) }.unwrap()[0];
    unsafe {
        vk.device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).unwrap();
        vk.device.end_command_buffer(cb).unwrap();
    }

    // 3) Submit with signal.
    let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(cb)];
    let sig_info = [vk::SemaphoreSubmitInfo::default()
        .semaphore(sem)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
    let submit = [vk::SubmitInfo2::default()
        .command_buffer_infos(&cb_info)
        .signal_semaphore_infos(&sig_info)];
    unsafe {
        vk.device.queue_submit2(vk.graphics_queue, &submit, vk::Fence::null()).unwrap();
    }

    // 4) Export.
    let get_info = vk::SemaphoreGetFdInfoKHR::default()
        .semaphore(sem)
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let raw_fd = unsafe { vk.external_semaphore_fd.get_semaphore_fd(&get_info) }.unwrap();
    assert!(raw_fd >= 0, "expected valid fd");

    // 5) poll(POLLIN) with 1 s timeout — must be ready.
    let mut pollfd = libc::pollfd {
        fd: raw_fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let n = unsafe { libc::poll(&mut pollfd, 1, 1000) };
    assert_eq!(n, 1, "poll returned {n}");
    assert!(pollfd.revents & libc::POLLIN != 0);

    // 6) Cleanup.
    unsafe {
        libc::close(raw_fd);
        vk.device.destroy_command_pool(pool, None);
        vk.device.destroy_semaphore(sem, None);
    }
}

/// Stronger smoke per spec P0.3: hand the exported fd to KMS as
/// IN_FENCE_FD on a no-op atomic commit. Validates that the
/// explicit-sync handoff actually works on this device + DRM driver
/// combination, not just that the export-and-poll path is well-formed.
#[test]
#[ignore = "needs live VK ICD + KMS-capable session"]
fn binary_sync_fd_handoff_to_kms() {
    let vk = /* construct VkContext */;
    let drm = /* open the same DRM device the backend uses */;
    if !vk.external_sem_caps.binary_sync_fd_exportable {
        eprintln!("device does not support binary SYNC_FD export, skipping");
        return;
    }
    // Same setup as binary_sync_fd_export_signals_after_submit through
    // step 4. Then: instead of poll(), pass the fd to an atomic commit
    // with IN_FENCE_FD set. Use a no-op atomic commit (PROPERTY only,
    // no plane change) so the test does not depend on a connected
    // display, OR use the existing `submit_flip_with_fences` path
    // against the current scanout BO if the test rig has a display.
    // Assert the commit succeeds and the fd is consumed (kernel takes
    // ownership on success).
}
```

(The `/* construct VkContext */` and `/* open DRM device */` placeholders are *not* hand-waves: the existing test harness for KMS likely has helpers. If not, factor the device-init paths in `device.rs` and `drm/mod.rs` into reusable `for_tests()` constructors that take the same env config as the backend.)

- [ ] **Step 3: Run on each available driver**

```bash
cargo test -p yserver --test sync_fd_smoke --features kms-hw -- --ignored
```

Repeat for each driver you can reach (RADV-Polaris, RADV-Renoir, lavapipe). Document pass/fail per device in `docs/status.md`. If RADV-Polaris fails — **stop**, the architecture's primary path is dead on that device.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/tests/sync_fd_smoke.rs docs/status.md
git commit -m "test(vk): SYNC_FD export end-to-end smoke (#[ignore])"
```

---

## Phase P1 — scaffolding

### Task P1.1: `bounded_wait` helper

**Files:**
- Create: `crates/yserver/src/kms/vk/bounded_wait.rs`
- Modify: `crates/yserver/src/kms/vk/mod.rs`

- [ ] **Step 1: Failing test for timeout error mapping**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn err_is_distinct_per_variant() {
        let a = WaitErr::Timeout;
        let b = WaitErr::DeviceLost;
        let c = WaitErr::Other(0);
        assert!(matches!(a, WaitErr::Timeout));
        assert!(matches!(b, WaitErr::DeviceLost));
        assert!(matches!(c, WaitErr::Other(_)));
    }
}
```

- [ ] **Step 2: Run, fail**

```bash
cargo test -p yserver kms::vk::bounded_wait::tests::err_is_distinct_per_variant
```

Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
//! Bounded `vkWaitForFences` helper. Spec v5 §"Device-lost / GPU
//! hang" — every hot-path fence wait uses a ≤250 ms timeout and
//! distinguishes TIMEOUT from DEVICE_LOST so the caller can decide
//! between "stall warning" and "fatal/reinit".

use ash::vk;

use super::device::VkContext;

pub const HOT_PATH_TIMEOUT_NS: u64 = 250_000_000; // 250 ms

#[derive(Debug)]
pub enum WaitErr {
    /// Did not signal within the bounded timeout. Caller decides:
    /// usually a stall warning + try again next iteration.
    Timeout,
    /// VK_ERROR_DEVICE_LOST. Backend transitions to fatal.
    DeviceLost,
    /// Other vk::Result.
    Other(i32),
}

pub fn wait_for_fences_bounded(
    vk: &VkContext,
    fences: &[vk::Fence],
) -> Result<(), WaitErr> {
    if fences.is_empty() {
        return Ok(());
    }
    match unsafe {
        vk.device.wait_for_fences(fences, true, HOT_PATH_TIMEOUT_NS)
    } {
        Ok(()) => Ok(()),
        Err(vk::Result::TIMEOUT) => Err(WaitErr::Timeout),
        Err(vk::Result::ERROR_DEVICE_LOST) => Err(WaitErr::DeviceLost),
        Err(e) => Err(WaitErr::Other(e.as_raw())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn err_is_distinct_per_variant() {
        let a = WaitErr::Timeout;
        let b = WaitErr::DeviceLost;
        let c = WaitErr::Other(0);
        assert!(matches!(a, WaitErr::Timeout));
        assert!(matches!(b, WaitErr::DeviceLost));
        assert!(matches!(c, WaitErr::Other(_)));
    }
}
```

- [ ] **Step 4: Wire + test + clippy**

```bash
cargo test -p yserver kms::vk::bounded_wait
cargo clippy -p yserver -- -D warnings
```

Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/vk/bounded_wait.rs crates/yserver/src/kms/vk/mod.rs
git commit -m "feat(vk): bounded fence wait helper with timeout/device-lost split"
```

---

### Task P1.2: Fence pool

**Files:**
- Create: `crates/yserver/src/kms/vk/fence_pool.rs`
- Modify: `crates/yserver/src/kms/vk/mod.rs`

- [ ] **Step 1: Failing test for state machine**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_default_empty() {
        let s = FencePoolState::default();
        assert_eq!(s.free_len(), 0);
    }

    #[test]
    fn push_pop_roundtrip() {
        let mut s = FencePoolState::default();
        s.push_free(0xdeadbeef);
        assert_eq!(s.pop_free(), Some(0xdeadbeef));
        assert_eq!(s.pop_free(), None);
    }
}
```

- [ ] **Step 2: Run, fail**

```bash
cargo test -p yserver kms::vk::fence_pool::tests
```

Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
//! Pool of `VkFence` handles for CPU-side retirement. Binary
//! semaphores carry GPU↔GPU ordering; fences carry the "host can
//! recycle frame F's resources" signal. Spec v5 §"Frame lifetime".

use ash::vk;
use std::sync::Arc;

use super::device::VkContext;

#[derive(Debug, Default)]
pub struct FencePoolState {
    free: Vec<u64>,
}

impl FencePoolState {
    pub fn free_len(&self) -> usize { self.free.len() }
    pub fn push_free(&mut self, raw: u64) { self.free.push(raw); }
    pub fn pop_free(&mut self) -> Option<u64> { self.free.pop() }
}

pub struct FencePool {
    vk: Arc<VkContext>,
    state: FencePoolState,
}

impl FencePool {
    pub fn new(vk: Arc<VkContext>) -> Self {
        Self { vk, state: FencePoolState::default() }
    }

    /// Acquire an unsignalled `VkFence`. Reuses+resets a freed one;
    /// otherwise creates fresh.
    pub fn acquire(&mut self) -> Result<vk::Fence, vk::Result> {
        if let Some(raw) = self.state.pop_free() {
            let f = vk::Fence::from_raw(raw);
            unsafe { self.vk.device.reset_fences(&[f])? };
            return Ok(f);
        }
        let info = vk::FenceCreateInfo::default();
        unsafe { self.vk.device.create_fence(&info, None) }
    }

    /// Release. Caller must have observed it signalled (or be teardown).
    pub fn release(&mut self, f: vk::Fence) {
        self.state.push_free(f.as_raw());
    }
}

impl Drop for FencePool {
    fn drop(&mut self) {
        // LOAD-BEARING: device_wait_idle at teardown is correct;
        // any in-flight reference to a pool fence has now completed.
        unsafe {
            let _ = self.vk.device.device_wait_idle();
            for raw in self.state.free.drain(..) {
                self.vk.device.destroy_fence(vk::Fence::from_raw(raw), None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_default_empty() {
        let s = FencePoolState::default();
        assert_eq!(s.free_len(), 0);
    }

    #[test]
    fn push_pop_roundtrip() {
        let mut s = FencePoolState::default();
        s.push_free(0xdeadbeef);
        assert_eq!(s.pop_free(), Some(0xdeadbeef));
        assert_eq!(s.pop_free(), None);
    }
}
```

- [ ] **Step 4: Wire, test, clippy, commit**

```bash
cargo test -p yserver kms::vk::fence_pool
cargo clippy -p yserver -- -D warnings
git add crates/yserver/src/kms/vk/fence_pool.rs crates/yserver/src/kms/vk/mod.rs
git commit -m "feat(vk): fence pool for CPU-side frame retirement"
```

---

### Task P1.3: `(slot, output)`-keyed semaphore pool

**Files:**
- Create: `crates/yserver/src/kms/vk/semaphore_pool.rs`
- Modify: `crates/yserver/src/kms/vk/mod.rs`

The spec is explicit (v5): each `(slot_id, output_id)` pair owns a distinct semaphore handle. We need two parallel pools — one for `paint_done` (no export-config), one for `composite_done` (export-configured for SYNC_FD).

- [ ] **Step 1: Failing test for the key**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_pair() {
        let a = SemKey { slot: 0, output: OutputId(1) };
        let b = SemKey { slot: 0, output: OutputId(1) };
        let c = SemKey { slot: 1, output: OutputId(1) };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn state_acquire_lazy_create() {
        let mut s = SemaphorePoolState::default();
        let k = SemKey { slot: 0, output: OutputId(2) };
        assert!(s.get(k).is_none());
        s.insert(k, SemaphoreHandle(0x42));
        assert_eq!(s.get(k), Some(SemaphoreHandle(0x42)));
    }
}
```

- [ ] **Step 2: Run, fail**

```bash
cargo test -p yserver kms::vk::semaphore_pool::tests
```

Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
//! `(slot, output)`-keyed binary `VkSemaphore` pools.
//!
//! Spec v5 mandates each (frame_slot, output) pair own a distinct
//! semaphore handle — sharing across in-flight slots violates binary
//! cardinality. Two parallel pools live on `FramePool`: one for
//! `paint_done` (no export-config), one for `composite_done`
//! (`VkExportSemaphoreCreateInfo{SYNC_FD}`).

use ash::vk;
use std::{collections::HashMap, sync::Arc};

use super::device::VkContext;

/// Stable index into `KmsBackend::outputs`. Wrapped so callers can't
/// confuse it with a layout index or display ID.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OutputId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SemKey {
    pub slot: usize,
    pub output: OutputId,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SemaphoreHandle(pub u64);

#[derive(Debug, Default)]
pub struct SemaphorePoolState {
    handles: HashMap<SemKey, SemaphoreHandle>,
}

impl SemaphorePoolState {
    pub fn get(&self, k: SemKey) -> Option<SemaphoreHandle> {
        self.handles.get(&k).copied()
    }
    pub fn insert(&mut self, k: SemKey, h: SemaphoreHandle) {
        self.handles.insert(k, h);
    }
    pub fn drain_all(&mut self) -> Vec<SemaphoreHandle> {
        self.handles.drain().map(|(_, h)| h).collect()
    }
}

/// Whether semaphores from this pool are export-configured.
#[derive(Debug, Clone, Copy)]
pub enum PoolExport {
    None,
    SyncFd,
}

pub struct SemaphorePool {
    vk: Arc<VkContext>,
    export: PoolExport,
    state: SemaphorePoolState,
}

impl SemaphorePool {
    pub fn new(vk: Arc<VkContext>, export: PoolExport) -> Self {
        Self { vk, export, state: SemaphorePoolState::default() }
    }

    /// Acquire the handle for `(slot, output)`, lazily creating a
    /// fresh `VkSemaphore` if it doesn't exist. The handle's lifetime
    /// is tied to the slot: it stays valid until `release_slot` is
    /// called (frame retired) — see `FramePool::retire_slot`.
    pub fn acquire(&mut self, k: SemKey) -> Result<vk::Semaphore, vk::Result> {
        if let Some(h) = self.state.get(k) {
            return Ok(vk::Semaphore::from_raw(h.0));
        }
        let sem = self.create()?;
        self.state.insert(k, SemaphoreHandle(sem.as_raw()));
        Ok(sem)
    }

    fn create(&self) -> Result<vk::Semaphore, vk::Result> {
        match self.export {
            PoolExport::None => {
                let info = vk::SemaphoreCreateInfo::default();
                unsafe { self.vk.device.create_semaphore(&info, None) }
            }
            PoolExport::SyncFd => {
                let mut ex = vk::ExportSemaphoreCreateInfo::default()
                    .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
                let info = vk::SemaphoreCreateInfo::default().push_next(&mut ex);
                unsafe { self.vk.device.create_semaphore(&info, None) }
            }
        }
    }
}

impl Drop for SemaphorePool {
    fn drop(&mut self) {
        // LOAD-BEARING: device_wait_idle at teardown.
        unsafe {
            let _ = self.vk.device.device_wait_idle();
            for h in self.state.drain_all() {
                self.vk.device.destroy_semaphore(vk::Semaphore::from_raw(h.0), None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_pair() {
        let a = SemKey { slot: 0, output: OutputId(1) };
        let b = SemKey { slot: 0, output: OutputId(1) };
        let c = SemKey { slot: 1, output: OutputId(1) };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn state_acquire_lazy_create() {
        let mut s = SemaphorePoolState::default();
        let k = SemKey { slot: 0, output: OutputId(2) };
        assert!(s.get(k).is_none());
        s.insert(k, SemaphoreHandle(0x42));
        assert_eq!(s.get(k), Some(SemaphoreHandle(0x42)));
    }
}
```

- [ ] **Step 4: Wire, test, clippy, commit**

```bash
cargo test -p yserver kms::vk::semaphore_pool
cargo clippy -p yserver -- -D warnings
git add crates/yserver/src/kms/vk/semaphore_pool.rs crates/yserver/src/kms/vk/mod.rs
git commit -m "feat(vk): (slot, output)-keyed binary semaphore pools"
```

---

### Task P1.4: `FrameScopedQueue` for per-op scratch lifetime

**Files:**
- Create: `crates/yserver/src/kms/vk/frame_resource.rs`
- Modify: `crates/yserver/src/kms/vk/mod.rs`

- [ ] **Step 1: Failing test for slot-based retirement (wrap-safe)**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_retired_returns_only_old_slots() {
        let mut q: FrameScopedQueue<u32> = FrameScopedQueue::default();
        q.push(0, 100);
        q.push(1, 200);
        q.push(2, 500);
        // "retired slots" = {0, 1}; slot 2 still in flight.
        let drained = q.drain_retired(&[true, true, false]);
        assert_eq!(drained, vec![100, 200]);
        let drained = q.drain_retired(&[true, true, true]);
        assert_eq!(drained, vec![500]);
    }
}
```

- [ ] **Step 2: Run, fail**

```bash
cargo test -p yserver kms::vk::frame_resource::tests
```

Expected: FAIL.

- [ ] **Step 3: Implement (slot-indexed, not frame-id-indexed — wrap-safe by design)**

```rust
//! Resources whose lifetime is tied to a frame slot. Spec v5 §"Frame
//! lifetime". Slot-indexed so `u32::wrap` of frame_id never matters
//! for retirement ordering.

use std::collections::VecDeque;

pub struct FrameScopedQueue<T> {
    pending: VecDeque<(usize, T)>, // (slot_id, item)
}

impl<T> Default for FrameScopedQueue<T> {
    fn default() -> Self {
        Self { pending: VecDeque::new() }
    }
}

impl<T> FrameScopedQueue<T> {
    pub fn push(&mut self, slot_id: usize, item: T) {
        self.pending.push_back((slot_id, item));
    }

    /// Drain everything whose owning slot is retired. `retired_slots`
    /// is a per-slot boolean indexed by `slot_id`, length == N_FRAMES.
    pub fn drain_retired(&mut self, retired_slots: &[bool]) -> Vec<T> {
        let mut keep = VecDeque::new();
        let mut out = Vec::new();
        while let Some((slot, item)) = self.pending.pop_front() {
            if retired_slots.get(slot).copied().unwrap_or(false) {
                out.push(item);
            } else {
                keep.push_back((slot, item));
            }
        }
        self.pending = keep;
        out
    }

    pub fn len(&self) -> usize { self.pending.len() }
    pub fn is_empty(&self) -> bool { self.pending.is_empty() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_retired_returns_only_old_slots() {
        let mut q: FrameScopedQueue<u32> = FrameScopedQueue::default();
        q.push(0, 100);
        q.push(1, 200);
        q.push(2, 500);
        let drained = q.drain_retired(&[true, true, false]);
        assert_eq!(drained, vec![100, 200]);
        let drained = q.drain_retired(&[true, true, true]);
        assert_eq!(drained, vec![500]);
    }

    #[test]
    fn empty_pool_no_drain() {
        let mut q: FrameScopedQueue<u32> = FrameScopedQueue::default();
        assert_eq!(q.drain_retired(&[true; 3]), Vec::<u32>::new());
    }
}
```

- [ ] **Step 4: Wire, test, clippy, commit**

```bash
cargo test -p yserver kms::vk::frame_resource
cargo clippy -p yserver -- -D warnings
git add crates/yserver/src/kms/vk/frame_resource.rs crates/yserver/src/kms/vk/mod.rs
git commit -m "feat(vk): FrameScopedQueue with slot-based wrap-safe retirement"
```

---

### Task P1.5: `FrameSlot` + `FramePool`

**Files:**
- Create: `crates/yserver/src/kms/vk/frame.rs`
- Modify: `crates/yserver/src/kms/vk/mod.rs`

- [ ] **Step 1: Failing tests for the slot phase machine**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_default_idle() {
        assert_eq!(SlotPhase::default(), SlotPhase::Idle);
    }

    #[test]
    fn slot_transitions() {
        let mut s = SlotPhase::default();
        s.begin_recording();
        assert_eq!(s, SlotPhase::Recording);
        s.mark_submitted();
        assert_eq!(s, SlotPhase::Submitted);
        s.mark_retired();
        assert_eq!(s, SlotPhase::Idle);
    }
}
```

- [ ] **Step 2: Run, fail**

```bash
cargo test -p yserver kms::vk::frame::tests
```

Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
//! Per-frame state for the paint-accumulate-then-flush model.
//! Spec v5 §"Frame lifetime".

use ash::vk;
use std::{collections::HashMap, sync::Arc};

use super::{
    device::VkContext,
    fence_pool::FencePool,
    semaphore_pool::{OutputId, PoolExport, SemKey, SemaphorePool},
};

pub const N_FRAMES: usize = 3;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SlotPhase {
    #[default]
    Idle,
    Recording,
    Submitted,
}

impl SlotPhase {
    pub fn begin_recording(&mut self) {
        debug_assert_eq!(*self, SlotPhase::Idle);
        *self = SlotPhase::Recording;
    }
    pub fn mark_submitted(&mut self) {
        debug_assert_eq!(*self, SlotPhase::Recording);
        *self = SlotPhase::Submitted;
    }
    pub fn mark_retired(&mut self) {
        debug_assert!(matches!(*self, SlotPhase::Submitted | SlotPhase::Idle));
        *self = SlotPhase::Idle;
    }
}

pub struct FrameSlot {
    pub slot_id: usize,
    pub cmd_pool: vk::CommandPool,
    pub cmd_buffer: vk::CommandBuffer,
    pub paint_fence: vk::Fence,
    pub phase: SlotPhase,
    /// `frame_id` last recorded into this slot. Used for logging only.
    pub frame_id: u32,
    /// Outputs in C(F) for the frame this slot currently holds.
    /// Populated at flush time. Used by retirement to know which
    /// composite fences must signal before the slot can be reused.
    pub c_of_f: Vec<OutputId>,
    /// Composite fences keyed by output, populated at composite-submit time.
    pub composite_fence: HashMap<OutputId, vk::Fence>,
}

pub struct FramePool {
    vk: Arc<VkContext>,
    pub slots: Vec<FrameSlot>,
    pub next_frame_id: u32,
    pub paint_done: SemaphorePool,    // PoolExport::None
    pub composite_done: SemaphorePool, // PoolExport::SyncFd
    pub fences: FencePool,
}

impl FramePool {
    pub fn new(vk: Arc<VkContext>) -> Result<Self, vk::Result> {
        let mut slots = Vec::with_capacity(N_FRAMES);
        for slot_id in 0..N_FRAMES {
            let pool_info = vk::CommandPoolCreateInfo::default()
                .queue_family_index(vk.graphics_queue_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
            let cmd_pool = unsafe { vk.device.create_command_pool(&pool_info, None)? };
            let alloc = vk::CommandBufferAllocateInfo::default()
                .command_pool(cmd_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            let cmd_buffer = unsafe { vk.device.allocate_command_buffers(&alloc)?[0] };

            let fence_info = vk::FenceCreateInfo::default();
            let paint_fence = unsafe { vk.device.create_fence(&fence_info, None)? };

            slots.push(FrameSlot {
                slot_id,
                cmd_pool,
                cmd_buffer,
                paint_fence,
                phase: SlotPhase::default(),
                frame_id: 0,
                c_of_f: Vec::new(),
                composite_fence: HashMap::new(),
            });
        }

        Ok(Self {
            vk: vk.clone(),
            slots,
            next_frame_id: 0,
            paint_done: SemaphorePool::new(vk.clone(), PoolExport::None),
            composite_done: SemaphorePool::new(vk.clone(), PoolExport::SyncFd),
            fences: FencePool::new(vk),
        })
    }

    /// `slot_id` for the next frame (does not bump). Use `alloc_frame_id`
    /// to bump.
    pub fn slot_id_for_next(&self) -> usize {
        (self.next_frame_id as usize) % N_FRAMES
    }

    pub fn alloc_frame_id(&mut self) -> (u32, usize) {
        let id = self.next_frame_id;
        let slot = (id as usize) % N_FRAMES;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);
        (id, slot)
    }

    pub fn paint_done_for(&mut self, slot: usize, out: OutputId)
        -> Result<vk::Semaphore, vk::Result>
    {
        self.paint_done.acquire(SemKey { slot, output: out })
    }

    pub fn composite_done_for(&mut self, slot: usize, out: OutputId)
        -> Result<vk::Semaphore, vk::Result>
    {
        self.composite_done.acquire(SemKey { slot, output: out })
    }
}

impl Drop for FramePool {
    fn drop(&mut self) {
        // LOAD-BEARING: device_wait_idle at teardown.
        unsafe {
            let _ = self.vk.device.device_wait_idle();
            for s in &self.slots {
                self.vk.device.destroy_fence(s.paint_fence, None);
                for (_, f) in &s.composite_fence {
                    self.vk.device.destroy_fence(*f, None);
                }
                self.vk.device.destroy_command_pool(s.cmd_pool, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_default_idle() {
        assert_eq!(SlotPhase::default(), SlotPhase::Idle);
    }

    #[test]
    fn slot_transitions() {
        let mut s = SlotPhase::default();
        s.begin_recording();
        assert_eq!(s, SlotPhase::Recording);
        s.mark_submitted();
        assert_eq!(s, SlotPhase::Submitted);
        s.mark_retired();
        assert_eq!(s, SlotPhase::Idle);
    }
}
```

- [ ] **Step 4: Wire, test, clippy, commit**

```bash
cargo test -p yserver kms::vk::frame::tests
cargo clippy -p yserver -- -D warnings
git add crates/yserver/src/kms/vk/frame.rs crates/yserver/src/kms/vk/mod.rs
git commit -m "feat(vk): FrameSlot + FramePool with (slot, output)-keyed pools"
```

---

### Task P1.6: Compositor descriptor ring (Tier 1b)

**Goal:** Replace `CompositorPipeline`'s single shared `descriptor_pool` (reset at the start of every composite pass) with a `(slot, output)`-keyed ring. Multiple per-output composites can be in flight simultaneously; resetting the shared pool would invalidate sets in flight.

**Files:**
- Create: `crates/yserver/src/kms/vk/composite_descriptor_ring.rs`
- Modify: `crates/yserver/src/kms/vk/pipeline.rs` (`CompositorPipeline`)
- Modify: `crates/yserver/src/kms/vk/compositor.rs` (`record_and_present_composite`)
- Modify: `crates/yserver/src/kms/vk/mod.rs`

- [ ] **Step 1: Failing test for ring key**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::vk::semaphore_pool::OutputId;

    #[test]
    fn key_lookup_distinct() {
        let a = RingKey { slot: 0, output: OutputId(0) };
        let b = RingKey { slot: 0, output: OutputId(1) };
        assert_ne!(a, b);
    }
}
```

- [ ] **Step 2: Run, fail**

```bash
cargo test -p yserver kms::vk::composite_descriptor_ring::tests
```

Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
//! Tier 1b compositor descriptor ring.
//! Spec v5 §"Tier 1b — composite pipeline's own descriptor pool".

use ash::vk;
use std::{collections::HashMap, sync::Arc};

use super::{device::VkContext, semaphore_pool::OutputId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RingKey {
    pub slot: usize,
    pub output: OutputId,
}

pub struct CompositeDescriptorRing {
    vk: Arc<VkContext>,
    /// Pools keyed on (slot, output). Created lazily when a new
    /// output joins C(F) for a slot that hasn't seen it before.
    pools: HashMap<RingKey, vk::DescriptorPool>,
    /// Pool size policy used for new pools — sum of expected per-
    /// frame descriptors per output's composite pass.
    pool_sizes: Vec<vk::DescriptorPoolSize>,
    max_sets: u32,
}

impl CompositeDescriptorRing {
    pub fn new(
        vk: Arc<VkContext>,
        pool_sizes: Vec<vk::DescriptorPoolSize>,
        max_sets: u32,
    ) -> Self {
        Self { vk, pools: HashMap::new(), pool_sizes, max_sets }
    }

    /// Ensure a pool exists for (slot, output). Returns the pool.
    /// Allocation failure is propagated up — caller must abort the
    /// composite for this output before recording any commands.
    pub fn ensure_pool(&mut self, key: RingKey) -> Result<vk::DescriptorPool, vk::Result> {
        if let Some(p) = self.pools.get(&key) {
            return Ok(*p);
        }
        let info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(self.max_sets)
            .pool_sizes(&self.pool_sizes);
        let pool = unsafe { self.vk.device.create_descriptor_pool(&info, None)? };
        self.pools.insert(key, pool);
        Ok(pool)
    }

    /// Reset the pool for (slot, output). Caller must have observed
    /// `composite_fence[slot, output]` signalled.
    pub fn reset(&self, key: RingKey) -> Result<(), vk::Result> {
        let Some(&pool) = self.pools.get(&key) else {
            return Ok(());
        };
        unsafe {
            self.vk.device.reset_descriptor_pool(
                pool,
                vk::DescriptorPoolResetFlags::empty(),
            )
        }
    }

    /// Drop pools for outputs that have been removed (hotplug).
    /// **Caller must guarantee no in-flight composite references this
    /// output's pools** — typically by calling `vkDeviceWaitIdle` or
    /// by waiting on every `composite_fence[*, out]` first. This is a
    /// hotplug-rare path; the simple `device_wait_idle` is cheap
    /// enough given the rarity. Codex round-4 finding.
    pub fn drop_output(&mut self, out: OutputId) {
        unsafe {
            let _ = self.vk.device.device_wait_idle();
        }
        let keys_to_drop: Vec<RingKey> = self.pools.keys()
            .filter(|k| k.output == out)
            .copied()
            .collect();
        for k in keys_to_drop {
            if let Some(p) = self.pools.remove(&k) {
                unsafe { self.vk.device.destroy_descriptor_pool(p, None) };
            }
        }
    }
}

impl Drop for CompositeDescriptorRing {
    fn drop(&mut self) {
        unsafe {
            let _ = self.vk.device.device_wait_idle();
            for (_, p) in self.pools.drain() {
                self.vk.device.destroy_descriptor_pool(p, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::vk::semaphore_pool::OutputId;

    #[test]
    fn key_lookup_distinct() {
        let a = RingKey { slot: 0, output: OutputId(0) };
        let b = RingKey { slot: 0, output: OutputId(1) };
        assert_ne!(a, b);
    }
}
```

- [ ] **Step 4: Update `CompositorPipeline` to use external pool**

The pipeline today owns the descriptor pool. Refactor so it owns *only* the pipeline / layout / sampler, and `allocate_descriptor_for_view` takes a `&vk::DescriptorPool` argument supplied by the ring caller.

```bash
grep -n "descriptor_pool\|allocate_descriptor_for_view\|reset_descriptors" /home/jos/Projects/yserver/crates/yserver/src/kms/vk/pipeline.rs | head
```

Walk through each spot:
- `CompositorPipeline::new`: stop creating the pool internally.
- `allocate_descriptor_for_view(&self, view)` becomes `allocate_descriptor_for_view(&self, pool: vk::DescriptorPool, view)`.
- `reset_descriptors` is removed (the ring resets at retirement time).

- [ ] **Step 5: Update `record_and_present_composite` to take a `pool` parameter**

```rust
pub fn record_and_present_composite(
    vk: &VkContext,
    drm: &DrmDevice,
    output: &Output,
    bo: &mut ScanoutBo,
    pipeline: &CompositorPipeline,
    descriptor_pool: vk::DescriptorPool,  // from the ring
    scene: &CompositeScene,
    wait_paint_done: Option<vk::Semaphore>,
) -> Result<(), PresentError>
```

Inside, replace `pipeline.reset_descriptors()` with nothing (ring handles reset). Replace `pipeline.allocate_descriptor_for_view(...)` with `pipeline.allocate_descriptor_for_view(descriptor_pool, ...)`.

**Allocation-failure policy change.** Today (`compositor.rs:149`) on alloc failure it logs and produces a partial scene. New policy: abort the whole composite, return `PresentError::Vk(OUT_OF_POOL_MEMORY)`, caller (the backend's `try_vulkan_composite_flip`) keeps the output dirty and tries again next cycle. Add this as an explicit `return Err(...)` instead of `break`.

- [ ] **Step 6: Update every caller of `record_and_present_composite`**

```bash
grep -rn "record_and_present_composite" /home/jos/Projects/yserver/crates/ 2>/dev/null
```

Each call site passes a freshly-`ensure_pool`'d `vk::DescriptorPool` from the new ring (use a sentinel `slot=0, output=current_output` until the new path wires through). The legacy callers still work because the descriptor pool argument is just plumbed in.

- [ ] **Step 7: Build, run existing tests**

```bash
cargo build -p yserver
cargo test -p yserver
cargo clippy -p yserver -- -D warnings
```

Expected: clean. xts5/rendercheck should be unchanged (we haven't touched paint yet).

- [ ] **Step 8: Commit**

```bash
git add crates/yserver/src/kms/vk/
git commit -m "refactor(vk): compositor descriptor ring (Tier 1b) replaces single shared pool"
```

---

### Task P1.7: Wire `FramePool` + ring onto `KmsBackend`; add `flush_frame` + retirement helpers

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`

This task makes three coupled changes that must land together: (a) introduce per-output dirty tracking to replace the global `screen_dirty: bool`; (b) factor `retire_slot(slot_id)` into a helper so both `open_frame` and `compose_catch_up_outputs` use the same path; (c) make `submit_composite_for_output` transactional — the composite_fence is only registered into `slot.composite_fence` on submit success.

- [ ] **Step 1: Add fields to `KmsBackend` (per-output dirty included)**

```rust
    pub(crate) frame_pool: Option<crate::kms::vk::frame::FramePool>,
    pub(crate) composite_descriptor_ring:
        Option<crate::kms::vk::composite_descriptor_ring::CompositeDescriptorRing>,
    /// `Some(slot_id)` while a frame is open (paint ops appending,
    /// not yet flushed).
    pub(crate) current_slot: Option<usize>,
    /// Per-output dirty flag — replaces the global `screen_dirty`.
    /// Set by `mark_dirty()` (for all outputs) or `mark_output_dirty(out)`.
    /// Cleared per-output when that output's composite completes
    /// successfully. Spec v5 §"Skipped-output rule" depends on this:
    /// a vk_flip_pending output stays dirty so its catch-up composite
    /// fires on the next iteration.
    pub(crate) output_dirty: std::collections::HashMap<
        crate::kms::vk::semaphore_pool::OutputId, bool,
    >,
```

**`screen_dirty: bool` is removed in this commit.** Update every caller:

```bash
grep -n "screen_dirty\|mark_dirty\b" /home/jos/Projects/yserver/crates/yserver/src/kms/backend.rs
```

Replace `mark_dirty(&mut self)` to set every present output's dirty bit; add `mark_output_dirty(out)` for paint sites that know which window/output they affected (use `mark_dirty()` everywhere first; refinement is a follow-up). Replace `if !self.screen_dirty { return Ok(()) }` in `composite_and_flip` with `if !self.is_dirty_anywhere() && self.current_slot.is_none() { return Ok(()) }`.

Init all three at construction; build the ring with a generous initial pool size (will be tuned in P2/P3 with real numbers):

```rust
let ring = crate::kms::vk::composite_descriptor_ring::CompositeDescriptorRing::new(
    vkctx.clone(),
    vec![vk::DescriptorPoolSize {
        ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
        descriptor_count: 1024,
    }],
    1024,
);
```

For tests / non-VK init paths set all three to `None`.

- [ ] **Step 2: Add `retire_slot` helper (used by both `open_frame` and catch-up)**

```rust
/// Retire `slot_id` if it's in Submitted phase. Waits on paint_fence
/// + every composite_fence in the slot's C(F), resets them, resets
/// per-(slot, output) descriptor pool ring entries, clears the slot's
/// C(F), and transitions to Idle. Idempotent on already-Idle slots.
/// Returns Err on timeout or device-lost — caller decides policy.
///
/// Reset errors are **propagated**, not swallowed: a failed reset can
/// poison descriptor reuse (codex round-4 finding).
fn retire_slot(&mut self, slot_id: usize) -> Result<(), vk::Result> {
    let pool = self.frame_pool.as_mut().expect("frame_pool absent");
    let slot = &mut pool.slots[slot_id];
    if slot.phase != SlotPhase::Submitted {
        return Ok(());
    }

    // Collect every fence this slot is gated on.
    let mut to_wait: Vec<vk::Fence> = Vec::with_capacity(1 + slot.c_of_f.len());
    to_wait.push(slot.paint_fence);
    for out in &slot.c_of_f {
        if let Some(&f) = slot.composite_fence.get(out) {
            to_wait.push(f);
        }
    }

    match crate::kms::vk::bounded_wait::wait_for_fences_bounded(&pool.vk, &to_wait) {
        Ok(()) => {}
        Err(crate::kms::vk::bounded_wait::WaitErr::Timeout) => {
            log::warn!("retire_slot {slot_id}: timed out after 250 ms (GPU stalled?)");
            return Err(vk::Result::TIMEOUT);
        }
        Err(crate::kms::vk::bounded_wait::WaitErr::DeviceLost) => {
            log::error!("retire_slot {slot_id}: DEVICE_LOST");
            // Spec v5 §"Device-lost / GPU hang": fatal path.
            std::process::exit(1);
        }
        Err(crate::kms::vk::bounded_wait::WaitErr::Other(e)) => {
            return Err(vk::Result::from_raw(e));
        }
    }

    // Reset fences. paint_fence first; then each composite_fence (and
    // release it back to the pool so the next composite for this slot
    // gets a fresh one).
    unsafe { pool.vk.device.reset_fences(&[slot.paint_fence])? };
    let outs: Vec<crate::kms::vk::semaphore_pool::OutputId> = slot.c_of_f.clone();
    for out in &outs {
        if let Some(f) = slot.composite_fence.remove(out) {
            unsafe { pool.vk.device.reset_fences(&[f])? };
            pool.fences.release(f);
        }
    }

    // Reset descriptor ring entries.
    if let Some(ring) = self.composite_descriptor_ring.as_mut() {
        for out in &outs {
            ring.reset(crate::kms::vk::composite_descriptor_ring::RingKey {
                slot: slot_id, output: *out,
            })?;  // propagate, don't swallow.
        }
    }

    let slot = &mut self.frame_pool.as_mut().unwrap().slots[slot_id];
    slot.c_of_f.clear();
    slot.phase.mark_retired();
    Ok(())
}
```

- [ ] **Step 3: `open_frame` uses `retire_slot`**

```rust
/// Open a frame if one isn't already open. Returns the slot's CB and
/// slot id. The first paint op of a frame calls this.
fn open_frame(&mut self) -> Result<(usize, vk::CommandBuffer), vk::Result> {
    if let Some(slot) = self.current_slot {
        let pool = self.frame_pool.as_ref().expect("frame_pool absent");
        return Ok((slot, pool.slots[slot].cmd_buffer));
    }
    let slot_id = self.frame_pool.as_ref().unwrap().slot_id_for_next();
    self.retire_slot(slot_id)?;

    let pool = self.frame_pool.as_mut().unwrap();
    let (frame_id, _) = pool.alloc_frame_id();
    let slot = &mut pool.slots[slot_id];
    slot.frame_id = frame_id;
    let cb = slot.cmd_buffer;
    unsafe {
        pool.vk.device.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty())?;
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        pool.vk.device.begin_command_buffer(cb, &begin)?;
    }
    slot.phase.begin_recording();
    self.current_slot = Some(slot_id);
    Ok((slot_id, cb))
}
```

- [ ] **Step 3: Add `flush_frame_and_composite` method**

This is the load-bearing helper: it submits paint (with C(F)-fan-out signals) and then per-output composites in one pass.

```rust
/// Flush the open frame: end the paint CB, submit it with the
/// per-output paint_done fan-out, then submit a composite per output
/// in C(F). C(F) is computed *now* (after pageflip-completion has
/// updated vk_flip_pending state). Returns Ok(()) even if some
/// outputs were skipped — the dirty flag stays set so they retry.
pub(crate) fn flush_frame_and_composite(&mut self) -> Result<(), vk::Result> {
    let Some(slot_id) = self.current_slot.take() else {
        // No paint pending. Still may need to compose dirty-and-retired
        // outputs (catch-up case). Walk them.
        return self.compose_catch_up_outputs();
    };

    // 1) Compute C(F): dirty AND previous flip retired.
    let c_of_f = self.compute_c_of_f();

    // 2) End paint CB.
    let pool = self.frame_pool.as_mut().expect("frame_pool absent");
    let slot = &mut pool.slots[slot_id];
    unsafe { pool.vk.device.end_command_buffer(slot.cmd_buffer)? };

    // 3) Acquire paint_done semaphores for each X in C(F).
    let mut signal_infos: Vec<vk::SemaphoreSubmitInfo> = Vec::with_capacity(c_of_f.len());
    let mut paint_dones: HashMap<OutputId, vk::Semaphore> = HashMap::with_capacity(c_of_f.len());
    for out in &c_of_f {
        let sem = pool.paint_done.acquire(SemKey { slot: slot_id, output: *out })?;
        paint_dones.insert(*out, sem);
        signal_infos.push(
            vk::SemaphoreSubmitInfo::default()
                .semaphore(sem)
                .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS),
        );
    }

    // 4) Submit paint with the fan-out signals + paint_fence.
    let cb_info = [vk::CommandBufferSubmitInfo::default().command_buffer(slot.cmd_buffer)];
    let submit = [vk::SubmitInfo2::default()
        .command_buffer_infos(&cb_info)
        .signal_semaphore_infos(&signal_infos)];
    unsafe {
        pool.vk.device.queue_submit2(pool.vk.graphics_queue, &submit, slot.paint_fence)?;
    }
    slot.phase.mark_submitted();
    slot.c_of_f = c_of_f.clone();

    // 5) Submit composite per output in C(F). Per-output failures
    // are recorded by submit_composite_for_output (output stays
    // dirty, fence released) — keep going for the other outputs.
    for out in &c_of_f {
        let paint_done = paint_dones[out];
        let _ = self.submit_composite_for_output(slot_id, *out, Some(paint_done));
    }

    Ok(())
}
```

- [ ] **Step 4: Add `compute_c_of_f` + helpers**

```rust
/// Set of outputs that will composite this flush. **Call contract:**
/// only invoke from `flush_frame_and_composite` (which is in turn only
/// invoked from `composite_and_flip` or `maybe_composite`, both of
/// which run *after* `on_page_flip_ready` has processed pending
/// pageflip-complete events for the current core-loop iteration).
/// Calling from anywhere else means the vk_flip_pending state may be
/// stale and C(F) is wrong.
///
/// Spec v5 §"Output-set timing".
fn compute_c_of_f(&self) -> Vec<OutputId> {
    let mut out = Vec::new();
    for layout_idx in 0..self.outputs.len() {
        let oid = OutputId(layout_idx as u32);
        if !self.output_dirty.get(&oid).copied().unwrap_or(false) { continue; }
        if self.vk_flip_pending_for(layout_idx) { continue; }
        if !self.output_composable(layout_idx) { continue; }
        out.push(oid);
    }
    out
}

fn vk_flip_pending_for(&self, layout_idx: usize) -> bool {
    self.scanout_pools
        .get(layout_idx)
        .and_then(|p| p.as_ref())
        .map(|p| p.bos.iter().any(|b| matches!(
            b.state.phase,
            BoPhase::Submitted | BoPhase::Pending
        )))
        .unwrap_or(false)
}

/// Hotplug + lifecycle guard: an output is composable only if its
/// scanout pool exists AND has at least one Free BO. Codex round-4
/// finding: outputs added between frames must not join C(F) until
/// their scanout resources are ready.
fn output_composable(&self, layout_idx: usize) -> bool {
    let Some(p) = self.scanout_pools.get(layout_idx).and_then(|p| p.as_ref()) else {
        return false;
    };
    p.bos.iter().any(|b| matches!(b.state.phase, BoPhase::Free))
}

fn is_dirty_anywhere(&self) -> bool {
    self.output_dirty.values().any(|&v| v)
}
```

- [ ] **Step 5: Add transactional `submit_composite_for_output`**

```rust
/// Submit one output's composite. **Transactional**: the
/// composite_fence is only registered into `slot.composite_fence`
/// on submit success. On record/submit failure the fence is
/// released back to the pool and the output stays dirty for retry.
///
/// `wait_paint_done` is None for the catch-up path.
fn submit_composite_for_output(
    &mut self,
    slot_id: usize,
    out: OutputId,
    wait_paint_done: Option<vk::Semaphore>,
) -> Result<(), vk::Result> {
    let layout_idx = out.0 as usize;

    let ring_key = crate::kms::vk::composite_descriptor_ring::RingKey {
        slot: slot_id, output: out,
    };
    let descriptor_pool = self.composite_descriptor_ring
        .as_mut().expect("ring absent")
        .ensure_pool(ring_key)?;

    let composite_done = self.frame_pool.as_mut().unwrap()
        .composite_done_for(slot_id, out)?;

    // Acquire fence locally — DO NOT insert into slot.composite_fence yet.
    let composite_fence = self.frame_pool.as_mut().unwrap().fences.acquire()?;

    // Build scene + invoke compositor.
    let scene = self.build_composite_scene(layout_idx);
    let result = crate::kms::vk::compositor::record_and_present_composite_with_fence(
        &self.vk.as_ref().unwrap(),
        &self.drm, &self.outputs[layout_idx].output,
        &mut self.scanout_pools[layout_idx].as_mut().unwrap()
            .pick_free_bo_mut().expect("ring-checked Free bo"),
        &self.compositor_pipeline.as_ref().unwrap(),
        descriptor_pool,
        &scene,
        wait_paint_done,
        composite_done,
        composite_fence,
    );

    match result {
        Ok(()) => {
            // Submit succeeded: NOW register the fence as a retirement
            // dependency, mark the output clean.
            self.frame_pool.as_mut().unwrap()
                .slots[slot_id].composite_fence.insert(out, composite_fence);
            self.output_dirty.insert(out, false);
            Ok(())
        }
        Err(e) => {
            // Submit failed: release the fence (it was never submitted,
            // so it's still in the unsignalled state acquire() returned).
            // Keep output dirty so the next iteration retries.
            log::warn!(
                "composite for output {} failed: {e:?} — keeping dirty, releasing fence",
                layout_idx
            );
            self.frame_pool.as_mut().unwrap().fences.release(composite_fence);
            self.output_dirty.insert(out, true);
            Err(vk::Result::ERROR_UNKNOWN)
        }
    }
}
```

- [ ] **Step 6: Add `compose_catch_up_outputs` (uses `retire_slot`)**

```rust
/// Catch-up path: dirty outputs whose flips have retired but there
/// was no paint this cycle. Spec v5 §"Skipped-output rule" — composite
/// each with no paint_done wait; same-queue ordering guarantees the
/// last paint submission's writes are visible.
fn compose_catch_up_outputs(&mut self) -> Result<(), vk::Result> {
    let c = self.compute_c_of_f();
    if c.is_empty() { return Ok(()); }

    // Need a slot to own the composite_fence entries. Reserve the
    // next slot, retire its previous occupant, and treat this as a
    // "paint-empty" frame: empty CB submit signals paint_fence.
    let slot_id = self.frame_pool.as_ref().unwrap().slot_id_for_next();
    self.retire_slot(slot_id)?;  // <-- Blocker 2 fix: route through retirement.

    let pool = self.frame_pool.as_mut().unwrap();
    let (frame_id, _) = pool.alloc_frame_id();
    pool.slots[slot_id].frame_id = frame_id;
    pool.slots[slot_id].phase.begin_recording();

    // Empty paint submit (no CB, no signals) just to flip paint_fence
    // into a signalled state. Valid Vulkan: a SubmitInfo2 with zero
    // command-buffer infos signals the fence on dispatch.
    let cb_info: [vk::CommandBufferSubmitInfo; 0] = [];
    let submit = [vk::SubmitInfo2::default().command_buffer_infos(&cb_info)];
    unsafe {
        pool.vk.device.queue_submit2(
            pool.vk.graphics_queue, &submit, pool.slots[slot_id].paint_fence,
        )?;
    }
    pool.slots[slot_id].phase.mark_submitted();
    pool.slots[slot_id].c_of_f = c.clone();

    log::debug!("compose_catch_up: slot {slot_id} frame {frame_id} outputs={c:?}");
    for out in &c {
        // `submit_composite_for_output` already handles per-output
        // success/failure: success clears that output's dirty bit;
        // failure releases the fence and keeps it dirty.
        let _ = self.submit_composite_for_output(slot_id, *out, None);
    }

    Ok(())
}
```

- [ ] **Step 6: Build, test, clippy, commit**

```bash
cargo build -p yserver
cargo test -p yserver
cargo clippy -p yserver -- -D warnings
git add crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): open_frame + flush_frame_and_composite + catch-up path"
```

The helpers exist but no paint pipeline uses them yet. xts5/rendercheck should be unchanged.

---

### Task P1.8: Legacy dispatch helper

**Files:**
- Create: `crates/yserver/src/kms/vk/legacy_dispatch.rs`
- Modify: `crates/yserver/src/kms/vk/mod.rs`

The helper wraps the existing `run_one_shot_op` pattern so legacy callers, during the P3 rollout window, never reorder w.r.t. an open frame. Spec v5 §"Rollout invariant".

- [ ] **Step 1: Failing test for "flush before legacy" rule**

The trait method is generic over a recorder `FnOnce(&VkContext, vk::CommandBuffer)`, so the mock's `submit_one_shot_and_wait_idle` just records the call without invoking the closure (passing a real `VkContext` in tests is too much). Tests cover the ordering, not the live submit.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct MockBackend {
        events: Vec<&'static str>,
        frame_open: bool,
    }

    impl LegacyDispatchHost for MockBackend {
        fn frame_open(&self) -> bool { self.frame_open }
        fn flush_frame_and_composite(&mut self) -> Result<(), ()> {
            self.events.push("flush_frame_and_composite");
            self.frame_open = false;
            Ok(())
        }
        fn submit_one_shot_and_wait_idle<F>(&mut self, _record: F) -> Result<(), ()>
        where
            F: FnOnce(&VkContext, vk::CommandBuffer) -> Result<(), vk::Result>,
        {
            self.events.push("legacy_op");
            Ok(())
        }
    }

    fn noop_record(_vk: &VkContext, _cb: vk::CommandBuffer) -> Result<(), vk::Result> {
        Ok(())
    }

    #[test]
    fn legacy_with_open_frame_flushes_first() {
        let mut b = MockBackend { events: vec![], frame_open: true };
        dispatch_legacy(&mut b, noop_record).unwrap();
        assert_eq!(b.events, vec!["flush_frame_and_composite", "legacy_op"]);
    }

    #[test]
    fn legacy_with_no_open_frame_just_runs() {
        let mut b = MockBackend { events: vec![], frame_open: false };
        dispatch_legacy(&mut b, noop_record).unwrap();
        assert_eq!(b.events, vec!["legacy_op"]);
    }
}
```

- [ ] **Step 2: Run, fail**

```bash
cargo test -p yserver kms::vk::legacy_dispatch::tests
```

Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
//! Legacy dispatch helper for P3 mixed-mode rollout.
//! Spec v5 §"Rollout invariant".
//!
//! Any caller that uses `run_one_shot_op` + `vkQueueWaitIdle` must
//! route through this helper so that, if a new-path frame is open,
//! the frame's paint + composites are submitted first. Without this,
//! the legacy op would reach the GPU before the deferred new-path
//! work, reordering paint within or across frames.
//!
//! Trait methods are object-unsafe (the recorder is a generic closure
//! over the host's VkContext + CommandBuffer types). `dispatch_legacy`
//! is therefore generic over both the host and the recorder closure;
//! we don't use `&mut dyn LegacyDispatchHost`.

use ash::vk;

use super::device::VkContext;

pub trait LegacyDispatchHost {
    fn frame_open(&self) -> bool;
    fn flush_frame_and_composite(&mut self) -> Result<(), ()>;
    fn submit_one_shot_and_wait_idle<F>(&mut self, record: F) -> Result<(), ()>
    where
        F: FnOnce(&VkContext, vk::CommandBuffer) -> Result<(), vk::Result>;
}

pub fn dispatch_legacy<H, F>(host: &mut H, record: F) -> Result<(), ()>
where
    H: LegacyDispatchHost,
    F: FnOnce(&VkContext, vk::CommandBuffer) -> Result<(), vk::Result>,
{
    if host.frame_open() {
        host.flush_frame_and_composite()?;
    }
    host.submit_one_shot_and_wait_idle(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct MockBackend {
        events: Vec<&'static str>,
        frame_open: bool,
    }

    impl LegacyDispatchHost for MockBackend {
        fn frame_open(&self) -> bool { self.frame_open }
        fn flush_frame_and_composite(&mut self) -> Result<(), ()> {
            self.events.push("flush_frame_and_composite");
            self.frame_open = false;
            Ok(())
        }
        fn submit_one_shot_and_wait_idle(
            &mut self, _record: &dyn Fn(),
        ) -> Result<(), ()> {
            self.events.push("legacy_op");
            Ok(())
        }
    }

    #[test]
    fn legacy_with_open_frame_flushes_first() {
        let mut b = MockBackend { events: vec![], frame_open: true };
        dispatch_legacy(&mut b, &|| {}).unwrap();
        assert_eq!(b.events, vec!["flush_frame_and_composite", "legacy_op"]);
    }

    #[test]
    fn legacy_with_no_open_frame_just_runs() {
        let mut b = MockBackend { events: vec![], frame_open: false };
        dispatch_legacy(&mut b, &|| {}).unwrap();
        assert_eq!(b.events, vec!["legacy_op"]);
    }
}
```

- [ ] **Step 4: Wire, test, clippy, commit**

```bash
cargo test -p yserver kms::vk::legacy_dispatch
cargo clippy -p yserver -- -D warnings
git add crates/yserver/src/kms/vk/legacy_dispatch.rs crates/yserver/src/kms/vk/mod.rs
git commit -m "feat(vk): legacy dispatch helper with flush-before-legacy rule"
```

P1.8 ends here. The `LegacyDispatchHost` implementation for `KmsBackend` lands in the new task P1.9 below, which wraps every legacy dispatch site through the helper as a single no-behaviour-change refactor. Doing it then keeps P1.8 to a clean compilable unit (module + tests only) — no `todo!` left in committed code.

---

### Task P1.9: Implement `LegacyDispatchHost` for `KmsBackend` + wrap every existing dispatch site

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`

This is a **no-behaviour-change refactor**. Today, every paint recorder runs through `self.with_ops_cb(|vk, cb| record_…(...))` which calls `run_one_shot_op` (submit + `queue_wait_idle`). After P1.9, every such call goes through `dispatch_legacy(self, |vk, cb| record_…(...))`. Since `current_slot` is always `None` before P2.1 lands, `frame_open()` returns false and the helper just forwards to `submit_one_shot_and_wait_idle` — same behaviour, same call shape. This is the prerequisite for P2.1 so that as soon as the first new-path recorder appears, *every other* recorder is already routed through the flush-first path. Codex round-4 finding.

- [ ] **Step 1: Impl the trait on `KmsBackend`**

```rust
impl crate::kms::vk::legacy_dispatch::LegacyDispatchHost for KmsBackend {
    fn frame_open(&self) -> bool { self.current_slot.is_some() }

    fn flush_frame_and_composite(&mut self) -> Result<(), ()> {
        KmsBackend::flush_frame_and_composite(self)
            .map_err(|e| log::error!("flush_frame failed: {e:?}"))
    }

    fn submit_one_shot_and_wait_idle<F>(&mut self, record: F) -> Result<(), ()>
    where
        F: FnOnce(&crate::kms::vk::device::VkContext, vk::CommandBuffer)
            -> Result<(), vk::Result>,
    {
        let vk = self.vk.as_ref().expect("vk absent");
        let pool = self.ops_command_pool.as_ref().expect("ops pool absent");
        crate::kms::vk::ops::run_one_shot_op(vk, pool.handle(), record)
            .map_err(|e| log::error!("run_one_shot_op failed: {e:?}"))
    }
}
```

- [ ] **Step 2: Walk every existing dispatch site, replace `with_ops_cb` with `dispatch_legacy`**

```bash
grep -n "with_ops_cb\|run_one_shot_op" /home/jos/Projects/yserver/crates/yserver/src/kms/backend.rs
```

For each site, replace:

```rust
self.with_ops_cb(|vk, cb| {
    crate::kms::vk::ops::fill::record_fill_rectangles(vk, cb, …)
})?;
```

with:

```rust
crate::kms::vk::legacy_dispatch::dispatch_legacy(self, |vk, cb| {
    crate::kms::vk::ops::fill::record_fill_rectangles(vk, cb, …)
}).map_err(|_| /* propagate as appropriate */ ...)?;
```

`with_ops_cb` itself can stay as a thin wrapper around `dispatch_legacy` for any internal callers, or be deleted if every caller is migrated.

- [ ] **Step 3: Build, run xts5/rendercheck — must match baseline**

```bash
cargo build -p yserver
cargo test -p yserver
cargo clippy -p yserver -- -D warnings
just xts-yserver scenario=Xproto timeout=600 2>&1 | tee /tmp/xts-p1.9.log
diff /tmp/xts-baseline.log /tmp/xts-p1.9.log
just rendercheck-yserver timeout=600 2>&1 | tee /tmp/rc-p1.9.log
diff /tmp/rc-baseline.log /tmp/rc-p1.9.log
```

Expected: byte-for-byte parity. Any divergence means the wrapper changed behaviour and needs investigation before P2.1.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/
git commit -m "refactor(kms): every legacy dispatch site routes through dispatch_legacy (no-behaviour-change)"
```

---

## Phase P2 — first pipeline migration (FillRect) as proof

### Task P2.1: Fork FillRect dispatch (new path)

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`

After P1.9, every legacy dispatch site already goes through `dispatch_legacy`. P2.1 only adds the **new-path** branch for FillRect; nothing else changes.

- [ ] **Step 1: Locate the FillRect dispatch site**

```bash
grep -n "fill::record_fill_rectangles\|dispatch_legacy" /home/jos/Projects/yserver/crates/yserver/src/kms/backend.rs | head
```

- [ ] **Step 2: Add the FillRect fork**

```rust
if std::env::var("YSERVER_LEGACY_VK_SYNC").is_ok() {
    // Whole-backend legacy mode (env-controlled). Already wrapped by
    // P1.9 — frame_open() is always false here so this is a straight
    // legacy submit + wait_idle.
    crate::kms::vk::legacy_dispatch::dispatch_legacy(self, |vk, cb| {
        crate::kms::vk::ops::fill::record_fill_rectangles(
            vk, cb, target, color, &rects, clip_scissor,
        )
    }).map_err(|_| vk::Result::ERROR_UNKNOWN)?;
} else {
    // New path: append to the open frame CB.
    let (_slot, cb) = self.open_frame()?;
    let vk = self.vk.as_ref().expect("vk absent");
    crate::kms::vk::ops::fill::record_fill_rectangles(
        vk, cb, target, color, &rects, clip_scissor,
    )?;
    self.mark_dirty();
}
```

`YSERVER_LEGACY_VK_SYNC` is a **whole-backend** switch: when set, every recorder takes the legacy branch; when unset, every recorder takes the new branch. P3 migrates one pipeline at a time by adding the new-path branch and leaving the legacy branch intact under the env gate.

- [ ] **Step 3: Build + run both modes**

```bash
cargo build -p yserver
cargo test -p yserver
YSERVER_LEGACY_VK_SYNC=1 cargo test -p yserver
```

Expected: clean both modes.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/
git commit -m "feat(kms): FillRect new-path dispatch via frame CB"
```

---

### Task P2.2: Wire `composite_and_flip` through `flush_frame_and_composite`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`
- Modify: `crates/yserver/src/kms/vk/compositor.rs`

- [ ] **Step 1: Update `record_and_present_composite` signature**

`record_and_present_composite_with_fence` (renamed in P1.6 step 5) needs to accept: optional `wait_paint_done: Option<vk::Semaphore>`, mandatory `composite_done: vk::Semaphore`, mandatory `composite_fence: vk::Fence`.

Inside, the `vkQueueSubmit2` block becomes:

```rust
let wait_arr: Vec<vk::SemaphoreSubmitInfo> = wait_paint_done.map(|s|
    vec![vk::SemaphoreSubmitInfo::default()
        .semaphore(s)
        // ALL_COMMANDS per spec v5 §"Composite submission" until
        // barriers are audited in a follow-up.
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)]
).unwrap_or_default();
let sig_info = [vk::SemaphoreSubmitInfo::default()
    .semaphore(composite_done)
    .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
let submit_arr = [
    vk::SubmitInfo2::default()
        .command_buffer_infos(&[vk::CommandBufferSubmitInfo::default().command_buffer(cb)])
        .wait_semaphore_infos(&wait_arr)
        .signal_semaphore_infos(&sig_info),
];
unsafe {
    vk.device.queue_submit2(vk.graphics_queue, &submit_arr, composite_fence)?;
}
```

The `bo.vk_semaphore` use in the existing code is replaced — `composite_done` is now the per-(slot, output) handle from the FramePool, not the bo's internal semaphore. The bo's internal semaphore can stay for backwards-compat with non-frame-pool callers if any remain.

- [ ] **Step 2: Replace `composite_and_flip` body to use the new pipeline**

Critically: **never blanket-clear dirty here.** Per-output dirty bits are cleared inside `submit_composite_for_output` on success; failures leave the output dirty. Skipped outputs (vk_flip_pending) are not in C(F), never run a composite, and their dirty bit stays set so the next iteration's catch-up path picks them up. Codex round-4 blocker fix.

```rust
pub fn composite_and_flip(&mut self) -> io::Result<()> {
    // Caller contract: on_page_flip_ready has already drained any
    // pending pageflip-complete events for this iteration before
    // this method runs. Both call sites (maybe_composite and the
    // on_page_flip_ready arm itself) satisfy this — see
    // crates/yserver-core/src/core_loop/run.rs:104..314.
    if !self.is_dirty_anywhere() && self.current_slot.is_none() {
        return Ok(());
    }
    self.flush_frame_and_composite()
        .map_err(|e| io::Error::other(format!("flush_frame: {e:?}")))
    // Dirty bits are NOT cleared here. Per-output success in
    // submit_composite_for_output is the only path that clears.
}
```

- [ ] **Step 3: Build, run xts5 + rendercheck under each mode**

```bash
cargo build -p yserver
cargo test -p yserver

just xts-yserver scenario=Xproto timeout=600 2>&1 | tee /tmp/xts-p2-new.log
YSERVER_LEGACY_VK_SYNC=1 just xts-yserver scenario=Xproto timeout=600 2>&1 | tee /tmp/xts-p2-legacy.log
diff /tmp/xts-baseline.log /tmp/xts-p2-legacy.log
diff /tmp/xts-baseline.log /tmp/xts-p2-new.log
```

Expected: legacy matches baseline. New path: only FillRect tests should differ in execution path, but visible behaviour identical.

If new-path regresses any test: **stop**, investigate. The new path bug is in something between `open_frame`, `flush_frame_and_composite`, and `record_and_present_composite_with_fence`. Don't continue P3 until parity is back.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/
git commit -m "feat(kms): composite_and_flip routes through flush_frame_and_composite"
```

---

### Task P2.3: Manual hardware smoke

- [ ] **Step 1: Run on Polaris MATE rig**

```bash
just yserver-mate-hw 2>&1 | tee /tmp/yserver-p2-mate.log
# In another shell: open mate-control-center, hover rows, move pointer
# rapidly. Watch radeontop / GPU utilisation.
```

Compare: with `YSERVER_LEGACY_VK_SYNC=1` (baseline 100% spike expected) vs no env (new path should not spike on FillRect-driven hover).

- [ ] **Step 2: Record observations in `docs/status.md`**

Under "Sync rework P2 results": GPU util before/after, any visible regressions, validation-layer output if `YSERVER_VK_VALIDATION=1` was on.

- [ ] **Step 3: Commit status update**

```bash
git add docs/status.md
git commit -m "docs: P2 sync rework — FillRect hardware smoke results"
```

---

### Task P2.4: Tighten descriptor-ring sizing based on P2 observations

- [ ] **Step 1: Add a counting log to the compositor**

In `record_and_present_composite_with_fence`, log the number of descriptors actually allocated per (slot, output) per frame. Run the manual smoke once more, scrape the log for the max.

- [ ] **Step 2: Tune `CompositeDescriptorRing` pool size**

Update the `pool_sizes` and `max_sets` at construction in `KmsBackend` to match the observed max with comfortable headroom (e.g., 2×).

- [ ] **Step 3: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "tune(kms): compositor descriptor ring sizing from P2 observations"
```

---

## Phase P3 — remaining paint pipelines

Each task migrates one pipeline. The dispatch fork pattern is identical to P2.1:

```rust
if std::env::var("YSERVER_LEGACY_VK_SYNC").is_ok() {
    crate::kms::vk::legacy_dispatch::dispatch_legacy(self, |vk, cb| {
        ops::<pipeline>::record_…(vk, cb, …)
    })?;
} else {
    let (_, cb) = self.open_frame()?;
    let vk = self.vk.as_ref().expect("vk absent");
    ops::<pipeline>::record_…(vk, cb, …)?;
    self.mark_dirty();
}
```

After each pipeline lands, run xts5 + rendercheck under both modes and compare to the P0.0 baseline. Per-op `vkQueueWaitIdle` inside the recorder's scratch (mask_scratch, gradient, etc.) is removed in the *resource-lifetime* step that lands alongside each pipeline.

### Task P3.0: Migrate scratch lifetime to `FrameScopedQueue`

This is the prereq for the per-pipeline migrations — many recorders need their scratch resources backed by frame-slot retirement, not per-op drain.

**Files:**
- Modify: `crates/yserver/src/kms/vk/{copy_scratch,mask_scratch,gradient,glyph,ops/mod.rs (OpsStaging)}.rs`

For each scratch type:

- [ ] **Step 1: Replace inline `queue_wait_idle` on resize/teardown with a push to `FrameScopedQueue<TypeName>`**

The scratch type takes `&mut FrameScopedQueue<…>` and the current `slot_id` as parameters when it wants to free / resize. Old resource goes on the queue; replacement is allocated inline. The `Drop` impl keeps `device_wait_idle` (teardown).

- [ ] **Step 2: Drain in `open_frame` (when retiring previous slot)**

In `open_frame`'s previous-occupant-retirement block, also drain each scratch's queue against the per-slot `retired_slots` boolean array. The drained items are then `unsafe { destroy }`ed.

- [ ] **Step 3: Test + commit per scratch type**

```bash
cargo test -p yserver
cargo clippy -p yserver -- -D warnings
git add crates/yserver/src/kms/vk/<scratch>.rs crates/yserver/src/kms/backend.rs
git commit -m "refactor(vk): <scratch> lifetime via FrameScopedQueue"
```

Repeat for: `copy_scratch`, `mask_scratch`, `gradient`, `glyph`, `OpsStaging`.

---

### Task P3.1: `CopyArea`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (dispatch sites for `record_copy_area_distinct`, `record_copy_area_same`, `record_copy_area_same_overlap`)
- Modify: `crates/yserver/src/kms/vk/ops/copy.rs` (add in-frame barriers)

- [ ] **Step 1: Fork each dispatch site**

Identical to P2.1's FillRect fork. Three call sites.

- [ ] **Step 2: Add in-frame source→dest barrier inside each `record_copy_area_*`**

Before the copy, insert a `vkCmdPipelineBarrier2` against the source mirror image: `(COLOR_ATTACHMENT_OUTPUT|ALL_GRAPHICS, COLOR_ATTACHMENT_WRITE|SHADER_WRITE) → (TRANSFER, TRANSFER_READ)`. After the copy, insert dest mirror barrier: `(TRANSFER, TRANSFER_WRITE) → (FRAGMENT_SHADER, SHADER_READ)`.

These were previously implicit via the queue drain.

- [ ] **Step 3: Test + commit**

```bash
cargo test -p yserver
just xts-yserver scenario=Xproto timeout=600 2>&1 | grep FAIL
just rendercheck-yserver timeout=600 2>&1 | grep -i fail
git add crates/yserver/src/kms/
git commit -m "feat(kms): CopyArea on per-frame CB + in-frame barriers"
```

---

### Task P3.2: `PutImage` + `GetImage`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`
- Modify: `crates/yserver/src/kms/vk/ops/image.rs` (in-frame barriers)
- Modify: `crates/yserver/src/kms/vk/dst_readback.rs` (separate one-shot CB + fence for `record_get_image`)

- [ ] **Step 1: PutImage — fork dispatch as P3.1**

Pre-op barrier `UNDEFINED→TRANSFER_DST`; post-op `TRANSFER_DST→SHADER_READ`. Inside `record_put_image`.

- [ ] **Step 2: GetImage — separate one-shot CB**

GetImage is **not a paint-stream recorder** — it must not open or append to a frame. The host needs to wait for *just this op*, which means a dedicated CB + fence; appending to the frame CB would entangle the GetImage host wait with arbitrary other paint ops in the frame. Codex round-4 clarification.

Recipe:
1. Call `self.flush_frame_and_composite()` first (so prior paint is in flight).
2. Acquire a one-shot CB from the existing `OpsCommandPool` (or a dedicated `dst_readback_pool`).
3. Acquire a `VkFence` from `frame_pool.fences`.
4. Record + submit with `signal_fence = my_fence`.
5. `wait_for_fences_bounded(&[my_fence])` with the standard 250 ms timeout.
6. memcpy from staging into the X11 reply buffer.
7. Release the fence to the pool.

Edit `dst_readback.rs:105, 264`: replace `queue_wait_idle` with the targeted fence wait.

- [ ] **Step 3: Test + commit**

```bash
cargo test -p yserver
just xts-yserver scenario=Xproto timeout=600 2>&1 | grep FAIL
just rendercheck-yserver timeout=600 2>&1 | grep -i fail
git add crates/yserver/src/kms/
git commit -m "feat(kms): PutImage/GetImage on per-frame CB + targeted readback fence"
```

---

### Task P3.3: RENDER `Composite`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`
- Modify: `crates/yserver/src/kms/vk/ops/render.rs` (in-frame barriers)
- Modify: `crates/yserver/src/kms/vk/render_pipeline.rs` (annotate or migrate teardown drains)

- [ ] **Step 1: Fork dispatch in `backend.rs`**

The existing path at `backend.rs:5466` (RENDER allocation) already happens *before* `run_one_shot_op` records, so the descriptor-alloc-first invariant holds. New path: same descriptor allocation, but the recorder appends to the frame CB.

- [ ] **Step 2: dst-readback into staging is GPU-internal**

For MODE=1 (OVER blend) the dst-readback is a `vkCmdCopyImageToBuffer` followed by a `vkCmdCopyBufferToImage` (or whatever the existing structure is — verify in `dst_readback.rs:167`). Both go into the **same** frame CB; an in-CB barrier between them is enough, no separate submit. The `dst_readback.rs:105` drain referenced earlier is the resize/grow lifetime, handled by P3.0.

- [ ] **Step 3: Audit `render_pipeline.rs:510, 652`**

```bash
grep -n -B 5 "queue_wait_idle" /home/jos/Projects/yserver/crates/yserver/src/kms/vk/render_pipeline.rs
```

Classify each as teardown-only (annotate) vs hot-path (migrate via `FrameScopedQueue`). The pipeline-cache rebuild ones are teardown.

- [ ] **Step 4: Test + commit**

```bash
cargo test -p yserver
just xts-yserver scenario=Xproto timeout=600 2>&1 | grep FAIL
just rendercheck-yserver timeout=600 2>&1 | grep -i fail
git add crates/yserver/src/kms/
git commit -m "feat(kms): RENDER Composite on per-frame CB + in-CB dst-readback"
```

---

### Task P3.4: RENDER `CompositeGlyphs`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`
- Modify: `crates/yserver/src/kms/vk/ops/text.rs`
- Modify: `crates/yserver/src/kms/vk/glyph.rs`
- Modify: `crates/yserver/src/kms/vk/text_pipeline.rs`

- [ ] **Step 1: Fork dispatch**

- [ ] **Step 2: Glyph atlas upload barrier in the frame CB**

`glyph.rs:444` is the atlas-upload drain. In the new path: barrier `(TRANSFER, TRANSFER_WRITE) → (FRAGMENT_SHADER, SHADER_READ)` on the atlas image after the upload, both inside the frame CB.

`glyph.rs:460` (sampler teardown) goes through `FrameScopedQueue` per P3.0.

- [ ] **Step 3: Test + commit**

```bash
cargo test -p yserver
just xts-yserver scenario=Xproto timeout=600 2>&1 | grep FAIL
just rendercheck-yserver timeout=600 2>&1 | grep -i fail
git add crates/yserver/src/kms/
git commit -m "feat(kms): RENDER CompositeGlyphs on per-frame CB + atlas barriers"
```

---

### Task P3.5: RENDER `Trapezoids`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`
- Modify: `crates/yserver/src/kms/vk/ops/traps.rs`

- [ ] **Step 1: Fork dispatch**

- [ ] **Step 2: Mask write → composite read barrier in the frame CB**

Trapezoids write a coverage mask then composite it. The mask is in `mask_scratch.rs` (allocator) — write → read barrier between the mask-write and the composite-read, both inside the frame CB.

- [ ] **Step 3: Test + commit**

```bash
cargo test -p yserver
just xts-yserver scenario=Xproto timeout=600 2>&1 | grep FAIL
just rendercheck-yserver timeout=600 2>&1 | grep -i fail
git add crates/yserver/src/kms/
git commit -m "feat(kms): RENDER Trapezoids on per-frame CB + mask barriers"
```

---

### Task P3.6: Logic-fill (`record_logic_fill`)

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`
- Modify: `crates/yserver/src/kms/vk/ops/fill.rs`
- Modify: `crates/yserver/src/kms/vk/logic_fill_pipeline.rs` (annotate teardown drain)

- [ ] **Step 1: Fork dispatch**

- [ ] **Step 2: Annotate `logic_fill_pipeline.rs:137`**

Pipeline-cache rebuild — teardown, keep.

- [ ] **Step 3: Test + commit**

```bash
cargo test -p yserver
just xts-yserver scenario=Xproto timeout=600 2>&1 | grep FAIL
just rendercheck-yserver timeout=600 2>&1 | grep -i fail
git add crates/yserver/src/kms/
git commit -m "feat(kms): logic-fill on per-frame CB"
```

---

### Task P3.7: Annotate all remaining `queue_wait_idle` sites

**Files:**
- Modify: any `crates/yserver/src/kms/vk/*.rs` with a remaining drain.

- [ ] **Step 1: Walk every remaining call site**

```bash
grep -n "queue_wait_idle" /home/jos/Projects/yserver/crates/yserver/src/kms/vk/*.rs /home/jos/Projects/yserver/crates/yserver/src/kms/vk/ops/*.rs
```

For each: confirm teardown / pipeline rebuild / Drop, add a one-line comment:

```rust
// LOAD-BEARING: pipeline cache rebuild, off the hot path. Do not
// migrate to per-frame fence — these resources aren't frame-scoped.
let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
```

- [ ] **Step 2: Commit**

```bash
git add crates/yserver/src/kms/vk/
git commit -m "docs(vk): annotate load-bearing queue_wait_idle on teardown paths"
```

---

## Phase P4 — strip legacy + integration + CI

### Task P4.1: Remove `YSERVER_LEGACY_VK_SYNC` gating

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`
- Modify: `crates/yserver/src/kms/vk/ops/mod.rs` (remove `run_one_shot_op` if no callers)
- Modify: `crates/yserver/src/kms/vk/legacy_dispatch.rs` (delete the module)

- [ ] **Step 1: Final parity check before stripping**

```bash
just xts-yserver scenario=Xproto timeout=600 2>&1 | tee /tmp/xts-p4-pre-new.log
YSERVER_LEGACY_VK_SYNC=1 just xts-yserver scenario=Xproto timeout=600 2>&1 | tee /tmp/xts-p4-pre-legacy.log
diff /tmp/xts-p4-pre-legacy.log /tmp/xts-p4-pre-new.log
```

Expected: parity. If not — stop, fix.

- [ ] **Step 2: Strip every `if std::env::var("YSERVER_LEGACY_VK_SYNC")` branch**

```bash
grep -n "YSERVER_LEGACY_VK_SYNC" /home/jos/Projects/yserver/crates/yserver/src/kms/backend.rs
```

Delete the legacy arm at each site; keep the new path.

- [ ] **Step 3: Remove `with_ops_cb` + `run_one_shot_op` + `OpsCommandPool` if unused**

```bash
grep -rn "with_ops_cb\|run_one_shot_op\|OpsCommandPool" /home/jos/Projects/yserver/crates/ 2>/dev/null
```

Anything left in tests? In docstrings? Update the doc comments in `ops/fill.rs` etc. to describe the new pattern.

- [ ] **Step 4: Delete `legacy_dispatch.rs`**

It's unreachable now. Remove `pub mod legacy_dispatch;` from `mod.rs`.

- [ ] **Step 5: Build, test**

```bash
cargo build -p yserver
cargo test -p yserver
cargo clippy -p yserver -- -D warnings
just xts-yserver scenario=Xproto timeout=600 2>&1 | grep FAIL
just rendercheck-yserver timeout=600 2>&1 | grep -i fail
```

Expected: clean. xts5/rendercheck still parity with baseline.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver/src/kms/
git commit -m "feat(kms): remove YSERVER_LEGACY_VK_SYNC, retire legacy dispatch"
```

---

### Task P4.2: Validation-layer enforcement

**Files:**
- Check / modify CI config and/or `tools/run-with-validation.sh`.

- [ ] **Step 1: Run with validation locally**

```bash
YSERVER_VK_VALIDATION=1 just xts-yserver scenario=Xproto timeout=600 2>&1 | tee /tmp/validation-p4.log
grep -i "VALIDATION\|ERROR" /tmp/validation-p4.log
```

Expected: zero validation errors. If any: **stop**, fix the barrier coverage before declaring P4 done.

- [ ] **Step 2: Find CI config**

```bash
ls /home/jos/Projects/yserver/.github/workflows/ 2>/dev/null
ls /home/jos/Projects/yserver/.gitlab-ci.yml 2>/dev/null
```

If CI exists, add a job that runs `YSERVER_VK_VALIDATION=1 just xts-yserver` and greps the log for `VALIDATION.*ERROR`, failing the job on a match. If no CI, write `tools/run-with-validation.sh` documenting the manual gate.

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ tools/ docs/status.md
git commit -m "ci: gate sync-rework on Vulkan validation layer cleanliness"
```

---

### Task P4.3: Integration test — paint→composite→flip end-to-end

**Files:**
- Create: `crates/yserver/tests/paint_composite_flip.rs`

- [ ] **Step 1: Write the `#[ignore]` integration test**

Uses `ServerFixture` (locate via `grep -rn "ServerFixture\|fn for_tests" /home/jos/Projects/yserver/crates/`):

1. Create a window.
2. Issue `PolyFillRectangle` (paints into mirror).
3. Issue `PolyFillRectangle` to a different window (so the frame has multiple ops).
4. Trigger `composite_and_flip`.
5. Use `do_dump_scanout` (existing path) to read back the scanout.
6. Assert pixel content matches a software-rendered reference.

```rust
#[test]
#[ignore = "needs live VK ICD"]
fn paint_composite_flip_dual_op() {
    let mut fixture = ServerFixture::with_kms();
    let win = fixture.create_window(100, 100, 200, 200);
    fixture.fill_rect(win, Rect::new(0, 0, 50, 50), Color::RED);
    fixture.fill_rect(win, Rect::new(50, 50, 100, 100), Color::BLUE);
    fixture.composite_and_flip().unwrap();
    let img = fixture.dump_scanout();
    assert_pixel(&img, 100 + 25, 100 + 25, Color::RED);
    assert_pixel(&img, 100 + 75, 100 + 75, Color::BLUE);
}
```

- [ ] **Step 2: Run**

```bash
cargo test -p yserver --test paint_composite_flip -- --ignored
```

- [ ] **Step 3: Commit**

```bash
git add crates/yserver/tests/paint_composite_flip.rs
git commit -m "test(vk): paint→composite→flip end-to-end integration"
```

---

## Phase P5 — HW cursor plane revert (optional)

### Task P5.1: Empirical smoke with HW cursor disabled

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`

- [ ] **Step 1: Locate the HW cursor plane code**

```bash
git show 2d357cc --stat
grep -rn "cursor_plane\|hw_cursor\|drmModeMoveCursor" /home/jos/Projects/yserver/crates/yserver/src/kms/ | head
```

- [ ] **Step 2: Add env gate `YSERVER_DISABLE_HW_CURSOR`**

```bash
YSERVER_DISABLE_HW_CURSOR=1 just yserver-mate-hw
# Manual: hover pointer through mate-control-center, watch for lag/artifacts.
```

If responsive without HW cursor: proceed to P5.2.
If lag returns: keep HW cursor; stop.

- [ ] **Step 3: Commit the env gate + observations**

```bash
git add crates/yserver/src/kms/backend.rs docs/status.md
git commit -m "feat(kms): YSERVER_DISABLE_HW_CURSOR env flag for P5 smoke"
```

---

### Task P5.2: Revert HW cursor plane (only if P5.1 passed)

- [ ] **Step 1: `git revert 2d357cc`**

```bash
git revert --no-commit 2d357cc
git status
# Resolve conflicts; the cursor path returns to the pre-2d357cc software quad.
```

- [ ] **Step 2: Remove `YSERVER_DISABLE_HW_CURSOR` gating**

Redundant now.

- [ ] **Step 3: Smoke + tests**

```bash
just yserver-mate-hw  # manual
just xts-yserver scenario=Xproto timeout=600
just rendercheck-yserver timeout=600
```

Expected: no regressions; pointer responsive.

- [ ] **Step 4: Commit**

```bash
git commit -m "revert(kms): HW cursor plane no longer needed after sync rework"
```

---

## Phase P6 — dual-output flicker re-test

### Task P6.1: Replay flicker tests

- [ ] **Step 1: Boot dual-screen Polaris**

```bash
just yserver-dual-polaris  # or equivalent
```

- [ ] **Step 2: Replay wezterm + uxterm tests**

Open wezterm, scroll output. Resize uxterm rapidly. Hover mate-control-center. Watch both outputs for flicker.

- [ ] **Step 3: Record results in docs/status.md**

If absent: P6 closed.
If present: open P6.2.

- [ ] **Step 4: Commit status update**

```bash
git add docs/status.md
git commit -m "docs: P6 dual-output flicker re-test results"
```

---

### Task P6.2 (conditional): per-output `vk_flip_pending` scheduling follow-up

If flicker persists:

- [ ] **Step 1: Open a follow-up spec**

`docs/superpowers/specs/2026-MM-DD-dual-output-flip-scheduling.md`. Scope: investigate per-output frame-age drift caused by the skip in `composite_and_flip`.

- [ ] **Step 2: Stop this plan here**

P6.2 implementation is out of scope; gets its own plan via the writing-plans skill.

---

## Phase P7 — timeline-semaphore optimisation (conditional on P0)

**Skip if `external_sem_caps.timeline_sync_fd_exportable == false` on primary devices.** Re-check P0.2's logged caps.

### Task P7.1: Replace per-(slot, output) binary paint_done with one timeline

**Files:**
- Modify: `crates/yserver/src/kms/vk/frame.rs`
- Modify: `crates/yserver/src/kms/vk/semaphore_pool.rs` (add a `TimelinePool` or similar)
- Modify: `crates/yserver/src/kms/backend.rs`

- [ ] **Step 1: Add a timeline `paint_done_timeline` semaphore to `FramePool`**

The paint submit signals value `slot_seq + 1`; each composite waits at `slot_seq + 1`. The (slot, output) binary pool is dropped for paint_done.

`composite_done` stays binary (the SYNC_FD export to KMS is a binary handle; timeline export is for the upgrade-side, not the KMS-side).

- [ ] **Step 2: Replace SYNC_FD export for paint_done with the timeline-value variant**

Actually, paint_done is **GPU-internal** — it's not exported. So nothing changes for the export path; this task is purely about reducing the number of binary semaphore handles.

- [ ] **Step 3: xts5 + rendercheck parity**

```bash
just xts-yserver scenario=Xproto timeout=600 2>&1 | grep FAIL
just rendercheck-yserver timeout=600 2>&1 | grep -i fail
```

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/
git commit -m "perf(vk): timeline paint_done (P7, gated on device caps)"
```

---

## Finishing

### Final task: full verification + branch finish

- [ ] **Step 1: Full test matrix**

```bash
cargo build -p yserver
cargo test -p yserver
cargo clippy -p yserver -- -D warnings
just xts-yserver scenario=Xproto timeout=600
just rendercheck-yserver timeout=600
YSERVER_VK_VALIDATION=1 just xts-yserver scenario=Xproto timeout=600 2>&1 | grep -i validation
```

Expected: clean; no validation errors.

- [ ] **Step 2: Hardware regression sweep**

- mate-control-center hover (Polaris) — no GPU spike.
- wezterm open + uxterm resize on dual-screen Polaris — flicker absent or matches P6 result.
- gtk3-demo, MATE desktop session smoke — no visual regressions.
- vkcube + glxgears on a redirected window — no regression (spec Test 10).

- [ ] **Step 3: Update `docs/status.md`**

"Sync rework — landed" section: GPU util before/after, flicker resolution, HW cursor status.

- [ ] **Step 4: Use `superpowers:finishing-a-development-branch`**

It will guide the merge/PR decision.

---

## Risks and mitigations (cross-reference to spec)

- **Latent ordering bugs.** Per-pipeline rollout with `YSERVER_LEGACY_VK_SYNC` gate; visual A/B at every P3 step.
- **TIMELINE+SYNC_FD portability.** Binary primary, timeline P7-conditional, gated on P0 probe.
- **Shared mirror sync across outputs.** All paint in one frame CB; per-(slot, output) paint_done fan-out at signal time. C(F)-at-flush avoids signal-without-wait.
- **Scratch / staging / descriptor pool lifetime.** `FrameScopedQueue` (slot-indexed, wrap-safe), reset in `open_frame` at slot-retire time.
- **Compositor descriptor pool collision under async submits.** `CompositeDescriptorRing` keyed on `(slot, output)`, reset only when that slot's `composite_fence[output]` signals.
- **Validation-layer regressions.** CI gate at P4.2.
- **In-frame barrier coverage.** Conservative `ALL_GRAPHICS`/`ALL_COMMANDS` masks first; audit and tighten in a follow-up.
- **Dual-output flicker may persist.** P6 re-test; P6.2 follow-up if needed.
- **Device-lost / GPU hang.** `wait_for_fences_bounded` (250 ms) at every hot-path wait. `WaitErr::Timeout` logs a stall warning and returns; the dirty flag stays set, the next composite cycle retries. `WaitErr::DeviceLost` is fatal — log + `std::process::exit(1)`. KMS has no reinit story; a separate follow-up would add it. P1.7's `open_frame` and P3.2's GetImage path are the two hot-path call sites; both already handle this through the `WaitErr` match arms.
- **HW cursor pacing.** P5 optional, empirical.
- **Skipped-output correctness.** Same-queue ordering, not carried semaphore — see spec v5 §"Skipped-output rule".
- **Mid-frame OUT_OF_POOL_MEMORY.** Recorder error before any `vkCmd*`; dispatch helper flushes + retries in a fresh frame (the descriptor-before-recording invariant is documented in spec v5 and verified for all current recorders in codex round 4).
