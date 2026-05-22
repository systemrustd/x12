# Deferred PRESENT completion implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace commit `8ca552a` (bee use-after-free + PRESENT wait deadlock fix) with an asynchronous deferred-completion mechanism that closes the use-after-free without the synchronous CPU wait, restores Task 3 aggregation depth on yoga, and eliminates the shutdown-hang side effect.

**Architecture:** `PRESENT::Pixmap` and `PRESENT::PixmapSynced` request handlers stop blocking. Each enqueues a `PendingPresentEntry` capturing the cow_batch fence ticket and an `Arc` clone of the underlying xshmfence / syncobj primitive (lifetime pin against `FreeSyncobj` / `XFixesDestroyFence`). The backend owns an inner `epoll` FD aggregating per-entry sync_file FDs plus a `wakeup_eventfd`; the outer main loop watches that one stable FD and drains when it fires. Drain dispatches the wake signal via the held `Arc` (not by xid lookup), then returns `CompletedPresentEvent` payloads to the loop, which fires `IdleNotify` + `CompleteNotify { mode: Copy }` to subscribed clients.

**Tech Stack:** Rust 2021, ash (Vulkan), Linux `epoll(7)` + `eventfd(2)` + `sync_file`, v2 rendering backend (`crates/yserver/src/kms/v2/`), v2's existing `FenceTicket` + `PendingCowBatch` + `PendingRenderBatch` infrastructure.

**Spec:** `docs/superpowers/specs/2026-05-23-deferred-present-completion-design.md`

**Pre-flight check:** Master HEAD must include commit `8ca552a` (the bee fix being replaced) and commit `a034d82` (the local revert that proved the design). Run `cargo build -p yserver && cargo test -p yserver --lib` to confirm baseline tests pass — the existing 379 lib tests should be green. Any pre-existing flake should be flagged before proceeding.

---

## File Map

**Created:**
- `crates/yserver/src/kms/v2/present_completion.rs` — `PendingPresentEntry`, `PinnedWake`, `KmsBackendV2` impl of `enqueue_present_completion` + `drain_completed_present_events` + inner-epoll/eventfd management.

**Modified:**
- `crates/yserver-core/src/backend/trait_def.rs` — `BackendFdKind::PresentCompletion` variant; `Backend::enqueue_present_completion` + `Backend::drain_completed_present_events` trait methods (default no-op + empty); new public types `CompletedPresentEvent` + `PresentWake`.
- `crates/yserver-core/src/core_loop/run.rs` — dispatch arm for `BackendFdKind::PresentCompletion` token; main-loop drain + emission hook.
- `crates/yserver-core/src/core_loop/process_request.rs` — `PRESENT::Pixmap` handler at ~5055 + `PRESENT::PixmapSynced` handler at ~5284 swap synchronous wait for enqueue; extract `fire_present_completion_events` helper from the current ~5384 fan-out.
- `crates/yserver/src/kms/v2/backend.rs` — new fields on `KmsBackendV2`; `dri3_xshmfence_handle` + `dri3_syncobj_handle` accessors; `dri3_trigger_fence_via_handle` + `dri3_signal_syncobj_via_handle` impls; xshmfence + syncobj storage wrapped in `Arc`; `disable_output` adds flush + drain.
- `crates/yserver/src/kms/v2/platform.rs` — `FenceTicket::export_sync_file_fd` method; `poll_fds()` adds `PresentCompletion` FD.
- `crates/yserver/src/kms/vk/device.rs` — verify / enable `VK_KHR_external_fence_fd` device extension.
- `crates/yserver/src/kms/v2/engine.rs` — new accessor `current_cow_batch_ticket() -> Option<FenceTicket>`.
- `crates/yserver/src/kms/v2/mod.rs` — register new `present_completion` module.
- `crates/yserver/src/kms/backend.rs` — v1 stub: `dri3_xshmfence_handle` + `dri3_syncobj_handle` + `*_via_handle` accessors returning errors / `None` is acceptable; v1 doesn't use the deferred path. Arc-wrap v1's storage to keep type signatures consistent across the two backends.
- `crates/yserver/tests/v2_acceptance.rs` — new acceptance tests.
- `docs/status.md` — log the deferred-completion landing under Stage 5 Task 6.1.

**Left untouched (v1 paint path):**
- v1's `KmsBackend` PRESENT path doesn't use Task 3 batching; the synchronous wait there is correct as-is. v1's `dri3_trigger_fence` / `dri3_signal_syncobj` xid-keyed methods stay; the new by-handle methods are stubs on v1.

---

## Conventions

- Every Rust source change must end in a clean `cargo build -p yserver`, clippy clean (`cargo clippy -p yserver --tests -- -D warnings`), and the touched test set green. Per AGENTS.md, **do not** use `clippy::pedantic` in this repo.
- Per global `~/.claude/CLAUDE.md` rules: `cargo +nightly fmt` before commits. Do not `--amend`.
- Vk-backed tests are gated `#[ignore = "needs live Vulkan ICD"]` matching the existing v2 pattern. Run them with:
  ```
  VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
    cargo test -p yserver --test v2_acceptance -- --ignored
  ```
- Commit cadence: one logical change per commit, conventional-commit prefix, no AI footer.
- The plan's task numbering reflects landing order. Tasks 1–5 are the spec's "Implementation prerequisites" — they leave the tree green individually with no visible behaviour change. Tasks 6+ build the deferred-completion machinery on top.

---

## Task 1: `BackendFdKind::PresentCompletion` variant + stub dispatch arm

Add the new FD-kind variant to the trait crate and stub the main-loop dispatch arm. No behaviour change — there are no `PresentCompletion` FDs in any backend yet.

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs:31-41`
- Modify: `crates/yserver-core/src/core_loop/run.rs:328-335`

- [ ] **Step 1: Write the failing test**

Append to `crates/yserver-core/src/backend/trait_def.rs` test module (or create one if absent):

```rust
#[cfg(test)]
mod tests {
    use super::BackendFdKind;

    #[test]
    fn present_completion_kind_distinct_from_existing_kinds() {
        // Sanity: the new variant exists and isn't accidentally
        // aliased to an existing one.
        assert_ne!(BackendFdKind::PresentCompletion, BackendFdKind::Libinput);
        assert_ne!(BackendFdKind::PresentCompletion, BackendFdKind::Drm);
        assert_ne!(BackendFdKind::PresentCompletion, BackendFdKind::HostX11);
    }
}
```

- [ ] **Step 2: Run, confirm failure**

```
cargo test -p yserver-core --lib backend::trait_def::tests::present_completion_kind_distinct_from_existing_kinds
```
Expected: compile error — `BackendFdKind::PresentCompletion` does not exist.

- [ ] **Step 3: Add the variant**

In `crates/yserver-core/src/backend/trait_def.rs`, extend the enum:

```rust
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BackendFdKind {
    /// libinput's epoll fd. Readiness is dispatched to the libinput
    /// thread (KMS) — but the fd inventory still flows through the
    /// trait so the core's poller can register it uniformly.
    Libinput,
    /// DRM device fd; readiness drives `on_page_flip_ready`.
    Drm,
    /// Host X11 connection fd (ynest only); readiness drives
    /// `Backend::drain_host_socket` on the core thread.
    HostX11,
    /// Stage 5 Task 6.1: backend-internal epoll FD aggregating
    /// per-entry sync_file FDs for deferred PRESENT completion +
    /// a wakeup_eventfd. Readiness drives
    /// `Backend::drain_completed_present_events`. Spec
    /// `2026-05-23-deferred-present-completion-design.md`.
    PresentCompletion,
}
```

- [ ] **Step 4: Add the dispatch arm in `run_core`**

In `crates/yserver-core/src/core_loop/run.rs:328-335`, the existing dispatch sets tokens to variants of `BackendFdKind`. Add a new constant + arm. Find the existing arms (something like):

```rust
            BackendFdKind::Libinput => LIBINPUT_TOKEN,
            BackendFdKind::Drm => DRM_TOKEN,
            BackendFdKind::HostX11 => HOST_X11_TOKEN,
```

and add:

```rust
            BackendFdKind::PresentCompletion => PRESENT_COMPLETION_TOKEN,
```

Then add the token definition near the others:

```rust
const PRESENT_COMPLETION_TOKEN: Token = Token(/* next sequential id */);
```

Verify by grepping for `LIBINPUT_TOKEN` / `DRM_TOKEN` / `HOST_X11_TOKEN` to see the existing pattern; copy it exactly. Token ids are typically `Token(0)`, `Token(1)`, etc.; the new one takes the next unused slot.

In the loop body where ready tokens dispatch (e.g. `match token { LIBINPUT_TOKEN => ..., DRM_TOKEN => ..., HOST_X11_TOKEN => ..., }`), add a stub arm for `PRESENT_COMPLETION_TOKEN` that logs at trace level and does nothing else. The real drain wiring lands in Task 11.

```rust
            PRESENT_COMPLETION_TOKEN => {
                log::trace!(
                    "core loop: PresentCompletion fd ready (drain wiring lands in Task 11)"
                );
            }
```

- [ ] **Step 5: Run all tests, confirm pass**

```
cargo +nightly fmt
cargo clippy -p yserver-core --tests -- -D warnings
cargo clippy -p yserver --tests -- -D warnings
cargo test -p yserver-core --lib
cargo test -p yserver --lib
```
Expected: all green, clippy clean.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver-core/src/backend/trait_def.rs \
        crates/yserver-core/src/core_loop/run.rs
git commit -m "feat(backend): BackendFdKind::PresentCompletion variant + dispatch stub

Foundation prereq #1 for Stage 5 Task 6.1 deferred PRESENT
completion. Adds the new FD-kind variant + main-loop dispatch
arm that logs trace-level on readiness; real drain dispatch
lands in a later commit. Spec
2026-05-23-deferred-present-completion-design.md."
```

---

## Task 2: Arc-wrap xshmfence registry + by-handle signal API

`KmsBackendV2.dri3_xshmfences: HashMap<u32, FenceMapping>` at `backend.rs:221` wraps `FenceMapping` directly. To support lifetime-pinning past `XFixesDestroyFence`, wrap the value type in `Arc<FenceMapping>` and add a by-handle signal method that bypasses xid lookup.

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs:221` (struct field type)
- Modify: `crates/yserver/src/kms/v2/backend.rs:8748-8790` (existing `dri3_fence_from_fd` + `dri3_trigger_fence` impls)
- Modify: `crates/yserver/src/kms/backend.rs` (v1: matching changes for type consistency, even if v1 never calls the new APIs)
- Modify: `crates/yserver-core/src/backend/trait_def.rs` (new trait methods)

- [ ] **Step 1: Write the failing test**

Append to `crates/yserver/src/kms/v2/backend.rs` test module:

```rust
    #[test]
    fn xshmfence_handle_accessor_returns_arc_clone() {
        // Construct a backend without Vk (skip if test fixture needs it).
        let mut b = match crate::kms::v2::KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: no Vk: {e}");
                return;
            }
        };
        // Manually inject an entry into the registry — bypass the
        // protocol path (DRI3 FenceFromFD) since constructing a real
        // xshmfence FD in a unit test is fragile.
        let xid = 0x1234_5678_u32;
        let mapping = crate::kms::xshmfence::FenceMapping::for_tests_dummy();
        b.dri3_xshmfences.insert(xid, std::sync::Arc::new(mapping));
        let h1 = b.dri3_xshmfence_handle(xid).expect("handle present");
        assert_eq!(std::sync::Arc::strong_count(&h1), 2,
            "registry + caller should both hold a reference");
        let h2 = b.dri3_xshmfence_handle(xid).expect("second handle");
        assert_eq!(std::sync::Arc::strong_count(&h1), 3,
            "registry + two callers should all hold references");
        let _ = h1;
        let _ = h2;
        // Drop the registry entry (mimics XFixesDestroyFence).
        b.dri3_xshmfences.remove(&xid);
        // Accessor returns None now; but the caller's Arc clones
        // still pin the FenceMapping alive (no destructor panic).
        assert!(b.dri3_xshmfence_handle(xid).is_none());
    }
```

Note: `FenceMapping::for_tests_dummy()` is a new test-only constructor. If `FenceMapping` doesn't have one, add it in this task — a zero-content constructor that creates the struct with all fields defaulted / null-safe.

- [ ] **Step 2: Run, confirm failure**

```
cargo test -p yserver --lib xshmfence_handle_accessor_returns_arc_clone -- --ignored
```
Expected: compile error — `dri3_xshmfence_handle` undefined, `dri3_xshmfences` field type wrong.

- [ ] **Step 3: Change the storage type on `KmsBackendV2`**

In `crates/yserver/src/kms/v2/backend.rs:221`:

```rust
pub(crate) dri3_xshmfences: HashMap<u32, std::sync::Arc<crate::kms::xshmfence::FenceMapping>>,
```

Adjust the two constructors at backend.rs:466 and :988 — the `HashMap::new()` calls don't change but every site that inserts into the map now wraps the value in `Arc::new(...)`.

Find every site that touches `dri3_xshmfences` (use `grep -n 'dri3_xshmfences' crates/yserver/src/kms/v2/backend.rs`) and update:

- Insertion path at backend.rs:8756: `self.dri3_xshmfences.insert(fence_xid, std::sync::Arc::new(mapping));`
- Read-and-use at backend.rs:8774: `let mapping = self.dri3_xshmfences.get(&fence_xid)?;` — the `&Arc<FenceMapping>` derefs to `&FenceMapping` automatically for method calls. Most sites need no change beyond an extra `.clone()` if they want to escape the borrow.

- [ ] **Step 4: Mirror the v1 change**

In `crates/yserver/src/kms/backend.rs:707-720` (or wherever `dri3_xshmfences` is defined on `KmsBackend`), apply the same `Arc` wrap. v1 doesn't use the deferred-completion path but the trait method signatures (Task 5) need consistent types across both backends.

- [ ] **Step 5: Add by-handle methods to the `Backend` trait**

In `crates/yserver-core/src/backend/trait_def.rs`, near the existing `dri3_trigger_fence`:

```rust
    /// Stage 5 Task 6.1: take an Arc clone of the xshmfence's
    /// underlying primitive, suitable for deferred completion paths
    /// that need to survive an intervening `XFixesDestroyFence`.
    /// Returns `None` if the xid isn't in the registry at call time.
    fn dri3_xshmfence_handle(
        &self,
        _fence_xid: u32,
    ) -> Option<std::sync::Arc<crate::kms::xshmfence::FenceMapping>> {
        None
    }

    /// Stage 5 Task 6.1: trigger the xshmfence via a held Arc clone,
    /// bypassing xid lookup. Used by the deferred PRESENT completion
    /// drain when the resource id may have been freed mid-flight.
    fn dri3_trigger_fence_via_handle(
        &mut self,
        _handle: &std::sync::Arc<crate::kms::xshmfence::FenceMapping>,
    ) -> std::io::Result<()> {
        Err(std::io::Error::other("dri3_trigger_fence_via_handle not implemented"))
    }
```

Note: the `crate::kms::xshmfence::FenceMapping` path is in the yserver crate, not yserver-core. This creates a cross-crate dependency for the trait surface. If `yserver-core` doesn't already import from `yserver`, this is a problem — invert the dependency by defining a re-export or moving `FenceMapping` to yserver-core. Verify the current crate dependency direction (`grep -n 'kms::xshmfence' crates/yserver-core/src/`) — if zero results, the dependency goes the other way (yserver-core → yserver) and `FenceMapping` needs to move or be re-defined in the trait crate.

**Likely fix**: introduce an opaque trait in `yserver-core` like `trait XshmfenceHandle: Send + Sync {}` blank-implemented for the concrete type in yserver; the trait methods take `&dyn XshmfenceHandle`. Simpler alternative: use `&dyn std::any::Any + Send + Sync` for the handle parameter type since the trait method is opaque to yserver-core consumers. Pick whichever is cleanest given the existing dependency direction.

- [ ] **Step 6: Implement the trait method on `KmsBackendV2`**

In `crates/yserver/src/kms/v2/backend.rs`, inside the existing `impl Backend for KmsBackendV2` block:

```rust
    fn dri3_xshmfence_handle(
        &self,
        fence_xid: u32,
    ) -> Option<std::sync::Arc<crate::kms::xshmfence::FenceMapping>> {
        self.dri3_xshmfences.get(&fence_xid).cloned()
    }

    fn dri3_trigger_fence_via_handle(
        &mut self,
        handle: &std::sync::Arc<crate::kms::xshmfence::FenceMapping>,
    ) -> std::io::Result<()> {
        // Direct trigger on the held primitive — no registry lookup.
        // `FenceMapping::trigger` is the same method
        // `dri3_trigger_fence(xid)` ultimately calls after its lookup.
        handle.trigger()
            .map_err(|e| std::io::Error::other(format!("xshmfence trigger: {e}")))
    }
```

If `FenceMapping::trigger` doesn't yet exist as a method, extract it from the existing inline trigger code at backend.rs:8774-8780 (or wherever the actual trigger logic lives) into a method on `FenceMapping`. Then both `dri3_trigger_fence` (xid path) and `dri3_trigger_fence_via_handle` call the same method.

- [ ] **Step 7: Implement v1's trait method as a no-op**

In `crates/yserver/src/kms/backend.rs`'s `impl Backend for KmsBackend`:

```rust
    fn dri3_xshmfence_handle(
        &self,
        fence_xid: u32,
    ) -> Option<std::sync::Arc<crate::kms::xshmfence::FenceMapping>> {
        self.dri3_xshmfences.get(&fence_xid).cloned()
    }

    fn dri3_trigger_fence_via_handle(
        &mut self,
        handle: &std::sync::Arc<crate::kms::xshmfence::FenceMapping>,
    ) -> std::io::Result<()> {
        handle.trigger()
            .map_err(|e| std::io::Error::other(format!("xshmfence trigger: {e}")))
    }
```

v1 doesn't have a deferred path but the method works the same; it would be called only if some future code path on v1 needed it.

- [ ] **Step 8: Run the test, confirm pass**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
cargo test -p yserver --lib xshmfence_handle_accessor_returns_arc_clone -- --ignored
cargo test -p yserver --lib
cargo test -p yserver-core --lib
```
Expected: all green.

- [ ] **Step 9: Commit**

```bash
git add crates/yserver-core/src/backend/trait_def.rs \
        crates/yserver/src/kms/backend.rs \
        crates/yserver/src/kms/v2/backend.rs \
        crates/yserver/src/kms/xshmfence.rs
git commit -m "feat(dri3): Arc-wrap xshmfence registry + by-handle trigger API

Foundation prereq #2 (xshmfence half) for Stage 5 Task 6.1.
Registry value type becomes Arc<FenceMapping>; new
dri3_xshmfence_handle accessor returns a pinning clone; new
dri3_trigger_fence_via_handle signals the underlying primitive
without an xid lookup. The lifetime pin survives mid-flight
XFixesDestroyFence — required by the deferred PRESENT completion
drain. Existing xid-keyed dri3_trigger_fence unchanged."
```

---

## Task 3: Arc-wrap syncobj registry + by-handle signal API

Same shape as Task 2 but for the DRM syncobj path. The syncobj registry stores `vk::Semaphore` (a `Copy` u64 handle); we need a struct that owns the destruction so `Arc<OwnedSemaphore>` can pin lifetime.

**Files:**
- Create: `crates/yserver/src/kms/v2/owned_semaphore.rs` — small RAII wrapper around `vk::Semaphore`
- Modify: `crates/yserver/src/kms/v2/backend.rs:227` (syncobj field type)
- Modify: `crates/yserver/src/kms/v2/backend.rs:8807-8833` (existing impls)
- Modify: `crates/yserver/src/kms/backend.rs` (v1: mirror)
- Modify: `crates/yserver-core/src/backend/trait_def.rs` (trait methods)
- Modify: `crates/yserver/src/kms/v2/mod.rs` (register `owned_semaphore` module)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn syncobj_handle_accessor_returns_arc_clone() {
        let mut b = match crate::kms::v2::KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: no Vk: {e}");
                return;
            }
        };
        // Manually inject an OwnedSemaphore via the existing
        // create-syncobj path (or via a test-only direct insert).
        // Simplest: do the dummy-insert pattern from Task 2.
        let xid = 0xAAAA_BBBB_u32;
        let sem = crate::kms::v2::owned_semaphore::OwnedSemaphore::for_tests_dummy();
        b.dri3_sync_resources.insert(xid, std::sync::Arc::new(sem));
        let h = b.dri3_syncobj_handle(xid).expect("handle present");
        assert_eq!(std::sync::Arc::strong_count(&h), 2);
        b.dri3_sync_resources.remove(&xid);
        // Holding h still pins the OwnedSemaphore alive.
        assert!(b.dri3_syncobj_handle(xid).is_none());
        drop(h); // OwnedSemaphore::Drop fires here, calling
                 // destroy_semaphore if Vk is live; on the test fixture
                 // OwnedSemaphore::for_tests_dummy holds vk::Semaphore::null
                 // so destroy is a no-op.
    }
```

- [ ] **Step 2: Run, confirm failure**

```
cargo test -p yserver --lib syncobj_handle_accessor_returns_arc_clone -- --ignored
```
Expected: compile error — `owned_semaphore` module missing.

- [ ] **Step 3: Create the `OwnedSemaphore` module**

In `crates/yserver/src/kms/v2/mod.rs`, register the module:

```rust
pub(crate) mod owned_semaphore;
```

Create `crates/yserver/src/kms/v2/owned_semaphore.rs`:

```rust
//! RAII wrapper for a `vk::Semaphore` so it can be `Arc`-shared
//! for the deferred PRESENT completion path. Destruction happens
//! on the last Arc drop (via `vkDestroySemaphore`), independent of
//! the X11 resource id's lifetime.

use std::sync::Arc;

use ash::vk;

use crate::kms::vk::device::VkContext;

pub(crate) struct OwnedSemaphore {
    vk: Arc<VkContext>,
    semaphore: vk::Semaphore,
}

impl OwnedSemaphore {
    pub(crate) fn new(vk: Arc<VkContext>, semaphore: vk::Semaphore) -> Self {
        Self { vk, semaphore }
    }

    pub(crate) fn semaphore(&self) -> vk::Semaphore {
        self.semaphore
    }

    /// Signal a timeline-semaphore value via `vkSignalSemaphore`.
    /// Mirrors the existing xid-keyed `dri3_signal_syncobj` body —
    /// extracted as a method so both the by-xid and by-handle paths
    /// share signaling code.
    pub(crate) fn signal(&self, value: u64) -> Result<(), vk::Result> {
        let info = vk::SemaphoreSignalInfo::default()
            .semaphore(self.semaphore)
            .value(value);
        unsafe { self.vk.device.signal_semaphore(&info) }
    }

    /// Test-only constructor: holds a null semaphore handle. Drop
    /// is a no-op (destroy_semaphore on null is undefined behaviour;
    /// guarded below).
    #[cfg(test)]
    pub(crate) fn for_tests_dummy() -> Self {
        // SAFETY: only used in tests; the null semaphore is never
        // submitted or signaled. Drop checks for null and skips
        // destroy.
        unsafe {
            Self {
                vk: Arc::new(VkContext::for_tests_null()),
                semaphore: vk::Semaphore::null(),
            }
        }
    }
}

impl Drop for OwnedSemaphore {
    fn drop(&mut self) {
        if self.semaphore == vk::Semaphore::null() {
            return; // test-fixture dummy; nothing to destroy
        }
        unsafe {
            self.vk.device.destroy_semaphore(self.semaphore, None);
        }
    }
}
```

If `VkContext::for_tests_null()` doesn't exist, add it as a `#[cfg(test)]` constructor that creates a `VkContext` with null `vk::Device` etc. — only safe to use if no Vk calls are made. Look at the existing `KmsBackendV2::for_tests` (non-vk) pattern in backend.rs:918+.

- [ ] **Step 4: Change the storage type**

In `crates/yserver/src/kms/v2/backend.rs:227`:

```rust
pub(crate) dri3_sync_resources: HashMap<u32, std::sync::Arc<crate::kms::v2::owned_semaphore::OwnedSemaphore>>,
```

Update every site that touches the map (use `grep -n 'dri3_sync_resources' crates/yserver/src/kms/v2/backend.rs`). The key changes are at insertion (`Arc::new(OwnedSemaphore::new(...))`) and at lookup (the value derefs to `&OwnedSemaphore`).

Specifically:
- `:8767` (insertion in `dri3_fence_from_fd`-like path) — wrap in `Arc::new(OwnedSemaphore::new(vk.clone(), semaphore))`.
- `:8807` (insertion in `dri3_import_syncobj`) — same.
- `:8817` (removal in `dri3_free_syncobj`) — `remove` returns `Option<Arc<OwnedSemaphore>>`; just drop the Arc (last reference triggers destruction if no other holder).
- `:8827` (lookup in `dri3_signal_syncobj`) — call `.signal(value)` on the held entry's `OwnedSemaphore`.

- [ ] **Step 5: Mirror v1**

In `crates/yserver/src/kms/backend.rs`, apply the same wrap. v1's `dri3_sync_resources` storage + insertion/removal/signal sites change shape identically.

- [ ] **Step 6: Add trait methods**

In `crates/yserver-core/src/backend/trait_def.rs`:

```rust
    /// Stage 5 Task 6.1: take an Arc clone of the syncobj's
    /// underlying VkSemaphore wrapper, suitable for deferred
    /// completion paths that need to survive an intervening
    /// `FreeSyncobj`.
    fn dri3_syncobj_handle(
        &self,
        _syncobj_xid: u32,
    ) -> Option<std::sync::Arc<crate::kms::v2::owned_semaphore::OwnedSemaphore>> {
        None
    }

    /// Stage 5 Task 6.1: signal the syncobj timeline point via a held
    /// Arc clone, bypassing xid lookup.
    fn dri3_signal_syncobj_via_handle(
        &mut self,
        _handle: &std::sync::Arc<crate::kms::v2::owned_semaphore::OwnedSemaphore>,
        _value: u64,
    ) -> std::io::Result<()> {
        Err(std::io::Error::other("dri3_signal_syncobj_via_handle not implemented"))
    }
```

Cross-crate caveat: same as Task 2 — yserver-core can't directly reference yserver's `OwnedSemaphore`. If that's the actual dependency direction, define the trait methods to take a `&dyn yserver_core::backend::SyncobjHandle` (a new trait local to yserver-core) blanket-impl'd for `OwnedSemaphore` in yserver. Pick the approach that mirrors what you did in Task 2.

- [ ] **Step 7: Implement on `KmsBackendV2`**

```rust
    fn dri3_syncobj_handle(
        &self,
        syncobj_xid: u32,
    ) -> Option<std::sync::Arc<crate::kms::v2::owned_semaphore::OwnedSemaphore>> {
        self.dri3_sync_resources.get(&syncobj_xid).cloned()
    }

    fn dri3_signal_syncobj_via_handle(
        &mut self,
        handle: &std::sync::Arc<crate::kms::v2::owned_semaphore::OwnedSemaphore>,
        value: u64,
    ) -> std::io::Result<()> {
        handle.signal(value)
            .map_err(|e| std::io::Error::other(format!("vkSignalSemaphore: {e:?}")))
    }
```

Same on v1's `KmsBackend` (if v1's syncobj storage exists; if not, leave as default no-op).

- [ ] **Step 8: Run + commit**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
cargo test -p yserver --lib syncobj_handle_accessor_returns_arc_clone -- --ignored
cargo test -p yserver --lib
```
Expected: green.

```bash
git add crates/yserver-core/src/backend/trait_def.rs \
        crates/yserver/src/kms/backend.rs \
        crates/yserver/src/kms/v2/backend.rs \
        crates/yserver/src/kms/v2/mod.rs \
        crates/yserver/src/kms/v2/owned_semaphore.rs
git commit -m "feat(dri3): Arc-wrap syncobj registry + by-handle signal API

Foundation prereq #2 (syncobj half) for Stage 5 Task 6.1.
Introduces OwnedSemaphore RAII wrapper so vk::Semaphore can be
Arc-shared; registry value type becomes Arc<OwnedSemaphore>;
new dri3_syncobj_handle + dri3_signal_syncobj_via_handle trait
methods. The lifetime pin survives mid-flight FreeSyncobj —
required by the deferred PRESENT completion drain."
```

---

## Task 4: `FenceTicket::export_sync_file_fd` + verify VK_KHR_external_fence_fd

Add the sync_file FD export accessor to `FenceTicket`. Verify the device extension is enabled.

**Files:**
- Modify: `crates/yserver/src/kms/v2/platform.rs:156` (add method to `FenceTicket`)
- Modify: `crates/yserver/src/kms/vk/device.rs` (verify / enable extension)

- [ ] **Step 1: Check extension status**

```
grep -nE "VK_KHR_external_fence_fd|external_fence_fd" crates/yserver/src/kms/vk/device.rs
```

If 0 results: the extension isn't enabled — add it in Step 3. If results show `external_fence_fd::Device` already loaded for the scanout path (it should be — semaphore export is enabled and the fence variant rides the same instance enabling), confirm the device-level extension `KHR_external_fence_fd` is in the enabled-extensions list.

- [ ] **Step 2: Write the failing test**

Append to `crates/yserver/src/kms/v2/platform.rs` test module:

```rust
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn fence_ticket_export_sync_file_fd_unsignaled_returns_some() {
        // Build a Vk context + fence pool; acquire a ticket against
        // a freshly-created unsignaled fence; export should yield an
        // OwnedFd.
        let Some(b) = crate::kms::v2::KmsBackendV2::for_tests_with_vk().ok() else {
            eprintln!("skipping: no Vk");
            return;
        };
        let vk = b.platform.vk().expect("vk live").clone();
        let mut pool = crate::kms::v2::platform::FencePool::new(vk.clone());
        let ticket = pool.acquire().expect("ticket");
        let fd_opt = ticket.export_sync_file_fd(&vk).expect("export ok");
        assert!(fd_opt.is_some(), "unsignaled fence should yield a sync_file FD");
        // Don't drop the fd before the ticket; OwnedFd Drop closes it.
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn fence_ticket_export_sync_file_fd_already_signaled_returns_none() {
        let Some(b) = crate::kms::v2::KmsBackendV2::for_tests_with_vk().ok() else {
            eprintln!("skipping: no Vk");
            return;
        };
        let vk = b.platform.vk().expect("vk live").clone();
        let mut pool = crate::kms::v2::platform::FencePool::new(vk.clone());
        let ticket = pool.acquire().expect("ticket");
        // Force-signal via a stub submit (or via the test helper).
        // If FenceTicket has a test-only `mark_signaled`, use it; else
        // submit an empty CB to ensure the fence completes.
        ticket.test_mark_signaled();
        let fd_opt = ticket.export_sync_file_fd(&vk).expect("export ok");
        assert!(fd_opt.is_none(),
            "already-signaled fence should yield None (vkGetFenceFdKHR returns -1)");
    }
```

`FenceTicket::test_mark_signaled` is the test-only knob already mentioned in the spec (§"Test fixtures"). If it doesn't exist, add it now as a `#[cfg(test)] pub(crate) fn test_mark_signaled(&self)` that sets `signaled_cache` to `true`. Note: this only fakes the CPU-side cache; `vkGetFenceFdKHR` on the actual VkFence may still return a valid FD if the GPU hasn't actually completed. For a reliable "already signaled" test, either use a fence that was never submitted to the GPU (the freshly-created state is unsignaled by Vulkan spec, so this doesn't help) or write a more elaborate test that submits empty work and waits. **Simpler approach for this test**: drop the second test and rely on the first one (unsignaled returns Some), plus the integration tests in later tasks for the already-signaled fast-path.

- [ ] **Step 3: Enable the extension if missing**

In `crates/yserver/src/kms/vk/device.rs`, find the device extensions list (search for `ash::khr` or `device_extensions` or similar):

```rust
// In the device-extensions enable list, add:
ash::khr::external_fence_fd::NAME.as_ptr(),
```

(Matches the pattern of `external_semaphore_fd::NAME` if that's already enabled for the scanout path.)

Also load the fence-fd extension instance:

```rust
let external_fence_fd = ash::khr::external_fence_fd::Device::new(&instance, &device);
```

and add to `VkContext`:

```rust
pub external_fence_fd: ash::khr::external_fence_fd::Device,
```

Mirror exactly how `external_semaphore_fd` is wired if it's already there.

- [ ] **Step 4: Add the export method on `FenceTicket`**

In `crates/yserver/src/kms/v2/platform.rs:156` (just after the existing `fence()` accessor):

```rust
    /// Stage 5 Task 6.1: export the underlying `VkFence` as a Linux
    /// sync_file FD via `vkGetFenceFdKHR(SYNC_FD)`. The returned FD
    /// becomes `POLLIN`-readable when the fence signals.
    ///
    /// Returns `Ok(Some(fd))` for an unsignaled fence;
    /// `Ok(None)` when `vkGetFenceFdKHR` returns -1 (the fence is
    /// already signaled at export time — caller treats as
    /// "drain immediately");
    /// `Err(_)` on Vk failure.
    pub(crate) fn export_sync_file_fd(
        &self,
        vk: &VkContext,
    ) -> Result<Option<std::os::fd::OwnedFd>, vk::Result> {
        let info = vk::FenceGetFdInfoKHR::default()
            .fence(self.inner.fence)
            .handle_type(vk::ExternalFenceHandleTypeFlags::SYNC_FD);
        let raw = unsafe { vk.external_fence_fd.get_fence_fd(&info)? };
        if raw < 0 {
            return Ok(None);
        }
        // SAFETY: vkGetFenceFdKHR returned a freshly-owned FD; we take
        // ownership.
        Ok(Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }))
    }
```

- [ ] **Step 5: Run + commit**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
cargo test -p yserver --lib
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --lib fence_ticket_export_sync_file_fd -- --ignored
```
Expected: lib tests green; the Vk-gated test passes under lavapipe.

```bash
git add crates/yserver/src/kms/v2/platform.rs \
        crates/yserver/src/kms/vk/device.rs
git commit -m "feat(vk): FenceTicket::export_sync_file_fd accessor

Foundation prereq #3 for Stage 5 Task 6.1. Exports the
underlying VkFence as a Linux sync_file FD via
vkGetFenceFdKHR(SYNC_FD); Ok(None) for already-signaled (the
-1 case); Err for Vk failure. Verifies VK_KHR_external_fence_fd
is enabled on the v2 VkContext device extension list."
```

---

## Task 5: Backend trait methods + public event types

Add `enqueue_present_completion` and `drain_completed_present_events` to the `Backend` trait with no-op / empty default impls. Define `CompletedPresentEvent` + `PresentWake` in yserver-core (the trait crate).

**Files:**
- Modify: `crates/yserver-core/src/backend/trait_def.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod present_completion_trait_tests {
    use super::*;

    // Use the existing RecordingBackend (or similar test backend) to
    // verify default trait impls compile + behave as expected.

    #[test]
    fn default_drain_completed_present_events_returns_empty() {
        let mut backend = crate::backend::recording::RecordingBackend::new();
        let events = backend.drain_completed_present_events();
        assert!(events.is_empty());
    }

    #[test]
    fn default_enqueue_present_completion_is_noop() {
        let mut backend = crate::backend::recording::RecordingBackend::new();
        backend.enqueue_present_completion(CompletedPresentEvent {
            client_id: yserver_protocol::x11::ClientId(0),
            serial: 1,
            host_xid: 0,
            dst_host_xid: 0,
            options: 0,
            wake: PresentWake::Pixmap { idle_fence_xid: 0 },
        });
        // No drain triggered; default impl swallows.
        assert!(backend.drain_completed_present_events().is_empty());
    }
}
```

- [ ] **Step 2: Add public types**

In `crates/yserver-core/src/backend/trait_def.rs`, after the existing `PresentCaps` struct (around line ~85):

```rust
/// Stage 5 Task 6.1: payload of a deferred PRESENT completion
/// returned by [`Backend::drain_completed_present_events`]. Carries
/// everything the main loop needs to fan out `IdleNotify` +
/// `CompleteNotify { mode: Copy }` events plus trigger the X11
/// resource-id-keyed wake objects (xshmfence / DRM syncobj). The
/// backend has already signalled the underlying primitive via the
/// `Arc`-pinned handle before returning this struct.
#[derive(Debug, Clone)]
pub struct CompletedPresentEvent {
    pub client_id: yserver_protocol::x11::ClientId,
    pub serial: u32,
    pub host_xid: u32,
    pub dst_host_xid: u32,
    pub options: u32,
    pub wake: PresentWake,
}

/// Per-PRESENT-path wake target. Surfaces the original
/// PRESENT::Pixmap (xshmfence-driven) vs PRESENT::PixmapSynced
/// (DRM syncobj timeline) distinction back to the loop. The xids
/// in each variant are for X11-protocol bookkeeping
/// (`state.sync_fences[xid].triggered = true`) only — the actual
/// signal call against the underlying primitive happens inside the
/// backend at drain time.
#[derive(Debug, Clone, Copy)]
pub enum PresentWake {
    Pixmap { idle_fence_xid: u32 },
    PixmapSynced { release_syncobj: u32, release_value: u64 },
}
```

- [ ] **Step 3: Add trait methods with default impls**

Inside the existing `pub trait Backend { ... }` block:

```rust
    /// Stage 5 Task 6.1: enqueue a deferred PRESENT completion. The
    /// backend captures the cow_batch fence ticket + an Arc-pinned
    /// clone of the wake primitive, and returns immediately. The
    /// drain hook later fires the wake signal + the event payload.
    /// Default impl is no-op so non-v2 backends opt out.
    fn enqueue_present_completion(&mut self, _event: CompletedPresentEvent) {
        // no-op
    }

    /// Stage 5 Task 6.1: drain entries whose cow_batch fence has
    /// signalled. The backend internally fires the xshmfence /
    /// syncobj signal via the Arc-pinned handle before returning
    /// the events. Caller is responsible for X11 event fan-out +
    /// `state.sync_fences` bookkeeping based on the returned
    /// `PresentWake` variant. Default impl returns empty so non-v2
    /// backends opt out.
    fn drain_completed_present_events(&mut self) -> Vec<CompletedPresentEvent> {
        Vec::new()
    }
```

- [ ] **Step 4: Run, confirm pass**

```
cargo +nightly fmt
cargo clippy -p yserver-core --tests -- -D warnings
cargo test -p yserver-core --lib backend::trait_def::present_completion_trait_tests
cargo test -p yserver-core --lib
cargo test -p yserver --lib
```
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/backend/trait_def.rs
git commit -m "feat(backend): enqueue/drain trait methods + CompletedPresentEvent

Foundation prereq #4 for Stage 5 Task 6.1. Adds
Backend::enqueue_present_completion (no-op default) and
Backend::drain_completed_present_events (empty default) trait
methods, plus public CompletedPresentEvent + PresentWake types.
v1 + ynest backends inherit the defaults; v2 will override in a
later commit."
```

---

## Task 6: Define `PendingPresentEntry` + `PinnedWake`

The internal backend state types that pair the public event payload with the lifetime-pin Arcs + the sync_file FD. Lives in the new `present_completion.rs` module so the implementation has a single home.

**Files:**
- Create: `crates/yserver/src/kms/v2/present_completion.rs`
- Modify: `crates/yserver/src/kms/v2/mod.rs` (register module)

- [ ] **Step 1: Create the module**

Create `crates/yserver/src/kms/v2/present_completion.rs`:

```rust
//! Deferred PRESENT completion queue (Stage 5 Task 6.1).
//!
//! Owns per-entry state for the v2 backend's `enqueue_present_completion`
//! + `drain_completed_present_events` trait impls. Internal types
//! never escape the `yserver` crate; the trait surface exchanges
//! the public `CompletedPresentEvent` only.
//!
//! Spec: `docs/superpowers/specs/2026-05-23-deferred-present-completion-design.md`.

use std::{os::fd::OwnedFd, sync::Arc};

use yserver_core::backend::CompletedPresentEvent;

use crate::kms::{
    v2::{owned_semaphore::OwnedSemaphore, platform::FenceTicket},
    xshmfence::FenceMapping,
};

/// One queued PRESENT awaiting cow_batch retirement. The drain
/// fires the wake signal via `wake_pin` + returns the `event`
/// payload to the main loop.
pub(crate) struct PendingPresentEntry {
    /// Cow_batch ticket the just-appended copy_area participates in.
    pub(crate) ticket: FenceTicket,
    /// Lifetime pin on the underlying wake primitive. Survives a
    /// mid-flight `XFixesDestroyFence` / `FreeSyncobj`.
    pub(crate) wake_pin: PinnedWake,
    /// sync_file FD exported from `ticket.fence` via
    /// `vkGetFenceFdKHR(SYNC_FD)`. `None` when the fence was
    /// already signaled at enqueue time (vkGetFenceFdKHR returned
    /// -1; the wakeup_eventfd path is used instead).
    pub(crate) fence_fd: Option<OwnedFd>,
    /// Public-facing event payload, returned by `drain_*` to the
    /// main loop.
    pub(crate) event: CompletedPresentEvent,
}

/// Wake-target lifetime pin variants. The drain dispatches signal
/// via the held `Arc` regardless of whether the X11 resource id is
/// still in the registry.
pub(crate) enum PinnedWake {
    Pixmap(Arc<FenceMapping>),
    PixmapSynced { handle: Arc<OwnedSemaphore>, value: u64 },
    /// Client passed no wake object (idle_fence_xid == 0 or
    /// release_syncobj == 0). Drain skips the signal step; X11 event
    /// emission still happens.
    None,
}

#[cfg(test)]
mod tests {
    use super::*;
    use yserver_core::backend::PresentWake;
    use yserver_protocol::x11::ClientId;

    /// Smoke test that the types compile + can be constructed.
    /// Real semantics tested in `KmsBackendV2` integration tests.
    #[test]
    fn pinned_wake_none_constructs() {
        let pin = PinnedWake::None;
        match pin {
            PinnedWake::None => {}
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn completed_present_event_carries_payload() {
        let event = CompletedPresentEvent {
            client_id: ClientId(7),
            serial: 42,
            host_xid: 0x100001,
            dst_host_xid: 0xE00001,
            options: 0,
            wake: PresentWake::Pixmap { idle_fence_xid: 0xCC },
        };
        assert_eq!(event.serial, 42);
    }
}
```

In `crates/yserver/src/kms/v2/mod.rs`:

```rust
pub(crate) mod present_completion;
```

- [ ] **Step 2: Run, confirm green**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
cargo test -p yserver --lib present_completion
```
Expected: 2 tests pass; clippy clean.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver/src/kms/v2/present_completion.rs \
        crates/yserver/src/kms/v2/mod.rs
git commit -m "feat(v2): PendingPresentEntry + PinnedWake internal types

Stage 5 Task 6.1 internal scaffolding. PendingPresentEntry pairs
the cow_batch FenceTicket + lifetime-pin Arc on the wake primitive
+ exported sync_file OwnedFd + the public CompletedPresentEvent
payload. PinnedWake variants: Pixmap (Arc<FenceMapping>),
PixmapSynced (Arc<OwnedSemaphore> + u64), or None (no wake object)."
```

---

## Task 7: Add `present_completion_epfd` + `wakeup_eventfd` to `KmsBackendV2`

Two `OwnedFd` fields, created at init, registered together so the eventfd lives inside the inner epoll. Exposed once via `poll_fds()` under `BackendFdKind::PresentCompletion`.

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` (struct fields + constructors)
- Modify: `crates/yserver/src/kms/v2/platform.rs:845` (`poll_fds`)
- Modify: `crates/yserver/Cargo.toml` (add `libc` or `rustix` dep if not present)

- [ ] **Step 1: Verify dep availability**

```
grep -E "^libc|^rustix" crates/yserver/Cargo.toml
```

`rustix` is the cleaner Rust-native option for `epoll` + `eventfd`. If neither is in `Cargo.toml`, add `rustix = { version = "0.38", features = ["event", "fs"] }` (or the latest available).

- [ ] **Step 2: Write the failing test**

```rust
    #[test]
    fn present_completion_epfd_present_at_init_and_poll_fds() {
        let b = match crate::kms::v2::KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: no Vk: {e}");
                return;
            }
        };
        // poll_fds reports the inner epfd under the new variant.
        let fds = b.platform.poll_fds();
        let present_kind = yserver_core::backend::BackendFdKind::PresentCompletion;
        assert!(
            fds.iter().any(|(_, k)| *k == present_kind),
            "platform.poll_fds() must report a PresentCompletion FD"
        );
        // The FD should be stable: a second call returns the same raw value.
        let raw1 = fds.iter().find(|(_, k)| *k == present_kind).unwrap().0;
        let raw2 = b.platform.poll_fds().iter().find(|(_, k)| *k == present_kind).unwrap().0;
        assert_eq!(raw1, raw2, "the inner epfd is stable across poll_fds() calls");
    }
```

- [ ] **Step 3: Add fields to `KmsBackendV2`** *or* (preferred) **to `PlatformBackend`**

The cleanest home is `PlatformBackend` since that's where `poll_fds()` lives. Modify `crates/yserver/src/kms/v2/platform.rs` (the `PlatformBackend` struct, near the existing DRM/libinput FDs):

```rust
    /// Stage 5 Task 6.1: inner epoll FD aggregating per-entry
    /// sync_file FDs for deferred PRESENT completion. Exposed via
    /// `poll_fds()` under `BackendFdKind::PresentCompletion`. Spec
    /// `2026-05-23-deferred-present-completion-design.md`.
    pub(crate) present_completion_epfd: std::os::fd::OwnedFd,

    /// Stage 5 Task 6.1: eventfd used to wake the main loop when an
    /// already-signaled fence is enqueued (vkGetFenceFdKHR returned
    /// -1) or any other force-wake condition arises. Registered with
    /// `present_completion_epfd` at init under EPOLLIN.
    pub(crate) wakeup_eventfd: std::os::fd::OwnedFd,
```

Create both FDs in the `PlatformBackend::new` (or equivalent) constructor:

```rust
let present_completion_epfd = rustix::event::epoll::create(rustix::event::epoll::CreateFlags::CLOEXEC)
    .map_err(|e| io::Error::other(format!("epoll_create1: {e}")))?;
let wakeup_eventfd = rustix::event::eventfd(
    0,
    rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
)
.map_err(|e| io::Error::other(format!("eventfd: {e}")))?;
// Register wakeup_eventfd with the inner epoll once at init.
rustix::event::epoll::add(
    &present_completion_epfd,
    &wakeup_eventfd,
    rustix::event::epoll::EventData::new_u64(WAKEUP_EVENTFD_TOKEN),
    rustix::event::epoll::EventFlags::IN,
)
.map_err(|e| io::Error::other(format!("epoll_ctl ADD wakeup_eventfd: {e}")))?;
```

`WAKEUP_EVENTFD_TOKEN` is a constant `u64` value the inner-epoll drain uses to distinguish wakeup-eventfd readiness from per-entry sync_file readiness. Define near the top of the file:

```rust
/// Stage 5 Task 6.1: epoll event-data token for the backend's
/// wakeup_eventfd. Per-entry sync_file FDs use the entry's index
/// (or a serial) as their token instead, distinguishing them from
/// the wakeup_eventfd.
pub(crate) const WAKEUP_EVENTFD_TOKEN: u64 = u64::MAX;
```

- [ ] **Step 4: Extend `poll_fds()`**

In `crates/yserver/src/kms/v2/platform.rs:845`:

```rust
    pub(crate) fn poll_fds(&self) -> Vec<(RawFd, BackendFdKind)> {
        use std::os::fd::AsRawFd;
        let mut fds = Vec::new();
        fds.push((self.device.as_fd().as_raw_fd(), BackendFdKind::Drm));
        if let Some(ctx) = self.input_ctx.as_ref() {
            fds.push((ctx.fd(), BackendFdKind::Libinput));
        }
        // Stage 5 Task 6.1: stable inner epfd for deferred PRESENT
        // completion. Always present.
        fds.push((
            self.present_completion_epfd.as_raw_fd(),
            BackendFdKind::PresentCompletion,
        ));
        fds
    }
```

(Adjust per the actual existing function shape — the key addition is the third `fds.push(...)`.)

- [ ] **Step 5: Run, confirm pass**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --lib present_completion_epfd_present_at_init_and_poll_fds -- --ignored
cargo test -p yserver --lib
```
Expected: green.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver/src/kms/v2/platform.rs crates/yserver/Cargo.toml \
        crates/yserver/Cargo.lock
git commit -m "feat(v2/platform): present_completion_epfd + wakeup_eventfd at init

Stage 5 Task 6.1 wake-source infrastructure. PlatformBackend now
creates an inner epoll FD + a wakeup eventfd at init; the eventfd
is registered with the epfd under WAKEUP_EVENTFD_TOKEN. poll_fds()
exposes the epfd under the new BackendFdKind::PresentCompletion
variant. Per-entry sync_file FDs will be added/removed from this
inner epfd by the enqueue/drain paths in later commits."
```

---

## Task 8: Engine accessor `current_cow_batch_ticket() -> Option<FenceTicket>`

The PRESENT handler needs to read the just-appended cow_batch's FenceTicket inside `enqueue_present_completion`. Add an engine-side accessor.

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn current_cow_batch_ticket_none_before_any_copy_area() {
        let Some(b) = crate::kms::v2::KmsBackendV2::for_tests_with_vk().ok() else {
            eprintln!("skipping: no Vk");
            return;
        };
        let engine = &b.engine;
        assert!(engine.current_cow_batch_ticket().is_none());
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn current_cow_batch_ticket_returns_some_after_cow_copy_area() {
        let mut b = match crate::kms::v2::KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: no Vk: {e}");
                return;
            }
        };
        // Create a COW pixmap-like target + a source pixmap; drive
        // engine.cow_copy_area(...).
        // (Reuse the existing acceptance-test fixture for the
        //  cow_copy_area path — pattern at v2_acceptance.rs:~880.)
        // After the call, current_cow_batch_ticket() must be Some.
        // (Implementation detail: the COW path may not be reachable
        // without scene setup; if so, this test moves to the
        // integration suite in Task 9.)
    }
```

- [ ] **Step 2: Add the accessor**

In `crates/yserver/src/kms/v2/engine.rs`, on `impl RenderEngine`:

```rust
    /// Stage 5 Task 6.1: snapshot of the FenceTicket the currently-
    /// open cow_batch will signal once flushed. Returns `None` if
    /// no cow_batch is pending. The deferred PRESENT completion path
    /// reads this immediately after `engine.cow_copy_area` to bind
    /// the just-appended copy to its eventual GPU-done signal.
    pub(crate) fn current_cow_batch_ticket(&self) -> Option<FenceTicket> {
        self.inner
            .as_ref()
            .and_then(|i| i.pending_cow_batch.as_ref().map(|b| b.ticket.clone()))
    }
```

`PendingCowBatch` already stores a `ticket: FenceTicket` per `engine.rs:~2060`. The field name may differ — verify with `grep -n 'struct PendingCowBatch\|pending_cow_batch' crates/yserver/src/kms/v2/engine.rs`.

- [ ] **Step 3: Run + commit**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
cargo test -p yserver --lib current_cow_batch_ticket
```
Expected: green.

```bash
git add crates/yserver/src/kms/v2/engine.rs
git commit -m "feat(v2/engine): current_cow_batch_ticket accessor

Stage 5 Task 6.1 plumbing. Exposes a clone of the currently-open
cow_batch's FenceTicket so the deferred PRESENT completion path
can bind an enqueue to its eventual GPU-done signal. Returns None
when no cow_batch is pending (the just-flushed-then-immediately-
PRESENT corner; the enqueue path handles via the already-
signaled fast path)."
```

---

## Task 9: `KmsBackendV2::enqueue_present_completion` + `drain_completed_present_events`

The load-bearing backend implementation. Captures the lifetime pin + exports the sync_file FD + registers with the inner epoll OR writes the wakeup_eventfd; drain fires the wake signal via the pinned handle + closes per-entry FDs + reads the eventfd.

**Files:**
- Modify: `crates/yserver/src/kms/v2/present_completion.rs` (add impl methods)
- Modify: `crates/yserver/src/kms/v2/backend.rs` (Backend trait method bodies delegate here)
- Add: `crates/yserver/src/kms/v2/backend.rs` field `pending_present_events: VecDeque<PendingPresentEntry>`

- [ ] **Step 1: Add the queue field**

In `KmsBackendV2` struct (around backend.rs:214 area):

```rust
    /// Stage 5 Task 6.1: queue of in-flight deferred PRESENT
    /// completions. Drained by `drain_completed_present_events` when
    /// the inner present_completion_epfd reports readiness.
    pending_present_events: std::collections::VecDeque<crate::kms::v2::present_completion::PendingPresentEntry>,
```

Initialise in every constructor (backend.rs:465 + :987 etc.):

```rust
    pending_present_events: std::collections::VecDeque::new(),
```

- [ ] **Step 2: Write the failing test**

```rust
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn enqueue_present_completion_captures_ticket_and_registers_fd() {
        let mut b = match crate::kms::v2::KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: no Vk: {e}");
                return;
            }
        };
        // Set up a cow_batch via a cow_copy_area, then enqueue.
        // (Use whatever harness the existing v2_acceptance.rs cow
        //  tests use — patterns at lines ~880-900.)
        // ...
        // Sanity: queue is empty before enqueue, length 1 after.
        // Sanity: drain returns empty (fence not signaled yet).
        // Sanity: forcing retirement makes drain return 1 entry.
    }
```

Place this in `crates/yserver/tests/v2_acceptance.rs` since the cow_copy_area setup is easier via the public Backend trait surface.

- [ ] **Step 3: Implement `enqueue_present_completion`**

Add to `crates/yserver/src/kms/v2/present_completion.rs`:

```rust
use std::collections::VecDeque;

use rustix::event::epoll;
use yserver_core::backend::PresentWake;

use crate::kms::v2::platform::WAKEUP_EVENTFD_TOKEN;

pub(crate) fn enqueue(
    queue: &mut VecDeque<PendingPresentEntry>,
    platform: &crate::kms::v2::platform::PlatformBackend,
    event: yserver_core::backend::CompletedPresentEvent,
    ticket: Option<FenceTicket>,
    wake_pin: PinnedWake,
) -> std::io::Result<()> {
    let Some(ticket) = ticket else {
        // No cow_batch in flight when PRESENT arrived. Treat as
        // already-done: write the wakeup_eventfd so the loop drains
        // immediately on next iteration. wake_pin is still held so
        // the deferred wake-signal fires from the drain.
        write_wakeup_eventfd(platform)?;
        // Synthetic "already signaled" entry: no FD, no ticket — drain
        // emits unconditionally.
        queue.push_back(PendingPresentEntry {
            ticket: dummy_signaled_ticket(platform), // see below
            wake_pin,
            fence_fd: None,
            event,
        });
        return Ok(());
    };

    let vk = platform.vk().expect("vk live for v2");
    let fd_opt = ticket.export_sync_file_fd(vk)
        .map_err(|e| std::io::Error::other(format!("vkGetFenceFdKHR: {e:?}")))?;

    let fence_fd = match fd_opt {
        Some(fd) => {
            // Register with the inner epoll under a per-entry token.
            // Per-entry token: the entry's queue index isn't stable
            // (insertion mutates the queue); use a backend-managed
            // monotonic counter instead. Simpler: don't bother with
            // per-entry tokens since drain re-polls every entry's
            // ticket.poll_signaled() anyway — the inner-epoll wake is
            // a "something signaled, go look" signal, not a per-entry
            // dispatcher. Token can be 0 or any non-WAKEUP value.
            let token = 0_u64; // not load-bearing; see note above
            epoll::add(
                &platform.present_completion_epfd,
                &fd,
                epoll::EventData::new_u64(token),
                epoll::EventFlags::IN,
            )
            .map_err(|e| std::io::Error::other(format!("epoll_ctl ADD: {e}")))?;
            Some(fd)
        }
        None => {
            // Already-signaled fence: vkGetFenceFdKHR returned -1.
            // Write the wakeup_eventfd so the next iteration drains.
            write_wakeup_eventfd(platform)?;
            None
        }
    };

    queue.push_back(PendingPresentEntry {
        ticket,
        wake_pin,
        fence_fd,
        event,
    });
    Ok(())
}

fn write_wakeup_eventfd(
    platform: &crate::kms::v2::platform::PlatformBackend,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let raw = platform.wakeup_eventfd.as_raw_fd();
    let buf: [u8; 8] = 1_u64.to_ne_bytes();
    // SAFETY: writing to an eventfd is a syscall; raw fd is owned by
    // platform. EAGAIN on saturation is benign.
    let n = unsafe { libc::write(raw, buf.as_ptr().cast(), buf.len()) };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EAGAIN) {
            return Ok(()); // saturation, benign
        }
        return Err(err);
    }
    Ok(())
}

fn dummy_signaled_ticket(
    platform: &crate::kms::v2::platform::PlatformBackend,
) -> FenceTicket {
    // The synthetic "no cow_batch" path needs a FenceTicket that
    // poll_signaled() returns true on. Acquire a fence from the pool,
    // mark its signaled_cache true. The fence is returned to the pool
    // on Drop (signaled return path).
    let mut pool = /* obtain via platform helper */;
    let t = pool.acquire().expect("fence available");
    t.test_mark_signaled();
    t
}
```

The `dummy_signaled_ticket` path is awkward; cleaner alternative is to NOT push an entry for the no-cow-batch case at all — just fire the events synchronously inline since the GPU has nothing to wait on. Update the enqueue logic:

```rust
    let Some(ticket) = ticket else {
        // No cow_batch in flight when PRESENT arrived. Fire the
        // event synchronously via the wakeup_eventfd path: write
        // the eventfd so the loop drains, but also push a synthetic
        // entry with fence_fd=None and a pre-signaled ticket. Drain
        // logic checks fence_fd==None || ticket.poll_signaled()
        // first — both conditions trigger immediate emission on the
        // next iteration.
        write_wakeup_eventfd(platform)?;
        // Synthesize a no-fence entry; drain treats fence_fd==None as
        // "always-ready".
        queue.push_back(PendingPresentEntry {
            ticket: FenceTicket::pre_signaled_sentinel(),
            wake_pin,
            fence_fd: None,
            event,
        });
        return Ok(());
    };
```

`FenceTicket::pre_signaled_sentinel()` is a new test-and-prod accessor that returns a ticket whose `poll_signaled()` always returns true. Implementation can be a simple `FenceTicket::from_signaled_cache_bool(true)` constructor that doesn't even hold a real VkFence. **Verify with the FenceTicket internals** that this is feasible — if `FenceTicket` always needs a live VkFence + pool entry, prefer the alternate path "drain handles `fence_fd: None` as always-ready and ticket isn't consulted."

Final shape: drain checks `entry.fence_fd.is_none() || entry.ticket.poll_signaled()`. This unifies the two ready cases.

- [ ] **Step 4: Implement `drain_completed_present_events`**

```rust
pub(crate) fn drain(
    queue: &mut VecDeque<PendingPresentEntry>,
    platform: &crate::kms::v2::platform::PlatformBackend,
    // The drain needs &mut Backend for *_via_handle calls; this is
    // awkward inside the present_completion.rs module. Cleanest: pass
    // closures or move the impl back to backend.rs. Pseudocode here
    // illustrates the algorithm; final placement is up to the impl.
) -> Vec<yserver_core::backend::CompletedPresentEvent> {
    use std::io::Read;
    use std::os::fd::AsRawFd;

    // First, drain the wakeup_eventfd to clear it. read returns
    // EAGAIN if it was never written; benign.
    let raw = platform.wakeup_eventfd.as_raw_fd();
    let mut buf = [0u8; 8];
    let _ = unsafe { libc::read(raw, buf.as_mut_ptr().cast(), buf.len()) };
    // (errno EAGAIN is the no-write case)

    // Walk the queue front-to-back; pop entries that are ready.
    // Same-queue submission order guarantees signals are monotone-by-
    // position, so the ready entries are a contiguous prefix.
    let mut completed = Vec::new();
    while let Some(front) = queue.front() {
        let ready = front.fence_fd.is_none()
            || front.ticket.poll_signaled(platform.vk().expect("vk live"));
        if !ready {
            break;
        }
        // Pop, unregister + close FD, fire wake signal via Arc, return
        // event payload to caller.
        let entry = queue.pop_front().expect("just peeked");
        if let Some(fd) = entry.fence_fd.as_ref() {
            let _ = epoll::delete(&platform.present_completion_epfd, fd);
        }
        // wake-signal happens here via the Arc — see Task 11 which
        // wires this through Backend::dri3_trigger_fence_via_handle /
        // dri3_signal_syncobj_via_handle.
        // (Module-internal drain pseudocode; final placement uses
        // &mut Backend so it can call those methods.)
        completed.push(entry.event);
    }
    completed
}
```

**Final placement consideration**: because `dri3_trigger_fence_via_handle` and `dri3_signal_syncobj_via_handle` are `&mut self` methods on `Backend`, the drain logic that calls them must live in `KmsBackendV2`'s `impl Backend` block, not in the `present_completion.rs` free-function module. Refactor by moving the algorithm into a method on `KmsBackendV2`:

```rust
impl KmsBackendV2 {
    fn drain_completed_present_events_impl(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        use std::io::Read;
        use std::os::fd::AsRawFd;

        // Drain wakeup_eventfd
        let raw = self.platform.wakeup_eventfd.as_raw_fd();
        let mut buf = [0u8; 8];
        let _ = unsafe { libc::read(raw, buf.as_mut_ptr().cast(), buf.len()) };

        let mut completed = Vec::new();
        let vk = self.platform.vk().expect("vk live for v2").clone();
        while let Some(front) = self.pending_present_events.front() {
            let ready = front.fence_fd.is_none()
                || front.ticket.poll_signaled(&vk);
            if !ready {
                break;
            }
            let entry = self.pending_present_events.pop_front().expect("just peeked");
            if let Some(fd) = entry.fence_fd.as_ref() {
                let _ = rustix::event::epoll::delete(
                    &self.platform.present_completion_epfd,
                    fd,
                );
            }
            // Fire the wake signal via the held Arc.
            match &entry.wake_pin {
                PinnedWake::Pixmap(h) => {
                    if let Err(e) = self.dri3_trigger_fence_via_handle(h) {
                        log::warn!("deferred IdleNotify: dri3_trigger_fence_via_handle failed: {e}");
                    }
                }
                PinnedWake::PixmapSynced { handle, value } => {
                    if let Err(e) = self.dri3_signal_syncobj_via_handle(handle, *value) {
                        log::warn!("deferred IdleNotify: dri3_signal_syncobj_via_handle failed: {e}");
                    }
                }
                PinnedWake::None => {}
            }
            completed.push(entry.event);
        }
        completed
    }
}
```

Then the trait method delegates:

```rust
    fn drain_completed_present_events(&mut self) -> Vec<CompletedPresentEvent> {
        self.drain_completed_present_events_impl()
    }
```

And `enqueue_present_completion`:

```rust
    fn enqueue_present_completion(&mut self, event: CompletedPresentEvent) {
        let ticket = self.engine.current_cow_batch_ticket();
        let wake_pin = match &event.wake {
            PresentWake::Pixmap { idle_fence_xid } if *idle_fence_xid != 0 => {
                match self.dri3_xshmfence_handle(*idle_fence_xid) {
                    Some(h) => PinnedWake::Pixmap(h),
                    None => PinnedWake::None,
                }
            }
            PresentWake::PixmapSynced { release_syncobj, release_value } if *release_syncobj != 0 => {
                match self.dri3_syncobj_handle(*release_syncobj) {
                    Some(h) => PinnedWake::PixmapSynced { handle: h, value: *release_value },
                    None => PinnedWake::None,
                }
            }
            _ => PinnedWake::None,
        };
        if let Err(e) = crate::kms::v2::present_completion::enqueue(
            &mut self.pending_present_events,
            &self.platform,
            event,
            ticket,
            wake_pin,
        ) {
            log::warn!("enqueue_present_completion failed: {e}");
        }
    }
```

- [ ] **Step 5: Run, confirm pass**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
cargo test -p yserver --lib
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --test v2_acceptance enqueue_present_completion -- --ignored
```
Expected: green.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver/src/kms/v2/present_completion.rs \
        crates/yserver/src/kms/v2/backend.rs
git commit -m "feat(v2): enqueue + drain deferred PRESENT completions

Stage 5 Task 6.1 backend internals. enqueue_present_completion
captures the current cow_batch ticket, takes Arc clones of the
wake-primitive (xshmfence or syncobj) for lifetime pinning,
exports the fence as a sync_file FD via vkGetFenceFdKHR, and
either registers the FD with the inner epoll (unsignaled case)
or writes the wakeup_eventfd (already-signaled case).

drain_completed_present_events walks the front of the queue,
pops entries whose fence_fd is None or whose ticket has
signaled, fires the wake signal via the Arc-pinned handle
(dri3_trigger_fence_via_handle / dri3_signal_syncobj_via_handle),
unregisters + closes the per-entry FD, and returns the public
event payloads.

No PRESENT handler change yet — old synchronous wait still in
place. Next commit replaces it with the enqueue call."
```

---

## Task 10: Extract `fire_present_completion_events` helper

The existing inline fan-out at `process_request.rs:5113-5170` (plus the helper at ~5384) fires `IdleNotify` + `CompleteNotify`. Extract into a single helper callable from both the legacy path (until Task 12 lands) and the new drain hook.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs`

- [ ] **Step 1: Find the existing fan-out**

```
grep -nE "fan out CompleteNotify|fire.*Idle|emit.*CompleteNotify" \
  crates/yserver-core/src/core_loop/process_request.rs | head
```

The fan-out helper is at line ~5384 per the spec. Find its exact signature.

- [ ] **Step 2: Verify it already operates on the data the new path needs**

If the helper currently takes `(state, &PresentPixmapRequest)` and reads fields, refactor it to take `(state, &CompletedPresentEvent)` instead — same data, just sourced from a different struct. The trick: `CompletedPresentEvent` carries `client_id, serial, host_xid, dst_host_xid, options, wake`; the existing helper likely reads `req.serial, req.window, req.options, req.idle_fence`, etc. Map fields:

| Existing | CompletedPresentEvent |
|---|---|
| `req.serial` | `event.serial` |
| `req.window` | `event.dst_host_xid` (depending on host vs client xid distinction) |
| `req.options` | `event.options` |
| `req.idle_fence` | `event.wake` (via `match`) |

The xid distinction matters: clients send client xids; the backend's xshmfence + sync_fences tables are indexed by host xids in some paths and client xids in others. Verify which the existing helper uses; pass the matching one through `CompletedPresentEvent`.

- [ ] **Step 3: Rename + extract**

Rename the helper to `fire_present_completion_events`. Make it take `(state: &mut ServerState, event: &CompletedPresentEvent)`. Move the body that emits `IdleNotify` first (per the existing `process_request.rs:5513` comment) then `CompleteNotify { mode: Copy }`.

```rust
/// Stage 5 Task 6.1: fan out IdleNotify (first) + CompleteNotify
/// (second) for a completed PRESENT entry. Called both from the
/// legacy synchronous PRESENT path (until that's removed in the
/// drain-wire-up commit) and from the new main-loop drain hook.
pub(crate) fn fire_present_completion_events(
    state: &mut ServerState,
    event: &CompletedPresentEvent,
) {
    // IdleNotify first — Mesa expects this order; see existing
    // process_request.rs:5513 comment.
    // ... existing IdleNotify emission body ...

    // CompleteNotify { mode: Copy } second.
    // ... existing CompleteNotify emission body ...
}
```

The existing inline emission site at `process_request.rs:5113-5170` calls the new helper instead:

```rust
fire_present_completion_events(state, &CompletedPresentEvent {
    client_id,
    serial: req.serial,
    host_xid: host_xid.as_raw(),
    dst_host_xid: dst.host_xid(),
    options: masked_options,
    wake: PresentWake::Pixmap { idle_fence_xid: req.idle_fence },
});
```

Note: this is the legacy path that still runs `wait_for_drawable_idle` synchronously. We're not removing that yet — just refactoring the emission helper. Task 12 deletes the wait + sites this same helper from the drain hook.

- [ ] **Step 4: Run all tests**

```
cargo +nightly fmt
cargo clippy -p yserver-core --tests -- -D warnings
cargo test -p yserver-core --lib
cargo test -p yserver --lib
```
Expected: green; behaviour unchanged.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/core_loop/process_request.rs
git commit -m "refactor(present): extract fire_present_completion_events helper

Stage 5 Task 6.1 prep. The existing inline IdleNotify +
CompleteNotify fan-out at process_request.rs:5113-5170 is
extracted into a public helper taking
(state, &CompletedPresentEvent). Both the legacy synchronous
PRESENT path and the new deferred-completion drain hook will
share emission logic. Behaviour unchanged."
```

---

## Task 11: Wire drain dispatch in run_core

Replace the trace-log stub from Task 1 with the real drain + emit.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/run.rs`

- [ ] **Step 1: Locate the stub arm**

```
grep -n "PRESENT_COMPLETION_TOKEN" crates/yserver-core/src/core_loop/run.rs
```

- [ ] **Step 2: Replace the stub with the drain call**

```rust
            PRESENT_COMPLETION_TOKEN => {
                let completed = backend.drain_completed_present_events();
                for entry in completed {
                    if let PresentWake::Pixmap { idle_fence_xid } = entry.wake {
                        if idle_fence_xid != 0 {
                            if let Some(f) = state.sync_fences.get_mut(&idle_fence_xid) {
                                f.triggered = true;
                            }
                        }
                    }
                    // Wake-signal already fired inside the backend's
                    // drain via the Arc-pinned handle; we only do
                    // X11-side event fan-out here.
                    crate::core_loop::process_request::fire_present_completion_events(
                        state,
                        &entry,
                    );
                }
            }
```

- [ ] **Step 3: Run + commit**

```
cargo +nightly fmt
cargo clippy -p yserver-core --tests -- -D warnings
cargo test -p yserver-core --lib
cargo test -p yserver --lib
```
Expected: green.

```bash
git add crates/yserver-core/src/core_loop/run.rs
git commit -m "feat(core-loop): drain + emit deferred PRESENT completions

Stage 5 Task 6.1 main-loop wire-up. The PRESENT_COMPLETION_TOKEN
arm now calls backend.drain_completed_present_events() and fires
fire_present_completion_events for each returned entry. The
backend has already signalled the underlying primitive via the
Arc-pinned handle by the time drain returns; this hook only
does X11 event fan-out + state.sync_fences bookkeeping."
```

---

## Task 12: Replace `PRESENT::Pixmap` synchronous wait with enqueue

Delete the synchronous `wait_for_drawable_idle` call + the immediate event emission + the immediate `dri3_trigger_fence`. Replace with `backend.enqueue_present_completion(...)`.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:5040-5170`

- [ ] **Step 1: Write the failing test**

```rust
// In v2_acceptance.rs
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_present_pixmap_enqueues_pending_and_defers_emission() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // Drive a synthetic PRESENT::Pixmap-shaped sequence:
    // 1. cow_copy_area into the COW
    // 2. enqueue_present_completion
    // Assert: pending_present_events grew by 1; no synchronous wait was
    //         taken; drain returns empty (fence not signaled yet).

    // Construct dst (COW analog) + src pixmap via the test fixture.
    let src_pix = b.create_pixmap(None, 32, 4, 4).expect("src pixmap");
    let cow_pix = b.create_pixmap(None, 32, 4, 4).expect("cow pixmap");
    b.copy_area(None, src_pix.as_raw(), cow_pix.as_raw(), 0, 0, 0, 0, 4, 4)
        .expect("copy_area");
    let before = std::time::Instant::now();
    b.enqueue_present_completion(yserver_core::backend::CompletedPresentEvent {
        client_id: yserver_protocol::x11::ClientId(0),
        serial: 1,
        host_xid: src_pix.as_raw(),
        dst_host_xid: cow_pix.as_raw(),
        options: 0,
        wake: yserver_core::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
    });
    let elapsed = before.elapsed();
    assert!(elapsed.as_millis() < 50,
        "enqueue must be fast (< 50 ms); was {} ms",
        elapsed.as_millis());
    // Drain returns empty since fence isn't signaled yet.
    let drained = b.drain_completed_present_events();
    assert!(drained.is_empty(), "drain returns empty pre-signal");
}
```

- [ ] **Step 2: Patch the handler**

In `crates/yserver-core/src/core_loop/process_request.rs:5055`, delete:

```rust
                backend.wait_for_drawable_idle(dst.host_xid())?;
```

In the immediate-emission block at ~5099-5170, delete:
- The `dri3_trigger_fence` call (the deferred drain does it via the Arc handle)
- The `sync_fences.triggered` direct mutation (the drain hook does it)
- The inline `fire_present_completion_events` call (the drain hook fires)

Replace with:

```rust
                backend.enqueue_present_completion(yserver_core::backend::CompletedPresentEvent {
                    client_id,
                    serial: req.serial,
                    host_xid: host_xid.as_raw(),
                    dst_host_xid: dst.host_xid(),
                    options: masked_options,
                    wake: yserver_core::backend::PresentWake::Pixmap {
                        idle_fence_xid: req.idle_fence,
                    },
                });
                backend.note_present_pixmap(host_xid.as_raw(), dst.host_xid());
```

Keep the surrounding code: damage accumulation, present_msc bookkeeping, present_scheduler.enqueue.

- [ ] **Step 3: Run + commit**

```
cargo +nightly fmt
cargo clippy -p yserver-core --tests -- -D warnings
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --test v2_acceptance v2_present_pixmap_enqueues -- --ignored
```
Expected: green.

```bash
git add crates/yserver-core/src/core_loop/process_request.rs \
        crates/yserver/tests/v2_acceptance.rs
git commit -m "feat(present): defer PRESENT::Pixmap completion to drain hook

Stage 5 Task 6.1 site #1. process_request.rs:5055 no longer
synchronously waits via wait_for_drawable_idle; instead it
enqueues a CompletedPresentEvent via
backend.enqueue_present_completion. The xshmfence trigger +
IdleNotify + CompleteNotify all fire from the main-loop drain
hook when the cow_batch fence signals."
```

---

## Task 13: Replace `PRESENT::PixmapSynced` synchronous wait + signal with enqueue

Same shape as Task 12 but for the explicit-sync variant. Replaces `dri3_signal_syncobj(release_syncobj, release_value)` immediate call with the deferred enqueue.

**Files:**
- Modify: `crates/yserver-core/src/core_loop/process_request.rs:5214-5310`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_present_pixmap_synced_enqueues_with_release_syncobj_wake() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // Same shape as v2_present_pixmap_enqueues_pending_and_defers_emission
    // but using PresentWake::PixmapSynced. Assert: enqueue captures
    // the syncobj handle via dri3_syncobj_handle (Arc); drain emits
    // with the original release_syncobj + release_value preserved.

    let src_pix = b.create_pixmap(None, 32, 4, 4).expect("src");
    let cow_pix = b.create_pixmap(None, 32, 4, 4).expect("cow");
    b.copy_area(None, src_pix.as_raw(), cow_pix.as_raw(), 0, 0, 0, 0, 4, 4)
        .expect("copy");
    b.enqueue_present_completion(yserver_core::backend::CompletedPresentEvent {
        client_id: yserver_protocol::x11::ClientId(0),
        serial: 2,
        host_xid: src_pix.as_raw(),
        dst_host_xid: cow_pix.as_raw(),
        options: 0,
        wake: yserver_core::backend::PresentWake::PixmapSynced {
            release_syncobj: 0, // 0 means "no wake object"; just exercises enqueue
            release_value: 42,
        },
    });
    let drained = b.drain_completed_present_events();
    assert!(drained.is_empty(), "drain returns empty pre-signal");
}
```

- [ ] **Step 2: Patch the handler**

In `crates/yserver-core/src/core_loop/process_request.rs:5214` (the `x11present::PIXMAP_SYNCED` arm). Find lines :5284 (the sync wait) and :5300-5308 (the immediate `dri3_signal_syncobj`). Replace both with an `enqueue_present_completion` call:

```diff
                 src_depth == dst.depth()
                     && backend.copy_area(/* ... */).is_ok()
-                    && backend.wait_for_drawable_idle(dst.host_xid()).is_ok()
+                    && {
+                        backend.enqueue_present_completion(
+                            yserver_core::backend::CompletedPresentEvent {
+                                client_id,
+                                serial: req.serial,
+                                host_xid: host_xid.as_raw(),
+                                dst_host_xid: dst.host_xid(),
+                                options: masked_options,
+                                wake: yserver_core::backend::PresentWake::PixmapSynced {
+                                    release_syncobj: req.release_syncobj,
+                                    release_value: req.release_value,
+                                },
+                            },
+                        );
+                        true
+                    }
```

And delete the immediate-signal block at :5300-5308:

```rust
-            if req.release_syncobj != 0
-                && let Err(e) = backend.dri3_signal_syncobj(req.release_syncobj, req.release_value)
-            {
-                log::warn!(
-                    "PRESENT::PixmapSynced: signalling release_syncobj 0x{:x} @ {} failed: {e}",
-                    req.release_syncobj,
-                    req.release_value
-                );
-            }
```

- [ ] **Step 3: Run + commit**

```
cargo +nightly fmt
cargo clippy -p yserver-core --tests -- -D warnings
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --test v2_acceptance v2_present_pixmap_synced -- --ignored
```
Expected: green.

```bash
git add crates/yserver-core/src/core_loop/process_request.rs \
        crates/yserver/tests/v2_acceptance.rs
git commit -m "feat(present): defer PRESENT::PixmapSynced completion to drain hook

Stage 5 Task 6.1 site #2. process_request.rs:5284 no longer
synchronously waits via wait_for_drawable_idle; the immediate
dri3_signal_syncobj at :5300-5308 is also removed. Both
mechanisms fire from the main-loop drain hook when the
cow_batch fence signals, with the underlying syncobj kept
alive past mid-flight FreeSyncobj via the Arc-pinned handle
on the PendingPresentEntry."
```

---

## Task 14: `disable_output` — flush + drain pending PRESENT events

The shutdown sequence needs to flush open cow/render batches into `submitted` before `engine.drain_all` walks the queue, and then drain the pending PRESENT events queue.

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs:1669` (`disable_output`)
- Modify: `crates/yserver/src/lib.rs` (call-site adaption for the returned events)

- [ ] **Step 1: Write the failing test**

```rust
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_disable_output_flushes_pending_batches_before_drain_all() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    // Open a cow_batch + enqueue a pending PRESENT entry.
    let src = b.create_pixmap(None, 32, 4, 4).expect("src");
    let cow = b.create_pixmap(None, 32, 4, 4).expect("cow");
    b.copy_area(None, src.as_raw(), cow.as_raw(), 0, 0, 0, 0, 4, 4)
        .expect("copy");
    b.enqueue_present_completion(yserver_core::backend::CompletedPresentEvent {
        client_id: yserver_protocol::x11::ClientId(0),
        serial: 1,
        host_xid: src.as_raw(),
        dst_host_xid: cow.as_raw(),
        options: 0,
        wake: yserver_core::backend::PresentWake::Pixmap { idle_fence_xid: 0 },
    });

    // Sanity: batch is open + entry is pending.
    assert!(b.has_pending_batches_for_tests());
    assert_eq!(b.pending_present_events_len_for_tests(), 1);

    // Call disable_output.
    b.disable_output().expect("disable_output ok");

    // Post-shutdown: no batches, no pending events.
    assert!(!b.has_pending_batches_for_tests());
    assert_eq!(b.pending_present_events_len_for_tests(), 0);
}
```

`has_pending_batches_for_tests` already exists from the bee fix's regression test. Add `pending_present_events_len_for_tests` as a sibling `#[doc(hidden)] pub fn` accessor.

- [ ] **Step 2: Modify `disable_output`**

In `crates/yserver/src/kms/v2/backend.rs:1669`:

```rust
    pub fn disable_output(&mut self) -> io::Result<()> {
        // Stage 5 Task 6.1: explicitly flush open cow/render batches
        // before drain_all walks the submitted queue. drain_all only
        // waits on already-submitted CBs; an open pending batch
        // wouldn't be there yet.
        if let Err(e) = self.engine.flush_cow_batch(&mut self.store, &mut self.platform) {
            log::warn!("v2 disable_output: flush_cow_batch failed: {e:?}");
        }
        if let Err(e) = self.engine.flush_render_batch(&mut self.store, &mut self.platform) {
            log::warn!("v2 disable_output: flush_render_batch failed: {e:?}");
        }

        // Existing drain path.
        self.engine.drain_all(&self.platform);
        self.sync_descriptor_pool_telemetry();
        self.scene.drain_all(&mut self.platform);

        // Stage 5 Task 6.1: drain the pending PRESENT events queue
        // unconditionally. After drain_all, every cow_batch ticket
        // is signaled or the renderer failed; force-fire all
        // entries' events + close per-entry FDs.
        let completed = self.drain_completed_present_events_impl();
        // Caller (lib.rs::run) will fan these out to clients before
        // tearing down the socket; this method doesn't have access to
        // ServerState so we return them upward via a new method.
        self.pending_completed_events_on_shutdown.extend(completed);

        // Force-fire any entries whose ticket *didn't* signal (renderer
        // failure path).
        for entry in self.pending_present_events.drain(..) {
            if let Some(fd) = entry.fence_fd {
                let _ = rustix::event::epoll::delete(
                    &self.platform.present_completion_epfd,
                    &fd,
                );
            }
            self.pending_completed_events_on_shutdown.push(entry.event);
        }

        self.telemetry.flush_submit_trace();
        self.platform.disable_output()
    }
```

This adds a new field `pending_completed_events_on_shutdown: Vec<CompletedPresentEvent>` on `KmsBackendV2` so the caller (`lib.rs::run`) can drain it after `disable_output` returns:

```rust
    pub(crate) pending_completed_events_on_shutdown:
        Vec<yserver_core::backend::CompletedPresentEvent>,
```

And a new accessor:

```rust
    pub fn take_shutdown_present_events(
        &mut self,
    ) -> Vec<yserver_core::backend::CompletedPresentEvent> {
        std::mem::take(&mut self.pending_completed_events_on_shutdown)
    }
```

- [ ] **Step 3: Call from `lib.rs`**

In `crates/yserver/src/lib.rs:295`:

```rust
    log::info!("yserver: shutting down, disabling output");
    if let Err(e) = backend.disable_output() {
        log::warn!("yserver: disable_output failed: {e}");
    }
    // Stage 5 Task 6.1: fan out any PRESENT completions that were
    // deferred past shutdown drain — events must reach clients before
    // we tear down the socket.
    for entry in backend.take_shutdown_present_events() {
        crate::core_loop::process_request::fire_present_completion_events(
            &mut state,
            &entry,
        );
    }
    let _ = fs::remove_file(&socket_path);
    log::info!("yserver: master released, exiting");
```

(This requires `take_shutdown_present_events` on the Backend trait too, with a default returning `Vec::new()`.)

- [ ] **Step 4: Run + commit**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --test v2_acceptance v2_disable_output_flushes -- --ignored
```
Expected: green.

```bash
git add crates/yserver/src/kms/v2/backend.rs \
        crates/yserver/src/lib.rs \
        crates/yserver-core/src/backend/trait_def.rs \
        crates/yserver/tests/v2_acceptance.rs
git commit -m "feat(v2): disable_output flushes pending batches + drains PRESENT queue

Stage 5 Task 6.1 shutdown path. disable_output now explicitly
flushes open cow + render batches before engine.drain_all, then
drains the pending PRESENT events queue unconditionally and
hands the events to lib.rs::run for client-side fan-out before
the socket is torn down. Closes the shutdown-hang side effect
of the bee fix (no synchronous wait + correct drain ordering)."
```

---

## Task 15: Verify the bee fix's synchronous-wait machinery is fully gone

Cleanup pass. The bee fix (`8ca552a`) added two layers: eager-touch (kept) + flush-before-wait (deleted). Confirm the deletion is complete and `wait_for_drawable_idle` has no remaining callers.

**Files:**
- Read-only: `crates/yserver/src/kms/v2/backend.rs:4244` (`wait_for_drawable_idle` body)
- Read-only: `crates/yserver-core/src/core_loop/process_request.rs` (no more callers)

- [ ] **Step 1: Confirm zero callers**

```
grep -nE "wait_for_drawable_idle" crates/yserver-core crates/yserver -r
```

Expected: only the method definition + perhaps test code. Zero callers from `process_request.rs`. If any caller survives, identify which path and either:
- Migrate it to the deferred-completion mechanism (similar to Task 12/13), or
- Document why it stays.

- [ ] **Step 2: Confirm the flush-before-wait body in `wait_for_drawable_idle` is gone**

If the body still contains the flush+wait sequence from `8ca552a`, this is a code smell — the method is now dead. Either delete the method entirely or, if you want to keep the surface for future callers, simplify to just the ticket wait (no flush). Either is acceptable; deleting is cleaner.

- [ ] **Step 3: Confirm eager-touch in `engine.rs` is intact**

```
grep -n "store.touch_render_fence" crates/yserver/src/kms/v2/engine.rs
```

The `8ca552a` eager-touch calls at the `cow_copy_area` + `render_composite_open` + `render_composite` appender sites must remain. These are what close the use-after-free. The deferred-completion design depends on them staying.

- [ ] **Step 4: Optionally delete `wait_for_drawable_idle`**

If you choose deletion:

```rust
// Delete the method body + trait method (or leave a trait default
// returning Ok(())). Note: tests like v2_wait_for_drawable_idle_
// flushes_pending_batches that were added with 8ca552a also need
// deletion or rewrite.
```

The bee fix's regression tests at `crates/yserver/tests/v2_acceptance.rs::v2_wait_for_drawable_idle_flushes_pending_batches` (if present) need to be either deleted (the property they assert is no longer load-bearing) or rewritten to assert the new deferred-completion behaviour.

- [ ] **Step 5: Run full test matrix + commit**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
cargo test -p yserver --lib
cargo test -p yserver-core --lib
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --test v2_acceptance -- --ignored
```
Expected: all green; clippy clean.

```bash
git add -A
git commit -m "chore(v2): remove dead wait_for_drawable_idle + obsolete bee-fix tests

Stage 5 Task 6.1 cleanup. After deferring PRESENT completion to
the drain hook (Tasks 12-13), wait_for_drawable_idle has zero
callers. The method body (flush + sync wait, added in 8ca552a)
is removed along with the v2_wait_for_drawable_idle_flushes_
pending_batches regression test that gated it. The eager-touch
layer of 8ca552a stays — that's the load-bearing UAF closure."
```

---

## Task 16: Hardware verification + status doc

Run the captures the design is gated on. Update `docs/status.md`.

- [ ] **Step 1: Yoga capture**

```
just yserver-mate-hw-telemetry
```
SSH from another machine + `pkill -TERM yserver` for clean exit. Drag MATE for ~60 s.

Expected vs the 2026-05-22 reverted-state target (yoga 1× scale):

| metric | target |
|---|---|
| cow batch depth avg | ≥ 7 |
| render batch depth avg | ≥ 2.0 |
| `cpu_fence_wait_ns/s` | near zero |
| `paint_submits/s` drag avg | ≤ 3000 |
| Subjective | snappy, low CPU, no spikes |

- [ ] **Step 2: Bee capture (if hardware accessible)**

`just yserver-mate-hw` — confirm:
- No `ERROR_DEVICE_LOST` flood
- No `addr_binding_report` fault
- The original UAF symptom does not reproduce
- Drag lag floor substantially lower than the synchronous-wait era

If bee isn't accessible at landing time, defer hardware validation and note in the status entry that bee validation is pending.

- [ ] **Step 3: Silence capture (regression check)**

`just yserver-mate-hw-telemetry` on silence — confirm no perf regression vs the post-Task-3 baseline (paint_submits drag avg ≤ 4200, cow batch depth ≥ 5).

- [ ] **Step 4: Update `docs/status.md`**

Add a new entry under Stage 5 Task 6.1, replacing or amending the existing "Task 6.1 - PRESENT IN_FENCE_FD" entry. Use the format the existing Task 4 layer 1 entry uses (search for "Task 4 layer 1 — DescriptorPoolRing" in status.md). Sample shape:

```markdown
  - [x] **Task 6.1 — Deferred PRESENT completion.** Landed
    2026-05-XX. Spec: `2026-05-23-deferred-present-completion-design.md`;
    plan: `2026-05-23-deferred-present-completion.md`. Commit
    `8ca552a` (bee use-after-free + PRESENT wait deadlock fix)
    replaced with an asynchronous deferred-completion design:
    PRESENT::Pixmap and PRESENT::PixmapSynced handlers enqueue a
    CompletedPresentEvent capturing the cow_batch fence ticket +
    an Arc-pinned clone of the wake primitive, then return
    immediately. A main-loop drain hook fires xshmfence_trigger /
    dri3_signal_syncobj + IdleNotify + CompleteNotify when the
    cow_batch fence signals. No new KMS plumbing — the existing
    scene-compose IN_FENCE_FD covers pageflip-side correctness
    via FIFO submission order.

    [yoga + bee + silence capture numbers + verdict]
```

- [ ] **Step 5: Commit**

```bash
git add docs/status.md
# also any captured artefacts under docs/captures/
git commit -m "docs(status): close Stage 5 Task 6.1 (deferred PRESENT completion)

PRESENT request handlers no longer synchronously wait on the GPU.
xshmfence trigger + IdleNotify + CompleteNotify deferred to the
main-loop drain hook; lifetime-pinned by Arc against mid-flight
FreeSyncobj / XFixesDestroyFence. Yoga MATE drag now matches the
2026-05-22 reverted-state target (cow batch depth avg ≥ 7,
cpu_fence_wait near zero, native subjective drag). Bee UAF stays
closed via the retained eager-touch layer."
```

---

## Out-of-scope for this plan

- **Telemetry counters per spec §"Telemetry"** (`pending_present_events_depth_max`, `pending_present_events_emitted_per_s`, `pending_present_events_force_fired_per_s`). Diagnostic-only; not load-bearing for correctness. Add as a one-line `record_*` extension to `crates/yserver/src/kms/v2/telemetry.rs` after the core mechanism is shipped if the per-second emitter line is useful for the next hardware capture.
- **`RendererFailed`-specific drain semantics per spec §"Error handling".** The implementation in Task 9 handles the happy path; the spec calls out that on `renderer_failed == true`, the drain should fire all entries unconditionally with one warn log. The implementer extends Task 9's drain with this branch using the same pattern as `engine.drain_all`'s renderer-failure check. Same shape, mechanical addition.

## Done — verification checklist

- [ ] `cargo +nightly fmt` clean.
- [ ] `cargo clippy -p yserver -p yserver-core --tests -- -D warnings` clean (no `clippy::pedantic` per AGENTS.md).
- [ ] `cargo test -p yserver --lib` + `cargo test -p yserver-core --lib` green.
- [ ] `cargo test -p yserver --test v2_acceptance -- --ignored` green under lavapipe; includes the new Task 12/13 enqueue tests + Task 14 shutdown-drain test.
- [ ] `grep -n "wait_for_drawable_idle" crates/yserver-core crates/yserver -r` returns only the method definition (or zero lines if deleted in Task 15).
- [ ] Yoga MATE drag: cow batch depth avg ≥ 7, no CPU spikes, subjective drag feels native.
- [ ] Bee MATE drag (if accessible): no `ERROR_DEVICE_LOST`, no UAF reproduction, lag floor substantially better than 8ca552a state.
- [ ] Silence MATE drag: no perf regression vs post-Task-3 baseline.
- [ ] `docs/status.md` updated.
