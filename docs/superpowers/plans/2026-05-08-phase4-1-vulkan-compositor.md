# Phase 4.1 — Vulkan compositor on KMS (implementation plan)

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to
> implement this plan task-by-task.

**Goal:** Replace the pixman CPU compositor in `crates/yserver/src/kms/`
with a Vulkan compositor built on a per-window-texture scene graph,
without changing the `Backend` trait surface (so `yserver-core` and the
ynest backend are untouched).

**Architecture:** New `crates/yserver/src/kms/vk/` subtree owns all
Vulkan code. Each X window/pixmap holds a `DrawableImage` (Vulkan
image). KMS scanout uses GBM bos imported as `VkImage`s, with explicit
fences (DRM syncobj ↔ `VK_KHR_external_semaphore_fd`) on atomic
pageflip. RENDER ops compile lazily into a `RenderPipelineKey`-keyed
pipeline cache. See full spec:
[`docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md`](../specs/2026-05-07-phase4-1-vulkan-compositor-design.md).

**Tech stack:** `ash` (raw Vulkan), `gpu-allocator` (VMA-style memory
manager), `gbm` (GBM bo allocation), `drm` (already in use). No
`vulkano`, no `wgpu`, no `smithay`.

**Branch:** `accel`, recreated from master. Long-lived; single
squash-merge to master at the end (see §4.1.6).

---

## Ground rules (apply to every task in this plan)

- **Format**: `cargo +nightly fmt` before every commit (project uses
  nightly rustfmt features).
- **Lint**: `cargo clippy` before every commit. Project does *not*
  require pedantic clippy (per `AGENTS.md`); regular clippy must be
  warning-clean.
- **Build**: `cargo build --bin yserver` is the inner-loop check.
- **Test**: `cargo test` (host-side; lavapipe integration tests opt-in
  via a feature gate, see Task L below).
- **Visual smoke**: `just yserver` for the QEMU window; for Vulkan
  inside the guest, use the Venus harness (see Task 1.2 — it adds a
  new justfile recipe that flips `-device virtio-gpu-pci` →
  `-device virtio-vga-gl,…,venus=true`).
- **Commits**: small, frequent, one logical change each. Don't squash
  on `accel`; the branch is squash-merged at the end so dev history
  doesn't reach master either way. Keeping individual commits helps
  bisecting problems on the branch.
- **Spec lookups**: section references like "design §2" point at the
  spec linked above. The plan re-states *what* to do; the spec is the
  *why*/edge cases.
- **AGENTS.md rule**: use `/codex` for self-review before opening any
  PR (none of these sub-phases is a PR; the squash is one PR at 4.1.6).

---

## Sub-phase 4.1.0 — Branch + baseline

Goal: have `accel` cut from master and record the parity-bar baseline
numbers in `docs/test-status.md`.

### Task 0.1: Confirm `kms-xts-tooling` is merged

**Step 1:** Run

```bash
git -C /home/jos/Projects/yserver log --oneline master..kms-xts-tooling
git -C /home/jos/Projects/yserver log --oneline kms-xts-tooling..master
```

**Expected:** First command empty (or only stale design-doc revision
commits already squashed into master). Second command shows master is
at-or-ahead.

**Step 2:** If first is non-empty with substantive code, stop and
escalate. Otherwise proceed.

### Task 0.2: Discard the existing empty `accel`

**Step 1:** Confirm there are no commits on `accel` worth keeping:

```bash
git -C /home/jos/Projects/yserver log --oneline master..accel
```

**Expected:** empty output, or only the placeholder commit referenced
in the design.

**Step 2:** Delete the local `accel`:

```bash
git -C /home/jos/Projects/yserver branch -D accel
```

**Step 3:** If a remote `accel` exists, leave it alone for now —
deletion of a remote branch is a destructive op needing user
confirmation.

### Task 0.3: Recreate `accel` from master tip

```bash
git -C /home/jos/Projects/yserver checkout master
git -C /home/jos/Projects/yserver pull --ff-only
git -C /home/jos/Projects/yserver checkout -b accel
```

### Task 0.4: Capture parity-bar baseline numbers

The full xts5 + rendercheck run against master/KMS is being captured by
the user on a separate machine. When those numbers arrive, paste them
into a new section of `docs/test-status.md`:

```markdown
## yserver / KMS — baseline (master, <DATE>)

xts5 (per scenario, PASS / total): … (paste)
rendercheck (per test, PASS / total): … (paste)
```

These numbers are the parity bar (design §4: ±5 PASS on xts5,
match-or-beat on rendercheck).

**Commit:**

```bash
git add docs/test-status.md
git commit -m "docs(test-status): record yserver/KMS parity-bar baseline for phase 4.1"
```

### Task 0.5: Push `accel`

```bash
git push -u origin accel
```

(Sandbox push uses the SSH wrapper — see memory
`feedback_git_push_in_sandbox.md`.)

---

## Sub-phase 4.1.1 — Vulkan plumbing, idle

Goal: `KmsBackend::new` brings up a Vulkan instance/device/allocator,
prints device info, tears down cleanly. **yserver still renders 100%
via pixman.** xts/rendercheck must be unchanged after this sub-phase.

The deliverable is "Vulkan is loaded and idle alongside pixman." No
pixman code is removed. No drawing path is changed.

### Task 1.1: Add Cargo dependencies

**File:** `Cargo.toml` (workspace)

**Step 1:** Add to `[workspace.dependencies]`:

```toml
ash = "0.38"
gpu-allocator = { version = "0.27", default-features = false, features = ["vulkan"] }
gbm = "0.18"
```

(Pin minor versions; bump only with the sub-phase that needs the
newer features.)

**File:** `crates/yserver/Cargo.toml`

**Step 2:** Add to the `[dependencies]` block:

```toml
ash.workspace = true
gpu-allocator.workspace = true
gbm.workspace = true
```

**Step 3:** Build to confirm no resolution errors:

```bash
cargo build --bin yserver
```

**Expected:** clean build, dependencies vendored.

**Step 4:** Commit.

```bash
git add Cargo.toml Cargo.lock crates/yserver/Cargo.toml
git commit -m "build(deps): add ash, gpu-allocator, gbm for kms vulkan backend"
```

### Task 1.2: Add justfile recipe for the Vulkan vng harness

The existing `just yserver` recipe uses `virtio-gpu-pci`, which
exposes no Vulkan device. Add a recipe that runs yserver under the
Venus passthrough harness so we can smoke-test Vulkan paths.

**File:** `Justfile`

**Step 1:** Append a new recipe (model after the existing
`yserver-debug` recipe + the `vulkan-check-venus` Vulkan harness):

```make
# Phase 4.1: yserver under virtio-gpu Venus passthrough.
# Exposes a real Vulkan device inside the guest. Requires
# `vulkan-virtio` on the host (Venus ICD).
yserver-venus mode="1024x768" log="info":
    cargo build --bin yserver
    vng -r {{KERNEL}} --disable-microvm --rw \
        --qemu-opts="-display gtk,gl=on -vga none -device virtio-vga-gl,hostmem=4G,blob=true,venus=true -device virtio-tablet-pci -device virtio-keyboard-pci" \
        -- bash -c 'RUST_LOG="{{log}}" RUST_BACKTRACE=1 YSERVER_MODE={{mode}} target/debug/yserver'
```

**Step 2:** Confirm the recipe parses:

```bash
just --list | grep yserver-venus
```

**Expected:** the recipe is listed.

**Step 3:** Commit.

```bash
git add Justfile
git commit -m "build(just): add yserver-venus recipe for vulkan-enabled vng"
```

### Task 1.3: Create the `kms/vk/` module skeleton

**Files (create, all empty stubs except `mod.rs`):**

- `crates/yserver/src/kms/vk/mod.rs`
- `crates/yserver/src/kms/vk/instance.rs`
- `crates/yserver/src/kms/vk/device.rs`
- `crates/yserver/src/kms/vk/memory.rs`

**Step 1:** Write `vk/mod.rs`:

```rust
//! Vulkan backend for the KMS compositor (Phase 4.1).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//!
//! Sub-phase 4.1.1: instance/device/allocator init, idle. Drawing
//! still runs through pixman; this module brings up Vulkan in
//! parallel.

pub mod device;
pub mod instance;
pub mod memory;
```

**Step 2:** Write three other stubs as empty modules (one-line
`//!` doc each).

**Step 3:** Wire into `kms/mod.rs`:

```rust
pub mod vk;
```

**Step 4:** Build.

```bash
cargo build --bin yserver
```

**Expected:** clean build.

**Step 5:** Commit.

```bash
git add crates/yserver/src/kms/mod.rs crates/yserver/src/kms/vk/
git commit -m "feat(kms/vk): scaffold module tree (instance/device/memory stubs)"
```

### Task 1.4: TDD — pure-logic helper for required-extension list

The instance creation needs a fixed list of required extensions. Write
the list-building as a pure function so it's unit-testable.

**File:** `crates/yserver/src/kms/vk/instance.rs`

**Step 1:** Write the failing test (append to `instance.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_instance_extensions_includes_external_memory_fd() {
        let ext = required_instance_extensions();
        assert!(ext.contains(&ash::khr::external_memory_capabilities::NAME));
        assert!(ext.contains(&ash::khr::external_semaphore_capabilities::NAME));
        assert!(ext.contains(&ash::ext::debug_utils::NAME));
    }
}
```

**Step 2:** Run.

```bash
cargo test -p yserver kms::vk::instance::tests::required_instance_extensions_includes_external_memory_fd
```

**Expected:** FAIL — `required_instance_extensions` not defined.

**Step 3:** Implement.

```rust
use std::ffi::CStr;

/// Instance extensions we always request. Selection rationale: external
/// memory + external semaphore for KMS pageflip handoff (sub-phase
/// 4.1.2); debug utils for the validation/messenger pass.
pub fn required_instance_extensions() -> Vec<&'static CStr> {
    vec![
        ash::khr::external_memory_capabilities::NAME,
        ash::khr::external_semaphore_capabilities::NAME,
        ash::ext::debug_utils::NAME,
    ]
}
```

**Step 4:** Re-run; expect PASS.

**Step 5:** Commit.

```bash
git add crates/yserver/src/kms/vk/instance.rs
git commit -m "feat(kms/vk): required_instance_extensions() with unit test"
```

### Task 1.5: Implement `VkContext` (instance + physical + logical device)

This task is one focused unit of work but spans several lines. Structure
it as: write the type + ctor, then a smoke test that runs only when a
real Vulkan ICD is present.

**File:** `crates/yserver/src/kms/vk/device.rs`

**Step 1:** Define `VkContext`:

```rust
use ash::vk;
use std::sync::Arc;

/// Lives for the entire backend lifetime. Drop order matters: device
/// before instance; instance-level loaders before instance.
///
/// Extension loaders (`debug_utils_instance`, `external_semaphore_fd`)
/// must be stored, not reconstructed per call: the underlying ash
/// loader resolves function pointers via `vkGetInstanceProcAddr` /
/// `vkGetDeviceProcAddr` once and caches them. Drop also goes through
/// the loader (`destroy_debug_utils_messenger`).
pub struct VkContext {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub debug_utils_instance: ash::ext::debug_utils::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: ash::Device,
    pub external_semaphore_fd: ash::khr::external_semaphore_fd::Device,
    pub graphics_queue_family: u32,
    pub graphics_queue: vk::Queue,
    pub debug_messenger: Option<vk::DebugUtilsMessengerEXT>,
}

impl VkContext {
    pub fn new() -> Result<Arc<Self>, VkInitError> {
        // … per-step body in subsequent tasks …
        unimplemented!()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VkInitError {
    #[error("vulkan loader: {0}")]
    Loader(#[from] ash::LoadingError),
    #[error("vulkan: {0}")]
    Vk(vk::Result),
    #[error("no suitable physical device (need graphics queue + drm format modifier ext)")]
    NoSuitableDevice,
}

impl From<vk::Result> for VkInitError {
    fn from(r: vk::Result) -> Self { VkInitError::Vk(r) }
}
```

**Step 2:** Add `thiserror` to workspace deps if not present (check
`Cargo.toml`); reuse if already present elsewhere.

**Step 3:** Build, confirm it compiles with `unimplemented!()` body.

**Step 4:** Commit the skeleton.

```bash
git add crates/yserver/src/kms/vk/
git commit -m "feat(kms/vk): VkContext shell + VkInitError"
```

### Task 1.6: Implement `VkContext::new` instance creation

**File:** `crates/yserver/src/kms/vk/device.rs`

**Step 1:** Replace the `unimplemented!()` with the instance-creation
sequence — load entry, build `vk::ApplicationInfo` (name "yserver",
api_version 1.3), enable instance extensions from
`required_instance_extensions()`, enable validation in debug builds
only:

```rust
pub fn new() -> Result<Arc<Self>, VkInitError> {
    let entry = unsafe { ash::Entry::load()? };
    let app_info = vk::ApplicationInfo::default()
        .application_name(c"yserver")
        .application_version(vk::make_api_version(0, 0, 1, 0))
        .engine_name(c"yserver-kms")
        .api_version(vk::API_VERSION_1_3);

    let ext_cstrs = super::instance::required_instance_extensions();
    let ext_ptrs: Vec<_> = ext_cstrs.iter().map(|c| c.as_ptr()).collect();

    let layer_ptrs: Vec<*const i8> = if cfg!(debug_assertions) {
        vec![c"VK_LAYER_KHRONOS_validation".as_ptr()]
    } else {
        Vec::new()
    };

    let create_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .enabled_extension_names(&ext_ptrs)
        .enabled_layer_names(&layer_ptrs);

    let instance = unsafe { entry.create_instance(&create_info, None)? };

    // Physical device + logical device come in the next task — leave a
    // placeholder for now that drops the instance and returns Err.
    unsafe { instance.destroy_instance(None) };
    Err(VkInitError::NoSuitableDevice)
}
```

**Step 2:** Build.

```bash
cargo build --bin yserver
```

**Expected:** clean build.

**Step 3:** Commit.

```bash
git add crates/yserver/src/kms/vk/device.rs
git commit -m "feat(kms/vk): VkContext::new instance creation"
```

### Task 1.7: Add physical-device selection

**File:** `crates/yserver/src/kms/vk/device.rs`

Selection rule: pick the first DEVICE_TYPE_DISCRETE_GPU; fall back to
first INTEGRATED_GPU; fall back to first CPU/lavapipe (so test runs
under lavapipe still get a device). Reject any device that doesn't
expose a graphics+transfer queue family.

**Step 1:** Add a private helper:

```rust
fn pick_physical_device(
    instance: &ash::Instance,
) -> Result<(vk::PhysicalDevice, u32), VkInitError> {
    let devices = unsafe { instance.enumerate_physical_devices() }?;

    let mut scored: Vec<(u32, vk::PhysicalDevice, u32)> = devices
        .into_iter()
        .filter_map(|pd| {
            let props = unsafe { instance.get_physical_device_properties(pd) };
            let queue_family = pick_graphics_queue_family(instance, pd)?;
            let score = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                vk::PhysicalDeviceType::VIRTUAL_GPU => 1,
                _ => 0,
            };
            Some((score, pd, queue_family))
        })
        .collect();
    scored.sort_by_key(|t| std::cmp::Reverse(t.0));
    scored.into_iter().next()
        .map(|(_, pd, qf)| (pd, qf))
        .ok_or(VkInitError::NoSuitableDevice)
}

fn pick_graphics_queue_family(
    instance: &ash::Instance, pd: vk::PhysicalDevice,
) -> Option<u32> {
    let qfp = unsafe { instance.get_physical_device_queue_family_properties(pd) };
    qfp.iter().enumerate().find_map(|(i, p)| {
        if p.queue_flags.contains(vk::QueueFlags::GRAPHICS | vk::QueueFlags::TRANSFER) {
            Some(i as u32)
        } else {
            None
        }
    })
}
```

**Step 2:** Wire into `VkContext::new` — remove the placeholder Err
and replace with a call to `pick_physical_device`. Save physical_device
+ queue_family for the next task.

**Step 3:** Build. Commit.

```bash
git commit -am "feat(kms/vk): physical device selection (discrete > integrated > virtual)"
```

### Task 1.8: Add logical-device + queue creation

**File:** `crates/yserver/src/kms/vk/device.rs`

**Step 1:** Inside `VkContext::new`, after physical-device selection:

```rust
// VK_KHR_external_memory_fd is required (per Vulkan spec) by
// VK_EXT_external_memory_dma_buf, which is how GBM bos enter the
// device as importable memory. Do not drop it.
//
// VK_KHR_swapchain is intentionally NOT requested: WSI is out of
// scope for Phase 4.1 (design §1, "Out of scope"). KMS pageflip is
// our presentation path.
let device_extensions = [
    ash::khr::external_memory_fd::NAME.as_ptr(),
    ash::ext::external_memory_dma_buf::NAME.as_ptr(),
    ash::ext::image_drm_format_modifier::NAME.as_ptr(),
    ash::khr::external_semaphore_fd::NAME.as_ptr(),
    ash::khr::dynamic_rendering_local_read::NAME.as_ptr(),
];

let priorities = [1.0_f32];
let queue_info = [vk::DeviceQueueCreateInfo::default()
    .queue_family_index(graphics_queue_family)
    .queue_priorities(&priorities)];

let mut features13 = vk::PhysicalDeviceVulkan13Features::default()
    .dynamic_rendering(true)
    .synchronization2(true);

// Required by `VK_KHR_dynamic_rendering_local_read`. Enabling the
// extension by name is not enough — conformant drivers gate the
// feature on this struct being chained and `dynamic_rendering_local_read`
// being explicitly toggled on. ShaderRMW (Disjoint/Conjoint, sub-phase
// 4.1.4.6) silently breaks without this.
let mut features_local_read =
    vk::PhysicalDeviceDynamicRenderingLocalReadFeaturesKHR::default()
        .dynamic_rendering_local_read(true);

let device_info = vk::DeviceCreateInfo::default()
    .queue_create_infos(&queue_info)
    .enabled_extension_names(&device_extensions)
    .push_next(&mut features13)
    .push_next(&mut features_local_read);

let device = unsafe {
    instance.create_device(physical_device, &device_info, None)?
};
let graphics_queue = unsafe { device.get_device_queue(graphics_queue_family, 0) };
let external_semaphore_fd =
    ash::khr::external_semaphore_fd::Device::new(&instance, &device);

// Pre-flight check: query the feature, abort with a clear error if
// the picked physical device doesn't support it (would have happened
// already in `pick_physical_device` if we'd asked there — Task 1.7
// note: extend the picker to require this feature).
//
// Hold onto `external_semaphore_fd` and (later) the debug_utils
// loader by storing them on `VkContext`.
```

**Step 2:** Construct and return `Arc<VkContext>`.

**Step 3:** Create the debug messenger (debug builds only) and the
`debug_utils` loader. The loader must be constructed *before* the
messenger so we can call `create_debug_utils_messenger_ext`. Build
this immediately after the instance create (logically belongs to
Task 1.6, but the field on `VkContext` was added in Task 1.5):

```rust
let debug_utils_instance =
    ash::ext::debug_utils::Instance::new(&entry, &instance);

let debug_messenger = if cfg!(debug_assertions) {
    let info = vk::DebugUtilsMessengerCreateInfoEXT::default()
        .message_severity(
            vk::DebugUtilsMessageSeverityFlagsEXT::WARNING
                | vk::DebugUtilsMessageSeverityFlagsEXT::ERROR,
        )
        .message_type(
            vk::DebugUtilsMessageTypeFlagsEXT::GENERAL
                | vk::DebugUtilsMessageTypeFlagsEXT::VALIDATION
                | vk::DebugUtilsMessageTypeFlagsEXT::PERFORMANCE,
        )
        .pfn_user_callback(Some(vk_debug_callback));
    Some(unsafe { debug_utils_instance.create_debug_utils_messenger(&info, None)? })
} else {
    None
};
```

Add a top-level `vk_debug_callback` that routes
`pMessage` → `log::warn!`/`log::error!` based on severity (no-op for
INFO/VERBOSE — too noisy).

**Step 4:** Implement `Drop for VkContext` in this exact order
(reverse of construction):

```rust
impl Drop for VkContext {
    fn drop(&mut self) {
        unsafe {
            // Wait for all queue work; tearing down with in-flight CBs
            // is undefined behaviour.
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            if let Some(m) = self.debug_messenger.take() {
                self.debug_utils_instance.destroy_debug_utils_messenger(m, None);
            }
            self.instance.destroy_instance(None);
        }
    }
}
```

The `entry` is `Arc`-held by ash and drops automatically after the
instance.

**Step 5:** Build. Commit.

```bash
git commit -am "feat(kms/vk): logical device, debug messenger, drop order"
```

### Task 1.9: Wire `VkContext::new` into `KmsBackend::new`

**File:** `crates/yserver/src/kms/backend.rs`

**Step 1:** Locate `KmsBackend::new` (it's near the top of the
struct's impl block; backend.rs is the 7500-line monolith).

**Step 2:** Add a field:

```rust
pub struct KmsBackend {
    // … existing fields …
    pub(crate) vk: Arc<crate::kms::vk::device::VkContext>,
}
```

**Step 3:** In `KmsBackend::new`, after the existing DRM/GBM/pixman
init, call:

```rust
let vk = crate::kms::vk::device::VkContext::new()
    .map_err(|e| io::Error::other(format!("vulkan init: {e}")))?;
log::info!("vulkan initialised on physical device {}",
    unsafe { vk.instance.get_physical_device_properties(vk.physical_device) }
        .device_name_as_c_str().unwrap_or(c"<unknown>").to_string_lossy());
```

**Step 4:** Build. Commit.

```bash
git commit -am "feat(kms/backend): hold VkContext alongside the pixman path"
```

### Task 1.10: Smoke test on lavapipe

**Step 1:** Inside vng, run:

```bash
just vulkan-check-lavapipe
```

(Confirm Vulkan loader works — a separate concern from yserver itself.)

**Step 2:** Run yserver under Venus:

```bash
just yserver-venus log=info
```

**Expected:** "vulkan initialised on physical device …" line in the
log; window comes up; pixman still renders the bouncing rect / WM /
xterm if launched. Clean shutdown via SIGTERM.

**Step 3:** If anything fails, debug. **Do not commit fixes inside
this task** — open new tasks per fix to keep the bisect history clean.

### Task 1.11: Sub-phase parity check

**Step 1:** Run the full-rendering parity sweep against the parity
bar (Task 0.4 numbers):

```bash
just rendercheck-yserver
just xts-yserver scenario=Xproto
just xts-yserver scenario=ShapeExt
```

**Expected:** numbers match Task 0.4's baseline within noise (±2
PASS). If a regression shows up, it's a bug in the Vulkan-init path
disturbing pixman's environment — investigate before moving on.

**Step 2:** If green, mark sub-phase 4.1.1 complete in
`docs/status.md`:

```bash
git commit -am "docs(status): phase 4.1.1 (vulkan plumbing, idle) complete"
```

---

## Sub-phase 4.1.2 — Vulkan-fed scanout

Goal: replace the existing pixman→KMS path with a Vulkan blit pass that
copies the pixman shadow buffer into a GBM-backed `VkImage`, then
pageflips with explicit fences (`IN_FENCE_FD` ↔
`OUT_FENCE_PTR`). **Pixman still draws every pixel** — we're swapping
out *only* the "shadow buffer → display" stage.

This is the single hardest plumbing sub-phase (the explicit-fence state
machine from design §2 lands here in full). Estimate 1-2 sessions.

### Task 2.1: TDD — bo state machine helpers

The per-bo state machine (Free / Recording / Submitted / Pending /
OnScreen / Retiring) and its transition rules (design §2 table) are
pure logic; test them on host before the Vulkan code lands.

**File:** `crates/yserver/src/kms/vk/scanout.rs` (new)

**Step 1:** Write the test first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_bo_is_free() {
        let bo = BoState::default();
        assert_eq!(bo.phase, BoPhase::Free);
        assert!(bo.in_fence_fd.is_none());
        assert!(bo.release_fence_fd.is_none());
    }

    #[test]
    fn record_then_submit_transitions_to_submitted() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        assert_eq!(bo.phase, BoPhase::Recording);
        bo.transition_to_submitted(/* in_fence */ 42);
        assert_eq!(bo.phase, BoPhase::Submitted);
        assert_eq!(bo.in_fence_fd, Some(42));
    }

    #[test]
    fn atomic_accept_transfers_in_fence_to_kernel_and_stores_out_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(42);
        bo.transition_to_pending(/* out_fence */ 99);
        assert_eq!(bo.phase, BoPhase::Pending);
        assert!(bo.in_fence_fd.is_none(), "kernel owns IN_FENCE_FD now");
        assert_eq!(bo.release_fence_fd, Some(99));
    }

    #[test]
    fn atomic_reject_returns_to_recording_and_we_still_own_in_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(42);
        let reclaimed = bo.transition_to_recording_after_atomic_reject();
        assert_eq!(bo.phase, BoPhase::Recording);
        assert_eq!(reclaimed, Some(42), "caller closes the fd");
    }
}
```

**Step 2:** Run; expect FAIL (types/methods don't exist).

**Step 3:** Implement the minimum:

```rust
#[derive(Debug, Default)]
pub struct BoState {
    pub phase: BoPhase,
    pub in_fence_fd: Option<i32>,
    pub release_fence_fd: Option<i32>,
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub enum BoPhase {
    #[default] Free,
    Recording,
    Submitted,
    Pending,
    OnScreen,
    Retiring,
}

impl BoState {
    pub fn transition_to_recording(&mut self) {
        debug_assert_eq!(self.phase, BoPhase::Free);
        self.phase = BoPhase::Recording;
    }
    pub fn transition_to_submitted(&mut self, in_fence_fd: i32) {
        debug_assert_eq!(self.phase, BoPhase::Recording);
        self.phase = BoPhase::Submitted;
        self.in_fence_fd = Some(in_fence_fd);
    }
    pub fn transition_to_pending(&mut self, out_fence_fd: i32) {
        debug_assert_eq!(self.phase, BoPhase::Submitted);
        self.phase = BoPhase::Pending;
        self.in_fence_fd = None;            // kernel owns it now
        self.release_fence_fd = Some(out_fence_fd);
    }
    /// Return the fence fd the caller must close.
    pub fn transition_to_recording_after_atomic_reject(&mut self) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Submitted);
        self.phase = BoPhase::Recording;
        self.in_fence_fd.take()
    }
}
```

**Step 4:** Run; expect PASS.

**Step 5:** Add tests for OnScreen and Retiring transitions, then
implement those methods. Same TDD cycle.

**Step 6:** Commit when the table from design §2 is fully covered.

```bash
git commit -am "feat(kms/vk/scanout): bo state machine + transitions (host-tested)"
```

### Task 2.2: GBM bo allocation

**File:** `crates/yserver/src/kms/vk/scanout.rs`

**Step 1:** Define `ScanoutBo` (pairs a GBM bo with the bo state and
its `VkImage`):

```rust
pub struct ScanoutBo {
    pub gbm_bo: gbm::BufferObject<()>,
    pub image: vk::Image,
    pub allocation: gpu_allocator::vulkan::Allocation,
    pub state: BoState,
    pub semaphore: vk::Semaphore,   // long-lived; payload churns
    pub width: u32,
    pub height: u32,
}
```

**Step 2:** Allocate three of them at backend init, keyed by CRTC.
Read the existing GBM init in `kms/backend.rs` (search for `gbm`) and
mirror the bo-allocation pattern but for the new
`VK_EXT_image_drm_format_modifier` path (import the bo's dma-buf as a
`VkImage` via `VK_EXT_external_memory_dma_buf`).

**Step 3:** Visual smoke under `just yserver-venus`. Pixmap path still
runs; this code path doesn't run yet. Confirms allocation succeeds at
init.

**Step 4:** Commit.

### Task 2.3: Long-lived `VkSemaphore` per bo (export-fd capable)

**File:** `crates/yserver/src/kms/vk/scanout.rs`

**Step 1:** At `ScanoutBo` construction, create a binary semaphore
with `VkExportSemaphoreCreateInfo`:

```rust
let mut export_info = vk::ExportSemaphoreCreateInfo::default()
    .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
let create_info = vk::SemaphoreCreateInfo::default().push_next(&mut export_info);
let semaphore = unsafe { device.create_semaphore(&create_info, None)? };
```

**Step 2:** Implement `export_signaled_fd(&self,
ext: &ash::khr::external_semaphore_fd::Device) -> Result<OwnedFd>`
wrapping `vkGetSemaphoreFdKHR` — used after each submit to extract a
fresh fd payload. Real coverage of this call comes from the Task 2.7
lavapipe integration test (the fence-cycle test exercises export +
close on every iteration). No host-side TDD here — the call has no
pure-logic surface to test red-green.

**Step 3:** Add a `#[cfg(test)] fn _compile_check_signature` that
constructs all the argument types so signature drift breaks the
build immediately rather than waiting on the integration test run.

**Step 4:** Commit.

### Task 2.4: Dummy single-frame composite that copies pixman shadow → bo

**Files:** `crates/yserver/src/kms/vk/scanout.rs`,
`crates/yserver/src/kms/vk/compositor.rs` (new).

**Step 1:** Write `compositor::blit_pixman_shadow_to_bo` — record a
graphics CB that uploads the host-mapped pixman shadow buffer into the
target `ScanoutBo.image` via `vkCmdCopyBufferToImage` (staging buffer
→ image), then transitions the image layout to
`VK_IMAGE_LAYOUT_PRESENT_SRC_KHR`.

**Step 2:** Add a backend-level switch (`VkScanoutMode::PixmanShadow`)
gating the new path; off by default for now.

**Step 3:** Test under `just yserver-venus`. Verify rect-bouncer still
renders (pixman is still doing the drawing; we're just shipping
through a Vulkan blit on the way to scanout — yet to be wired into
KMS).

**Step 4:** Commit.

### Task 2.5: Atomic-commit explicit-fence path

This is the hairy bit. Read design §2, "Per scanout / per CRTC,"
table of transitions, before starting.

**Files (existing — these are where the atomic_commit calls live):**
- `crates/yserver/src/drm/page_flip.rs:35` — per-frame pageflip
  (the one that needs `IN_FENCE_FD` / `OUT_FENCE_PTR`).
- `crates/yserver/src/drm/swapchain.rs` — bo rotation orchestration;
  this is where the `ScanoutBo` state machine plugs in.
- `crates/yserver/src/drm/modeset.rs:399, :462` — modeset commit
  (different code path; touched in Task 2.6 for hot-config).

**Files (new):** add explicit-fence helpers to
`crates/yserver/src/kms/vk/scanout.rs`. The DRM atomic-property
plumbing stays in `crates/yserver/src/drm/page_flip.rs` — extend the
existing helper rather than duplicate the property-name lookups.

**Step 1:** Extend the per-frame pageflip helper in
`drm/page_flip.rs` to take an `in_fence_fd: i32` and an
`out_fence_ptr: *mut i32` argument; set them as plane properties
(`IN_FENCE_FD` on the primary plane) and CRTC properties
(`OUT_FENCE_PTR` on the CRTC) respectively. Property-handle lookups
follow the pattern already in `drm/modeset.rs:413-415` (`PropMap::for_object`).

**Step 2:** In `drm/swapchain.rs`, change the bo-rotation loop to:
  - pull the next-free `ScanoutBo`,
  - record + submit the composite CB with `signalSemaphore =
    bo.semaphore`,
  - export the signaled fd via `external_semaphore_fd.get_semaphore_fd()`,
  - call the extended page_flip helper with `IN_FENCE_FD` set + a
    pointer for `OUT_FENCE_PTR`,
  - on accept (rc=0): adopt the returned out-fence as
    `release_fence_fd`, transition Submitted→Pending,
  - on reject (-EBUSY): close the still-payloaded fence fd, free the
    CB, return to Recording — re-record on the next frame iteration.

**Step 3:** Wire pageflip-complete event → `Pending → OnScreen`
transition. The dispatcher already exists at `drm/page_flip.rs:43`
(`receive_events` + `dispatch_event`). Extend the `on_page_flip`
callback signature to allow the caller to advance bo state.

**Step 4:** Wire next-flip-complete → `OnScreen → Retiring → Free`.

**Step 5:** Test under `just yserver-venus` for at least 60 seconds.
Watch for any -EBUSY in logs (rare but expected on hot-config).

**Step 6:** Commit each layer separately so bisect can isolate.

### Task 2.6: Modeset / hot-config path

**Files:**
- `crates/yserver/src/drm/modeset.rs:408` (`commit_modeset`) and
  `crates/yserver/src/drm/modeset.rs:382` (`disable_output`) — call
  sites that the hot-config drain hooks into.
- `crates/yserver/src/kms/vk/scanout.rs` — bo-state drain logic.

**Step 1:** When the existing modeset code path runs (CRTC reconfig,
hotplug, mode change), enumerate every bo by phase and execute the
"Modeset / hot-config events" rules from design §2:
- Recording → drop CB.
- Submitted → host-wait on the GPU CB completion fence, close
  in_fence_fd, drop.
- Pending/OnScreen/Retiring → wait release fence, close, drop.
- Free fresh bos with new dimensions/modifier; reset state machine.

**Step 2:** Hot-config smoke: `just yserver-multihead`, swap monitor
config mid-flight (see existing multi-monitor recipe in Justfile).

**Step 3:** Commit.

### Task 2.7: lavapipe integration test — scanout fence cycle

**File:** `crates/yserver/src/kms/vk/scanout.rs`

Per design §3:

```rust
#[cfg(all(test, feature = "lavapipe-tests"))]
mod fence_cycle_test {
    use super::*;

    #[test]
    fn six_frames_release_fences_signal_in_order() {
        // Allocate 3 bos, submit 6 dummy frames, assert each release
        // fence is signalled before the bo is reused.
        // Spec: design §3, "Scanout fence cycle".
        // …
    }
}
```

**Step 1:** Write the test (pure-logic version first; mock the GPU
side if Vulkan creation under lavapipe is awkward inside `cargo
test`).

**Step 2:** Add `[features] lavapipe-tests = []` to
`crates/yserver/Cargo.toml`.

**Step 3:** Run with `cargo test -p yserver --features lavapipe-tests`
locally, expect PASS.

**Step 4:** Commit.

### Task 2.8: Sub-phase parity check

Same as Task 1.11. xts + rendercheck must match the parity bar.
Visual smoke under `just yserver-venus` and the WM matrix.

**Commit:** `docs(status): phase 4.1.2 (vulkan-fed scanout) complete`

---

## Sub-phase 4.1.3 — Scene-graph compositor parallel to pixman

Goal: each X window/pixmap gets a `VkImage` mirror alongside its pixman
image. Drawing ops still route through pixman; results upload to the
mirror on damage. Composite pass (introduced in 4.1.2) reads
**per-window VkImages** instead of the unified shadow buffer.

This is the architectural checkpoint. After this lands, every
remaining sub-phase is a mechanical drawing-op port — no architectural
changes downstream.

### Task 3.1: Define `DrawableImage` (server-owned variant only)

**File:** `crates/yserver/src/kms/vk/target.rs` (new)

**Step 1:** Per design §2 ("Per X resource — `DrawableImage`
abstraction"), define the type with the `ImageBacking` enum and **all
three** constructors. Only the first two are *called* in 4.1.1-4.1.4;
`from_dmabuf` exists from day one so 4.2 doesn't have to retrofit a
new ctor onto a sealed type.

```rust
pub struct DrawableImage {
    pub vk_image: vk::Image,
    pub extent: vk::Extent2D,
    pub format: vk::Format,
    pub backing: ImageBacking,
    // sampler/descriptor caches and damage region added incrementally
}

pub enum ImageBacking {
    ServerOwned { allocation: gpu_allocator::vulkan::Allocation },
    Imported {
        dma_buf_fd: std::os::fd::OwnedFd,
        // modifier, plane offsets, sync state — added when 4.2 starts
    },
}

impl DrawableImage {
    pub fn new_server_owned_window(ctx: &VkContext, w: u32, h: u32) -> Result<Self, /*…*/>;
    pub fn new_server_owned_pixmap(ctx: &VkContext, w: u32, h: u32, depth: u8)
        -> Result<Self, /*…*/>;
    pub fn from_dmabuf(/* … */) -> Result<Self, /*…*/>;  // 4.2: not called yet
}
```

**Step 2:** Add a `#[cfg(test)] fn _compile_check_from_dmabuf` that
references all `from_dmabuf` parameter types so signature drift
breaks the build. This is a compile-only check — `from_dmabuf` has
no real call site until Phase 4.2 (DRI3), so there's no host-side
red-green test to write here. The constructor exists from day one
so 4.2 doesn't need to retrofit a sealed type.

**Step 3:** Commit.

### Task 3.2: Add a `DrawableImage` mirror per window/pixmap

**File:** `crates/yserver/src/kms/backend.rs`. The two structs to
extend are `WindowState` (around `backend.rs:798`) and `PixmapState`
(around `backend.rs:832`). **Both** must grow the mirror — the design
puts windows and pixmaps on equal footing as GPU-image owners.

**Step 1:** Extend both structs with a sibling field:

```rust
struct WindowState {
    // existing: image: RefCell<PixmanImage>, …
    vk_mirror: DrawableImage,                         // built via new_server_owned_window
}

struct PixmapState {
    // existing: image: PixmanImage, depth: u8, …
    vk_mirror: DrawableImage,                         // built via new_server_owned_pixmap
}
```

`WindowState`'s pixman image is wrapped in `RefCell`; the mirror does
not need that wrapping (Vulkan-side mutation is gated by command-buffer
recording, not by Rust borrow checking).

**Step 2:** At creation (`CreateWindow`/`CreatePixmap`), allocate the
mirror via the appropriate `new_server_owned_*` constructor. For
`WindowState`, format is fixed `B8G8R8A8_UNORM`; for `PixmapState`,
derive format from `depth` per design §2.

**Step 3:** At destruction (`DestroyWindow`/`FreePixmap`), free the
mirror image.

**Step 4:** Build. The mirror is unused at this point; no behaviour
change. Smoke under `yserver-venus`. Commit.

### Task 3.3: Damage-driven upload from pixman shadow → mirror

**File:** `crates/yserver/src/kms/vk/target.rs` and
`crates/yserver/src/kms/backend.rs` (drawing-op call sites).

**Step 1:** After every pixman drawing op that mutates a window or
pixmap image, mark the damaged rect on the corresponding
`DrawableImage`.

**Step 2:** Before the composite pass runs, walk dirty mirrors and
upload damaged rects from the pixman backing buffer (host-mapped) into
the `vk_image` via `vkCmdCopyBufferToImage`.

**Step 3:** Smoke test: rect bouncer + WM + xterm show identical
output to pre-3.3. Pixman renders; Vulkan mirrors.

**Step 4:** Commit.

### Task 3.4: Composite pass walks mirrors instead of shadow

**File:** `crates/yserver/src/kms/vk/compositor.rs`

**Step 1:** Replace the 4.1.2 "blit pixman shadow to scanout bo"
single-pass with a multi-window pass that walks the window tree (read
`yserver-core`'s tree — design §"Key invariants") and draws one quad
per visible window sampling that window's `vk_mirror`.

**Step 2:** Implement per-window AABB cull (design §"Frame composite
pass" step 2) and back-to-front occlusion cull (step 3).

**Step 3:** Implement scissor on `frame_damage` (step 4).

**Step 4:** WM matrix smoke under `yserver-venus`: e16, Window Maker.
fvwm3 known-broken.

**Step 5:** xts + rendercheck parity check.

**Step 6:** Commit each step separately.

### Task 3.5: lavapipe integration test — single-window draw + composite

Per design §3, "Single-window draw" smoke. Direct port of the Phase
4.1.1 plumbing test.

### Task 3.6: Sub-phase parity check

xts + rendercheck against parity bar. Visual: rect bouncer, e16+xterm,
wmaker+xterm. Commit `docs/status.md` update.

---

## Sub-phase 4.1.4 — Drawing op port, family by family

Goal: replace pixman drawing ops with Vulkan equivalents one *family*
at a time. After each family lands, that family's pixman code is
deleted; no `cfg!` toggles, no parallel paths.

**Plan-of-plans note.** Each family port below is itself a 1-2 session
chunk. Rather than enumerating bite-sized TDD tasks for all nine
families upfront (would be stale long before half ship), **write a
short focused mini-plan in `docs/superpowers/plans/` at the start of
each sub-task**, named e.g. `2026-MM-DD-phase4-1-4-N-<family>.md`.
Use the design spec's "RENDER attribute matrix" / "PictOp values" /
"RENDER pipeline-key model" sections as the per-family scope sheet.

The build order below is the design's ordering (§5, sub-phase 4.1.4).
Re-order only with strong reason — the order is chosen to minimise
blast radius (simplest/most-tested first).

### 4.1.4.1 — `PolyFillRectangle` + `ClearArea`

Solid-fill is the simplest case. One pipeline (FixedBlend, src=Src,
no mask, no transform). Establishes the per-target batch CB pattern
(design §"Data flow"). After this lands, the rendercheck `fill` test
must pass against the Vulkan path.

Files: new `crates/yserver/src/kms/vk/ops/fill.rs`, modify
`backend.rs` `poly_fill_rectangle` and `clear_area` call sites.
Delete the corresponding pixman code in `backend.rs` once the Vulkan
path passes rendercheck `fill`.

**Minimum GC clip support landing here.** rendercheck `fill` exercises
GC clip-rectangles. Implement the Region clip (scissor rectangles on
the draw pass) and the None case in this slot. Pixmap clip masks +
tiles/stipples/plane-mask come in 4.1.4.8.

### 4.1.4.2 — `CopyArea` + `CopyPlane`

Cross-target draws (per-frame DAG of dependencies, design §"Cross-target
draws") and same-target overlap (staging-image hazard, design §"Same-target
overlap"). lavapipe integration test from design §3 ("Same-target
overlap CopyArea") lands in this sub-task.

### 4.1.4.3 — `PutImage` + `GetImage` + MIT-SHM

Both regular and MIT-SHM v1.2 paths. SHM is a host-mapped staging
buffer + `vkCmdCopyBufferToImage`; no zero-copy (that's Phase 4.2).

**Protocol entry points in scope (design §1):**
- Core: `PutImage`, `GetImage`.
- MIT-SHM v1.2: `MitShmAttach` (minor 1, legacy `Attach`),
  `MitShmAttachFd`, `MitShmDetach`, `MitShmPutImage`,
  `MitShmGetImage`, `MitShmCreatePixmap` (server-allocated SHM
  pixmap), `MitShmCreateSegment` (minor 7, server-allocated `memfd`).

These are distinct request handlers in `yserver-core` that route to
backend hooks. Each one needs its Vulkan-side wiring exercised — not
folded into a single "PutImage path."

`GetImage` synchronous flush-and-readback path: design §"Synchronous
reads". MIT-SHM `GetImage` reuses that path then `memcpy`s into the
client's segment.

lavapipe integration test ("MIT-SHM PutImage/GetImage round-trip" from
design §3) lands here. Add a second integration test that covers
`MitShmCreatePixmap` round-trip (server-allocated path).

### 4.1.4.4 — Lines / arcs / points

`PolyLine`, `PolySegment`, `PolyPoint`, `PolyArc`, `PolyFillArc`. All
shader-rasterised (Vulkan line primitives + a tessellated arc shader).

### 4.1.4.5 — Glyph atlas + text

`ImageText{8,16}`, `PolyText{8,16}`. FreeType still rasterises CPU-side;
new `vk/glyph.rs` owns the shared atlas (`VkImage`, grow-on-demand to
4096×4096, then evict-LRU).

### 4.1.4.6 — RENDER `Composite` + `FillRectangles` ⚠️ HIGHEST RISK

Centre of mass. Full attribute matrix from design §2 lands here:
- 13 standard PictOps (FixedBlend pipelines).
- 12 Disjoint + 12 Conjoint PictOps (ShaderRMW pipelines using
  `VK_KHR_dynamic_rendering_local_read`).
- `RenderPipelineKey` cache + lazy compile + persistent
  `~/.cache/yserver/pipeline-cache.bin`.
- `repeat`, `alpha_map`, `clip_mask`, `subwindow_mode`, `poly_edge`,
  `component_alpha` (74 component-alpha pipeline variants), filters
  (`Nearest`/`Bilinear`/`Convolution`), transforms.

Schedule **2-3 sessions**.

**Acceptance criteria (pinned at parent-plan level — do not relax in
the mini-plan):**

1. rendercheck `composite` → match-or-beat parity-bar baseline (Task
   0.4 numbers).
2. rendercheck `cacomposite` → match-or-beat parity-bar baseline.
   Specifically exercises `component_alpha` (74-pipeline variant
   space).
3. rendercheck `bug7366` → match-or-beat parity-bar baseline. This is
   the regression-test for the historical Disjoint/Conjoint operator
   bug; it's why we require rendercheck ≥ 1.6 (design §2).
4. rendercheck `repeat`, `gradients`, `triangles` → match-or-beat
   (these slot land in 4.1.4.7 but cross over because trap/tri share
   the AA shader machinery; if they regress here, fix-and-not-defer).
5. xts5 `Xlib9` (the largest scenario, 1472 tests, currently 219 PASS
   on the ynest baseline) → no regression vs. KMS baseline.
6. WM matrix smoke: e16 + Window Maker render xterm and the desktop
   cleanly under `just yserver-venus`. fvwm3 boot-but-broken-menu
   remains broken (known-issues, not load-bearing).

**Permitted fallbacks (mini-plan may pick from these only):**

- `dynamic_rendering_local_read` unsupported on a target driver →
  fall back to `VK_EXT_attachment_feedback_loop_layout` (design
  §"PictOp values"). Document the driver in `known-issues.md`.
- A specific deferred attribute combination (per design §"Key-space
  cap" table — e.g. `Convolution` filter + non-identity src
  transform) hits 0 cases in rendercheck → may stay deferred to a
  CPU-resolve fallback path. Document the deferred combinations in
  `known-issues.md`.

**Rejected fallbacks (mini-plan must NOT propose these):**

- "Skip Disjoint/Conjoint, document as known-issue." pixman currently
  serves them; rendercheck 1.6 tests them; this is a parity
  regression, not deferable.
- "Land FixedBlend ops only this slot, ShaderRMW in a separate slot
  later." Splitting the op family across slots is fine internally,
  but ALL 37 ops must work before sub-phase 4.1.4.6 is closed.

Write the dedicated mini-plan as
`docs/superpowers/plans/2026-MM-DD-phase4-1-4-6-render-composite.md`,
referencing this acceptance block.

### 4.1.4.7 — RENDER `Trapezoids` / `Triangles` / `CompositeGlyphs`

Trap/tri share the AA shader machinery (design §2 row `poly_edge`).
`CompositeGlyphs` reuses the 4.1.4.5 atlas.

### 4.1.4.8 — GC clipping / tiles / stipples / plane-mask helpers

The GC-attribute family driving every core drawing op:

- **GC clipping** (`SetClipRectangles`, `clip_mask` of None / Pixmap /
  Region). Region: scissor rectangles on the per-target draw pass.
  Pixmap (depth-1): shader path sampling the mask as `R8`, discarding
  fragments where mask=0. None: no scissor / no mask.
- **Tiles** (`tile` GC attribute): `vkSampler` with
  `VK_SAMPLER_ADDRESS_MODE_REPEAT`, source picture is the tile
  pixmap.
- **Stipples** (`stipple` GC attribute): sample as `R8`, bit-extract
  in shader, opaque fragment writes the GC foreground colour.
- **Plane mask** (`plane_mask` GC attribute): specialization constant
  on the colour-write shader; AND the channel write-out with the
  mask before storing.

Earlier families (4.1.4.1, 4.1.4.4, 4.1.4.5, 4.1.4.7) all consume
GC clipping through whatever shim 4.1.4.1 leaves in place. If 4.1.4.1
ships a "no clip" stub, every later family re-uses it; this slot
upgrades the stub to the real thing. **All earlier families must run
their rendercheck-mini-suites again after this slot lands** to catch
regressions on the now-real clipping code.

### 4.1.4.9 — `bit_gravity` resize preservation

Per-window image now lives across the full window lifetime (since 3.2
landed). `ConfigureWindow` resize must blit the preserved corner per
gravity. Design §"`bit_gravity` rect math" + "Static gravity" +
"Forget gravity" + "Background fill regions" — a lot of math, all
already worked out. lavapipe integration test ("`bit_gravity` resize
preservation") lands here.

### Inter-family checkpoints

After each family lands:
- `cargo +nightly fmt && cargo clippy && cargo test`.
- Mini rendercheck smoke (whatever tests the family touches).
- Commit, push to `accel`.

After every 2-3 families:
- Full xts + rendercheck on the branch tip vs. parity bar.
- Periodic `master → accel` merge to pick up non-KMS work (per design
  §4 branch model).

---

## Sub-phase 4.1.5 — Pixman removal

After 4.1.4.9 lands, every drawing op routes through Vulkan. Pixman
imports remain only as dead code.

### Task 5.1: Drop pixman from `crates/yserver/Cargo.toml`

```toml
# Remove:
pixman.workspace = true
```

Build will fail with hundreds of missing-symbol errors. That's the
list of code to delete.

### Task 5.2: Delete `PixmanImage` and helpers

Remove from `crates/yserver/src/kms/backend.rs`:
- `PixmanImage` newtype.
- `composite32`, `composite_trapezoids`, `composite_triangles` FFI
  helpers.
- `region_from_shape_rects`, `image_ptr_for_xid`.
- All `pixman::ffi::*` call sites (each will already be replaced by a
  Vulkan op call from sub-phase 4.1.4).
- The `use pixman::…` import line.

Delete `crates/yserver/src/kms/render.rs` (was the
"Pixman drawing helpers" module).

Update `crates/yserver/src/kms/mod.rs`:
- Remove the `pub mod render;` declaration (currently `mod.rs:5`).
  Forgetting this leaves a dangling module reference and breaks the
  build.
- Remove `PixmanImage` from the `pub use backend::{KmsBackend,
  PixmanImage};` re-export.

### Task 5.3: `cargo build --bin yserver`

**Expected:** clean build. Any residual error means a 4.1.4 family
port left a pixman call site behind — go fix it.

### Task 5.4: Final parity gate

```bash
just rendercheck-yserver
just xts-yserver scenario=Xproto
just xts-yserver scenario=Xlib3
# … all 17 scenarios per design §3 gate loop …
just xts-yserver scenario=ShapeExt
```

Plus WM matrix smoke (e16 + Window Maker pass; fvwm3 known-broken
unchanged from baseline).

If green: **Phase 4.1 done.**

```bash
git commit -am "feat(kms): remove pixman; vulkan compositor is sole drawing path"
git commit -am "docs(status): phase 4.1.5 (pixman removal) complete"
```

---

## Sub-phase 4.1.6 — Merge

### Task 6.1: Self-review

Run `/codex` review on the diff `master..accel`. Address any
load-bearing findings on `accel` itself (don't sneak fixes into the
squash).

### Task 6.2: Open PR

```bash
gh pr create --base master --head accel \
  --title "phase 4.1: vulkan compositor on KMS (replaces pixman)" \
  --body-file docs/superpowers/plans/2026-05-08-phase4-1-vulkan-compositor.md
```

(Body should reference parity-bar pass, lavapipe integration test
results, WM matrix smoke status. Edit before sending.)

### Task 6.3: Squash-merge after review

User confirms before squash (per `AGENTS.md` "squash merge when ready
(ask confirmation)"). Squash commit message includes the parity-bar
delta and any known regressions documented in `known-issues.md`.

```bash
gh pr merge --squash
```

### Task 6.4: Post-merge cleanup

```bash
git checkout master
git pull --ff-only
git branch -d accel
git push origin --delete accel  # USER-CONFIRMED only
```

Phase 4.1 done. Phase 4.2 (DRI3 + Present) starts on a fresh branch
off the new master tip.

---

## Risks and mitigations (read before starting)

- **4.1.4.6 RENDER Composite slip.** The PictOp + attribute matrix is
  the largest block of work in the phase. Mitigation: 4.1.4.7-9 do
  not depend on its completion (they continue to use pixman until
  4.1.5 erases it). If 4.1.4.6 slips, run 4.1.4.7-9 in parallel.
- **Venus Vulkan vs. lavapipe gaps.** Some extensions (e.g.
  `dynamic_rendering_local_read`) may have different driver maturity.
  If Venus rejects something lavapipe accepts (or vice-versa), trace
  per design §2 fallback path
  (`VK_EXT_attachment_feedback_loop_layout`).
- **Per-frame overhead.** A 1-pixel damage rect against 500 windows
  is still 500 quads if culling is wrong. Mitigation: per-window AABB
  + occlusion cull from 4.1.3 onward; profile with WM matrix; revisit
  if frame time blows up.
- **fence-fd leaks.** Easiest source of resource-exhaustion bugs. The
  bo state machine in 4.1.2 must keep its invariant: every transition
  that closes a fd does so exactly once. The lavapipe integration
  test from Task 2.7 is the load-bearing safety net.
- **xts/rendercheck regression that "looks like" a Vulkan AA tweak.**
  Per design §"Explicit non-goal", the test suites are the arbiter,
  not pixel equality. If a rendercheck case fails by 1-2 pixels in an
  AA edge, document in `known-issues.md` rather than chasing
  pixman-equivalent output. Real regressions (clip violations,
  blend-mode wrongness, missing pixels) must still be fixed.

---

## Reference

- Design spec:
  [`docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md`](../specs/2026-05-07-phase4-1-vulkan-compositor-design.md)
- Vulkan dev-loop reference (memory):
  `~/.claude/projects/-home-jos-Projects-yserver/memory/reference_vng_vulkan_venus.md`
- Software cursor design (folds into compositor in 4.1.3-4):
  [`docs/plans/2026-05-04-software-cursor.md`](../../plans/2026-05-04-software-cursor.md)
- Per-phase status: [`docs/status.md`](../../status.md), §"Phase 4 — Accelerated clients"
- Test status / parity bar: [`docs/test-status.md`](../../test-status.md)
