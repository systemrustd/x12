# DescriptorPoolRing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace per-call `BatchDescriptorArena` allocation in v2's `try_vk_render_composite` and `try_vk_render_traps_or_tris` with a long-lived `DescriptorPoolRing`, eliminating the dominant `vkCreateDescriptorPool` → Turnip shmem allocator hot path identified in the 2026-05-21 yoga perf capture.

**Architecture:** A `DescriptorPoolRing` owned by `EngineInner` rotates through Free/Active/InFlight pools using a per-op `acquire_generation` watermark. Pools are reset (not destroyed) when the generation tag of all sets in a pool has retired through the engine's `submitted` FIFO. `vkResetDescriptorPool` failure poisons the entire ring (hard error). The legacy `BatchDescriptorArena` stays in tree untouched — v1 still calls it through `PaintBatch::descriptor_arena_mut()`.

**Tech Stack:** Rust 2021, ash (Vulkan), v2 rendering backend (`crates/yserver/src/kms/v2/`).

**Spec:** `docs/superpowers/specs/2026-05-21-descriptor-pool-ring-design.md`

**Pre-flight check:** Before starting, verify the working branch already includes Stage 4 close (commit `18c1954 docs(status): close Stage 4 + archive cow-authoritative diagnosis`). Run `cargo test -p yserver --lib` to confirm baseline tests pass (you should see the existing v2 lib test count green; any pre-existing flake should be flagged before continuing).

---

## File Map

**Created:**
- `crates/yserver/src/kms/v2/descriptor_pool_ring.rs` — the ring type, state machine, Drop, all unit tests.

**Modified:**
- `crates/yserver/src/kms/v2/mod.rs` — register the new module.
- `crates/yserver/src/kms/v2/telemetry.rs` — two new bucket fields, two `record_*` methods, emitter line addition.
- `crates/yserver/src/kms/vk/render_pipeline.rs` — add `allocate_descriptor_for_views_into_ring` method; factor the descriptor-write block into a shared private helper.
- `crates/yserver/src/kms/v2/engine.rs` — add `descriptor_pool_ring` + `acquire_generation` to `EngineInner`; add `generation: u64` to `SubmittedOp`; remove `descriptor_arena: Option<BatchDescriptorArena>` from `SubmittedOp` and from all 10 `SubmittedOp { ... }` literal sites that set it to `None`; switch the two real-arena sites (`engine.rs:2867`, `engine.rs:3413`) to the ring; call `descriptor_pool_ring.release_up_to(op.generation)` in `release_retired_ops` and `drain_all`; add `descriptor_pool_creates_lifetime()` / `descriptor_pool_resets_lifetime()` accessors.
- `crates/yserver/src/kms/v2/backend.rs` — add `last_observed_pool_creates`/`last_observed_pool_resets` snapshot fields; private `sync_descriptor_pool_telemetry()` helper called after each RENDER engine call site (`render_composite`, `render_fill_rectangles`, `render_trapezoids`/`render_triangles`) and after each `engine.poll_retired` / `engine.drain_all` site.
- `crates/yserver/tests/v2_acceptance.rs` — two new acceptance tests covering the Composite and Trapezoid call sites with the three-assertion shape (bounded creates + observed resets + bounded pool_count).
- `docs/status.md` — log the Task 4 layer 1 close under Stage 5 progress.

**Left untouched (v1 path):**
- `crates/yserver/src/kms/scheduler/batch_descriptor_arena.rs` — v1's `KmsBackend` still calls this via `paint_batch.descriptor_arena_mut()` (`backend.rs:5232`, `:6272`). Out of scope.
- `crates/yserver/src/kms/scheduler/paint_batch.rs` — same, untouched.

---

## Conventions

- Every Rust source change must end in a clean compile (`cargo build -p yserver`), clippy clean (`cargo clippy -p yserver --tests -- -D warnings`), and the touched test set green. Per `AGENTS.md`, **do not** use `clippy::pedantic` in this repo.
- Per global `~/.claude/CLAUDE.md` rules: `cargo +nightly fmt` before commits (this repo uses unstable rustfmt features). Do not `--amend`.
- Vk-backed tests are gated `#[ignore = "needs live Vulkan ICD"]` matching the existing v2 pattern. Run them with:
  ```
  VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
    cargo test -p yserver --lib descriptor_pool_ring -- --ignored
  ```
  and for acceptance:
  ```
  VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
    cargo test -p yserver --test v2_acceptance -- --ignored
  ```
- Commit cadence: one logical change per commit, conventional-commit prefix, no Claude/AI footer.

---

## Task 1: Telemetry primer

Add the two counter fields, `record_*` methods, and emit-line tokens. Purely additive; no call sites wired yet — those land in Task 9. The unit test verifies the records bump both bucket + lifetime, matching the existing telemetry pattern.

**Files:**
- Modify: `crates/yserver/src/kms/v2/telemetry.rs`

- [ ] **Step 1: Write the failing test**

Append inside the existing `#[cfg(test)] mod tests` block in `crates/yserver/src/kms/v2/telemetry.rs`:

```rust
    #[test]
    fn descriptor_pool_counters_accumulate() {
        let mut t = Telemetry::new();
        t.record_descriptor_pool_create();
        t.record_descriptor_pool_create();
        t.record_descriptor_pool_reset(3);
        assert_eq!(t.lifetime.descriptor_pool_creates, 2);
        assert_eq!(t.lifetime.descriptor_pool_resets, 3);
    }
```

- [ ] **Step 2: Run test, confirm it fails to compile**

```
cargo test -p yserver --lib telemetry::tests::descriptor_pool_counters_accumulate
```
Expected: compile error — `record_descriptor_pool_create` / `record_descriptor_pool_reset` do not exist, `Bucket::descriptor_pool_creates` does not exist.

- [ ] **Step 3: Add bucket fields**

In `crates/yserver/src/kms/v2/telemetry.rs`, add to the `Bucket` struct (after `disjoint_readback_count: u64,`):

```rust
    /// Stage 5 Task 4 layer 1: vkCreateDescriptorPool calls in this
    /// second. Should reach a near-zero floor after warm-up under
    /// the descriptor-pool-ring design (spec 2026-05-21).
    pub descriptor_pool_creates: u64,
    /// Stage 5 Task 4 layer 1: vkResetDescriptorPool calls in this
    /// second. Tracks paint_submits/s / SETS_PER_POOL on a healthy
    /// recycle path.
    pub descriptor_pool_resets: u64,
```

- [ ] **Step 4: Add `record_*` methods**

In the `impl Telemetry` block, after `record_disjoint_readback`, add:

```rust
    /// Stage 5 Task 4 layer 1: one `vkCreateDescriptorPool` site
    /// inside `DescriptorPoolRing::acquire_set` (no-Free-slot growth
    /// branch).
    pub(crate) fn record_descriptor_pool_create(&mut self) {
        self.bucket.descriptor_pool_creates += 1;
        self.lifetime.descriptor_pool_creates += 1;
    }

    /// Stage 5 Task 4 layer 1: bumped once per `vkResetDescriptorPool`
    /// `Ok` arm inside `DescriptorPoolRing::release_up_to`. `n` is
    /// the number of pools the call reset in a single sweep (the
    /// return value of `release_up_to`).
    pub(crate) fn record_descriptor_pool_reset(&mut self, n: u64) {
        self.bucket.descriptor_pool_resets =
            self.bucket.descriptor_pool_resets.saturating_add(n);
        self.lifetime.descriptor_pool_resets =
            self.lifetime.descriptor_pool_resets.saturating_add(n);
    }
```

- [ ] **Step 5: Extend the emitter line**

In `maybe_emit`, append two tokens to the existing `log::info!("v2_telemetry: ...", ...)` formatter. Add to the format string (before `avg_gpu_render_ns=...`):

```
descriptor_pool_creates/s={} descriptor_pool_resets/s={} \
```

and pass `b.descriptor_pool_creates, b.descriptor_pool_resets,` in matching argument order.

- [ ] **Step 6: Run test, confirm pass**

```
cargo test -p yserver --lib telemetry::tests::descriptor_pool_counters_accumulate
```
Expected: PASS.

- [ ] **Step 7: Run full lib tests to confirm no regression**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
cargo test -p yserver --lib
```
Expected: all green, clippy clean, formatter quiet.

- [ ] **Step 8: Commit**

```bash
git add crates/yserver/src/kms/v2/telemetry.rs
git commit -m "feat(v2/telemetry): primer for descriptor pool ring counters

Adds descriptor_pool_creates and descriptor_pool_resets to the v2
telemetry bucket + lifetime, with sites left dark until the
DescriptorPoolRing call sites wire in (spec 2026-05-21)."
```

---

## Task 2: DescriptorPoolRing skeleton

Create the ring module, types, constructor, Drop, and inspection accessors used by tests. No paint integration yet. The unit test verifies the just-constructed ring is empty + state_counts reports all zeros + Drop is clean on an empty ring.

**Files:**
- Create: `crates/yserver/src/kms/v2/descriptor_pool_ring.rs`
- Modify: `crates/yserver/src/kms/v2/mod.rs`

- [ ] **Step 1: Register module**

In `crates/yserver/src/kms/v2/mod.rs`, add (alphabetical):

```rust
pub(crate) mod descriptor_pool_ring;
```

between `cursor` and `engine`.

- [ ] **Step 2: Write the failing test**

Create `crates/yserver/src/kms/v2/descriptor_pool_ring.rs` with the test stub up front so the cycle is visible. Initial file:

```rust
//! Long-lived descriptor pool ring for the v2 RENDER paint path.
//!
//! Stage 5 Task 4 layer 1; spec at
//! `docs/superpowers/specs/2026-05-21-descriptor-pool-ring-design.md`.
//!
//! Replaces the per-RENDER-call `BatchDescriptorArena` instantiation
//! (which created + destroyed a `vk::DescriptorPool` per op) with a
//! cycle of Free/Active/InFlight slots keyed by an engine-supplied
//! `acquire_generation` watermark. Pools are reset on retirement, not
//! destroyed — the steady-state `vkCreateDescriptorPool/s` should
//! drop from thousands to near-zero after warm-up.

use std::sync::Arc;

use ash::vk;

use crate::kms::vk::device::VkContext;

const SETS_PER_POOL: u32 = 256;
const SAMPLERS_PER_POOL: u32 = 1024;
const UNIFORMS_PER_POOL: u32 = 256;
const STORAGE_PER_POOL: u32 = 64;

#[derive(Debug, PartialEq, Eq)]
enum PoolState {
    Free,
    Active,
    InFlight,
    Poisoned,
}

struct PoolSlot {
    pool: vk::DescriptorPool,
    state: PoolState,
    high_water_generation: u64,
    sets_remaining: u32,
}

pub(crate) struct DescriptorPoolRing {
    vk: Arc<VkContext>,
    pools: Vec<PoolSlot>,
    active: Option<usize>,
    lifetime_creates: u64,
    lifetime_resets: u64,
}

impl DescriptorPoolRing {
    pub(crate) fn new(vk: Arc<VkContext>) -> Self {
        Self {
            vk,
            pools: Vec::new(),
            active: None,
            lifetime_creates: 0,
            lifetime_resets: 0,
        }
    }

    pub(crate) fn pool_count(&self) -> usize {
        self.pools.len()
    }

    /// (Free, Active, InFlight, Poisoned)
    pub(crate) fn state_counts(&self) -> (usize, usize, usize, usize) {
        let mut counts = (0, 0, 0, 0);
        for s in &self.pools {
            match s.state {
                PoolState::Free => counts.0 += 1,
                PoolState::Active => counts.1 += 1,
                PoolState::InFlight => counts.2 += 1,
                PoolState::Poisoned => counts.3 += 1,
            }
        }
        counts
    }

    pub(crate) fn lifetime_creates(&self) -> u64 {
        self.lifetime_creates
    }

    pub(crate) fn lifetime_resets(&self) -> u64 {
        self.lifetime_resets
    }
}

impl Drop for DescriptorPoolRing {
    fn drop(&mut self) {
        for slot in self.pools.drain(..) {
            unsafe {
                self.vk.device.destroy_descriptor_pool(slot.pool, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::vk::device::VkContext;

    fn vk_or_skip() -> Option<Arc<VkContext>> {
        match VkContext::new() {
            Ok(vk) => Some(Arc::new(vk)),
            Err(e) => {
                eprintln!("skipping: no Vk: {e:?}");
                None
            }
        }
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn new_ring_is_empty() {
        let Some(vk) = vk_or_skip() else { return };
        let ring = DescriptorPoolRing::new(vk);
        assert_eq!(ring.pool_count(), 0);
        assert_eq!(ring.state_counts(), (0, 0, 0, 0));
        assert_eq!(ring.lifetime_creates(), 0);
        assert_eq!(ring.lifetime_resets(), 0);
        // Drop runs at end of scope; must not panic on empty ring.
    }
}
```

- [ ] **Step 3: Run the test**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --lib descriptor_pool_ring -- --ignored
```
Expected: 1 test passes (or "skipping: no Vk" if the ICD env is wrong, which is also acceptable for the skeleton — the file must compile + clippy-clean either way).

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/v2/mod.rs \
        crates/yserver/src/kms/v2/descriptor_pool_ring.rs
git commit -m "feat(v2): DescriptorPoolRing skeleton

Empty ring with pool_count / state_counts / lifetime_creates /
lifetime_resets inspection accessors. Drop destroys any pools (none
yet). Spec: 2026-05-21-descriptor-pool-ring-design.md."
```

---

## Task 3: `acquire_set` — grow on empty + Drop releases live pools

Land `acquire_set` and a layout helper. First test: empty ring + one acquire produces one pool and one set. Second test: Drop on a ring with one live pool is clean (no Vk validation warning on a debug ICD).

**Files:**
- Modify: `crates/yserver/src/kms/v2/descriptor_pool_ring.rs`

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block:

```rust
    fn make_layout(vk: &VkContext) -> vk::DescriptorSetLayout {
        let bindings = [vk::DescriptorSetLayoutBinding::default()
            .binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)];
        let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        unsafe {
            vk.device
                .create_descriptor_set_layout(&info, None)
                .expect("layout")
        }
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn acquire_grows_when_no_free_pool() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        let set = ring.acquire_set(layout, 1).expect("acquire");
        assert_ne!(set, vk::DescriptorSet::null());
        assert_eq!(ring.pool_count(), 1);
        // (Free, Active, InFlight, Poisoned)
        assert_eq!(ring.state_counts(), (0, 1, 0, 0));
        assert_eq!(ring.lifetime_creates(), 1);
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn drop_destroys_all_pools() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        {
            let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
            let _ = ring.acquire_set(layout, 1).expect("acquire");
            // Drop the ring here. If destroy_descriptor_pool leaks,
            // the validation layer flags it on VkDevice destroy.
        }
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }
```

- [ ] **Step 2: Run, confirm compile failure**

```
cargo test -p yserver --lib descriptor_pool_ring
```
Expected: compile error — `acquire_set` is undefined.

- [ ] **Step 3: Implement `acquire_set` (no rotate / no retry yet)**

In `descriptor_pool_ring.rs`, add to `impl DescriptorPoolRing` (private helpers first, then the public method). Replace the `impl DescriptorPoolRing { ... }` block with:

```rust
impl DescriptorPoolRing {
    pub(crate) fn new(vk: Arc<VkContext>) -> Self {
        Self {
            vk,
            pools: Vec::new(),
            active: None,
            lifetime_creates: 0,
            lifetime_resets: 0,
        }
    }

    pub(crate) fn pool_count(&self) -> usize {
        self.pools.len()
    }

    pub(crate) fn state_counts(&self) -> (usize, usize, usize, usize) {
        let mut counts = (0, 0, 0, 0);
        for s in &self.pools {
            match s.state {
                PoolState::Free => counts.0 += 1,
                PoolState::Active => counts.1 += 1,
                PoolState::InFlight => counts.2 += 1,
                PoolState::Poisoned => counts.3 += 1,
            }
        }
        counts
    }

    pub(crate) fn lifetime_creates(&self) -> u64 {
        self.lifetime_creates
    }

    pub(crate) fn lifetime_resets(&self) -> u64 {
        self.lifetime_resets
    }

    /// Acquire one descriptor set with `layout`, tagging the issuing
    /// pool with `generation`. Spec § "Architecture" / § "Internals
    /// on acquire_set": (1) ensure an Active pool exists + has
    /// capacity, (2) `vkAllocateDescriptorSets`, (3) bump the slot's
    /// `high_water_generation` and decrement `sets_remaining`.
    ///
    /// Errors map directly from Vk: `ERROR_OUT_OF_HOST_MEMORY`,
    /// `ERROR_OUT_OF_DEVICE_MEMORY`, etc. — propagated to the engine
    /// which converts to `RenderError::Vk`.
    pub(crate) fn acquire_set(
        &mut self,
        layout: vk::DescriptorSetLayout,
        generation: u64,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        if self.active.is_none() {
            self.create_pool_active()?;
        }
        let idx = self.active.expect("active set above");
        let pool = self.pools[idx].pool;
        let layouts = [layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        let sets = unsafe { self.vk.device.allocate_descriptor_sets(&alloc_info)? };
        let slot = &mut self.pools[idx];
        slot.sets_remaining = slot.sets_remaining.saturating_sub(1);
        if slot.high_water_generation < generation {
            slot.high_water_generation = generation;
        }
        Ok(sets[0])
    }

    fn create_pool_active(&mut self) -> Result<(), vk::Result> {
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                .descriptor_count(SAMPLERS_PER_POOL),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::UNIFORM_BUFFER)
                .descriptor_count(UNIFORMS_PER_POOL),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(STORAGE_PER_POOL),
        ];
        let info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(SETS_PER_POOL)
            .pool_sizes(&pool_sizes);
        let pool = unsafe { self.vk.device.create_descriptor_pool(&info, None)? };
        self.lifetime_creates += 1;
        self.pools.push(PoolSlot {
            pool,
            state: PoolState::Active,
            high_water_generation: 0,
            sets_remaining: SETS_PER_POOL,
        });
        self.active = Some(self.pools.len() - 1);
        Ok(())
    }
}
```

- [ ] **Step 4: Run, confirm pass**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --lib descriptor_pool_ring -- --ignored
```
Expected: 3 tests pass (new_ring_is_empty + acquire_grows_when_no_free_pool + drop_destroys_all_pools).

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/descriptor_pool_ring.rs
git commit -m "feat(v2/descriptor_pool_ring): acquire_set creates Active pool on demand

Empty ring grows one Active pool on first acquire_set call.
high_water_generation tracks the max generation tag of any set
issued; Drop destroys every pool. Spec § 'Internals on acquire_set'."
```

---

## Task 4: Rotate Active → InFlight when full

When `sets_remaining == 0`, rotate the current Active to InFlight and create a new Active (since there are no Free pools yet — that case lands in Task 5).

**Files:**
- Modify: `crates/yserver/src/kms/v2/descriptor_pool_ring.rs`

- [ ] **Step 1: Write the failing test**

Append to `mod tests`:

```rust
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn acquire_fills_active_then_rotates() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        // SETS_PER_POOL = 256. Acquire 257 sets at the same gen.
        for i in 0..257u64 {
            ring.acquire_set(layout, 1).unwrap_or_else(|e| {
                panic!("acquire #{i} failed: {e:?}");
            });
        }
        assert_eq!(ring.pool_count(), 2);
        // First pool rotated to InFlight, second is Active.
        assert_eq!(ring.state_counts(), (0, 1, 1, 0));
        assert_eq!(ring.lifetime_creates(), 2);
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }
```

- [ ] **Step 2: Run, confirm failure**

```
cargo test -p yserver --lib descriptor_pool_ring -- --ignored acquire_fills_active_then_rotates
```
Expected: FAIL — the current `acquire_set` either runs out of sets or returns an error after 256 acquires.

- [ ] **Step 3: Add rotation logic**

Replace the `acquire_set` body with the rotate-aware version. Modify only `acquire_set`:

```rust
    pub(crate) fn acquire_set(
        &mut self,
        layout: vk::DescriptorSetLayout,
        generation: u64,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        self.ensure_active_with_capacity()?;
        let idx = self.active.expect("ensure_active_with_capacity sets active");
        let pool = self.pools[idx].pool;
        let layouts = [layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        let sets = unsafe { self.vk.device.allocate_descriptor_sets(&alloc_info)? };
        let slot = &mut self.pools[idx];
        slot.sets_remaining = slot.sets_remaining.saturating_sub(1);
        if slot.high_water_generation < generation {
            slot.high_water_generation = generation;
        }
        Ok(sets[0])
    }

    /// Make sure there is an Active pool with at least one set
    /// remaining. Picks order:
    ///   1. current Active still has capacity → no-op.
    ///   2. Active is exhausted → rotate it to InFlight.
    ///   3. Pick the first Free pool → promote to Active.
    ///   4. Else create a new Active pool.
    fn ensure_active_with_capacity(&mut self) -> Result<(), vk::Result> {
        if let Some(idx) = self.active {
            if self.pools[idx].sets_remaining > 0 {
                return Ok(());
            }
            self.pools[idx].state = PoolState::InFlight;
            self.active = None;
        }
        for i in 0..self.pools.len() {
            if self.pools[i].state == PoolState::Free {
                self.pools[i].state = PoolState::Active;
                self.pools[i].sets_remaining = SETS_PER_POOL;
                self.pools[i].high_water_generation = 0;
                self.active = Some(i);
                return Ok(());
            }
        }
        self.create_pool_active()
    }
```

- [ ] **Step 4: Run, confirm pass**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --lib descriptor_pool_ring -- --ignored
```
Expected: 4 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/descriptor_pool_ring.rs
git commit -m "feat(v2/descriptor_pool_ring): rotate Active to InFlight on exhaustion

acquire_set now ensures an Active pool with capacity: exhausted
Active rotates to InFlight, Free is reused if present, else new
pool is created. Spec § 'Architecture'."
```

---

## Task 5: `release_up_to` watermark logic

Implement reset-on-watermark + the three watermark tests (move InFlight→Free, no-op below watermark, partial release across interleaved generations).

**Files:**
- Modify: `crates/yserver/src/kms/v2/descriptor_pool_ring.rs`

- [ ] **Step 1: Write the failing tests**

Append:

```rust
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn release_moves_inflight_to_free() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        // Fill pool A at gen=5, force rotate, then acquire gen=6 from
        // a fresh pool B so A is InFlight with high_water=5.
        for _ in 0..256 {
            ring.acquire_set(layout, 5).unwrap();
        }
        ring.acquire_set(layout, 6).unwrap();
        assert_eq!(ring.state_counts(), (0, 1, 1, 0));
        // release_up_to(5) reclaims pool A.
        let n = ring.release_up_to(5);
        assert_eq!(n, 1);
        assert_eq!(ring.state_counts(), (1, 1, 0, 0));
        assert_eq!(ring.lifetime_resets(), 1);
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn release_below_watermark_is_noop() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        for _ in 0..256 {
            ring.acquire_set(layout, 10).unwrap();
        }
        ring.acquire_set(layout, 11).unwrap();
        // release_up_to(9) — strictly below pool A's high_water=10.
        let n = ring.release_up_to(9);
        assert_eq!(n, 0);
        assert_eq!(ring.state_counts(), (0, 1, 1, 0));
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn interleaved_generations_partial_release() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        // Pool A: gen=100, fill.
        for _ in 0..256 {
            ring.acquire_set(layout, 100).unwrap();
        }
        // Pool B: gen=101, partial fill (1 set).
        ring.acquire_set(layout, 101).unwrap();
        assert_eq!(ring.state_counts(), (0, 1, 1, 0));
        // Release watermark=100 → pool A frees, pool B stays Active.
        assert_eq!(ring.release_up_to(100), 1);
        assert_eq!(ring.state_counts(), (1, 1, 0, 0));

        // Roll the cycle one more time: rotate B by filling it.
        for _ in 0..255 {
            ring.acquire_set(layout, 102).unwrap();
        }
        // B now has high_water=102 and is exhausted; one more acquire
        // rotates B and reuses the Free A.
        ring.acquire_set(layout, 103).unwrap();
        assert_eq!(ring.pool_count(), 2);
        assert_eq!(ring.state_counts(), (0, 1, 1, 0));
        // Release 101 → not enough; B's high_water=102 > 101.
        assert_eq!(ring.release_up_to(101), 0);
        // Release 102 → B reclaims to Free.
        assert_eq!(ring.release_up_to(102), 1);
        assert_eq!(ring.state_counts(), (1, 1, 0, 0));
        assert_eq!(ring.lifetime_resets(), 2);

        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }
```

- [ ] **Step 2: Run, confirm compile failure**

```
cargo test -p yserver --lib descriptor_pool_ring
```
Expected: compile error — `release_up_to` undefined.

- [ ] **Step 3: Implement `release_up_to`**

Add to `impl DescriptorPoolRing`, just after `acquire_set`:

```rust
    /// Caller signals "all submissions up to and including generation
    /// `retired_watermark` have retired." InFlight slots whose
    /// `high_water_generation <= retired_watermark` move to Free via
    /// `vkResetDescriptorPool`. Active pool is untouched.
    ///
    /// Returns the count of pools reclaimed for telemetry. On
    /// `vkResetDescriptorPool` error the slot is moved to Poisoned;
    /// the count of resets returned reflects only the `Ok` resets.
    pub(crate) fn release_up_to(&mut self, retired_watermark: u64) -> usize {
        let mut reclaimed = 0usize;
        for slot in &mut self.pools {
            if slot.state == PoolState::InFlight
                && slot.high_water_generation <= retired_watermark
            {
                match unsafe {
                    self.vk
                        .device
                        .reset_descriptor_pool(slot.pool, vk::DescriptorPoolResetFlags::empty())
                } {
                    Ok(()) => {
                        slot.state = PoolState::Free;
                        slot.sets_remaining = SETS_PER_POOL;
                        slot.high_water_generation = 0;
                        reclaimed += 1;
                    }
                    Err(e) => {
                        log::error!(
                            "DescriptorPoolRing: vkResetDescriptorPool failed on \
                             pool {:?}: {e:?} — poisoning slot",
                            slot.pool,
                        );
                        slot.state = PoolState::Poisoned;
                    }
                }
            }
        }
        self.lifetime_resets =
            self.lifetime_resets.saturating_add(reclaimed as u64);
        reclaimed
    }
```

- [ ] **Step 4: Run, confirm pass**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --lib descriptor_pool_ring -- --ignored
```
Expected: 7 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/descriptor_pool_ring.rs
git commit -m "feat(v2/descriptor_pool_ring): release_up_to with watermark recycle

InFlight slots whose high_water_generation <= retired_watermark
reset back to Free. Returns the count for telemetry. Reset errors
poison the slot (followup acquire short-circuit lands in next task).
Spec § 'Internals on release_up_to'."
```

---

## Task 6: OUT_OF_POOL_MEMORY + FRAGMENTED_POOL retry

Both errors share the same recovery: rotate Active to InFlight, pick / create a fresh Active, retry the alloc once. Spec § "Error handling" requires both to be tested individually because `FRAGMENTED_POOL` is a distinct driver code path.

The lavapipe ICD won't naturally produce these errors at a high rate, so we add a `#[cfg(test)]` injection knob: a single-shot "next allocate returns this error" override. Production Vk paths are not affected.

**Files:**
- Modify: `crates/yserver/src/kms/v2/descriptor_pool_ring.rs`

- [ ] **Step 1: Write the failing tests**

Append:

```rust
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn out_of_pool_memory_retry() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        ring.test_inject_next_allocate_error(vk::Result::ERROR_OUT_OF_POOL_MEMORY);
        let set = ring.acquire_set(layout, 1).expect("retry should succeed");
        assert_ne!(set, vk::DescriptorSet::null());
        // The first alloc consumed an Active pool then errored; the
        // retry rotates and creates pool 2.
        assert_eq!(ring.pool_count(), 2);
        assert_eq!(ring.state_counts(), (0, 1, 1, 0));
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }

    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn fragmented_pool_retry() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        ring.test_inject_next_allocate_error(vk::Result::ERROR_FRAGMENTED_POOL);
        let set = ring.acquire_set(layout, 1).expect("retry should succeed");
        assert_ne!(set, vk::DescriptorSet::null());
        assert_eq!(ring.pool_count(), 2);
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }
```

- [ ] **Step 2: Add the test injection field + helper**

In the `DescriptorPoolRing` struct, add (only inside `#[cfg(test)]`):

```rust
    #[cfg(test)]
    inject_next_allocate_error: Option<vk::Result>,
```

In `DescriptorPoolRing::new`, initialise the new field to `None` (gated `#[cfg(test)]`). Add a test-only setter:

```rust
    #[cfg(test)]
    pub(crate) fn test_inject_next_allocate_error(&mut self, err: vk::Result) {
        self.inject_next_allocate_error = Some(err);
    }
```

Concretely, the struct now looks like:

```rust
pub(crate) struct DescriptorPoolRing {
    vk: Arc<VkContext>,
    pools: Vec<PoolSlot>,
    active: Option<usize>,
    lifetime_creates: u64,
    lifetime_resets: u64,
    #[cfg(test)]
    inject_next_allocate_error: Option<vk::Result>,
}
```

and `new()`:

```rust
    pub(crate) fn new(vk: Arc<VkContext>) -> Self {
        Self {
            vk,
            pools: Vec::new(),
            active: None,
            lifetime_creates: 0,
            lifetime_resets: 0,
            #[cfg(test)]
            inject_next_allocate_error: None,
        }
    }
```

- [ ] **Step 3: Wire retry into `acquire_set`**

Replace the body of `acquire_set` with the retry-aware version:

```rust
    pub(crate) fn acquire_set(
        &mut self,
        layout: vk::DescriptorSetLayout,
        generation: u64,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        self.ensure_active_with_capacity()?;
        match self.try_allocate_one(layout) {
            Ok(set) => {
                self.note_acquired(generation);
                Ok(set)
            }
            Err(vk::Result::ERROR_OUT_OF_POOL_MEMORY | vk::Result::ERROR_FRAGMENTED_POOL) => {
                if let Some(idx) = self.active {
                    self.pools[idx].sets_remaining = 0;
                }
                self.ensure_active_with_capacity()?;
                let set = self.try_allocate_one(layout)?;
                self.note_acquired(generation);
                Ok(set)
            }
            Err(e) => Err(e),
        }
    }

    fn try_allocate_one(
        &mut self,
        layout: vk::DescriptorSetLayout,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        #[cfg(test)]
        if let Some(e) = self.inject_next_allocate_error.take() {
            return Err(e);
        }
        let idx = self.active.expect("ensure_active_with_capacity set active");
        let pool = self.pools[idx].pool;
        let layouts = [layout];
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool)
            .set_layouts(&layouts);
        let sets = unsafe { self.vk.device.allocate_descriptor_sets(&alloc_info)? };
        Ok(sets[0])
    }

    fn note_acquired(&mut self, generation: u64) {
        let idx = self.active.expect("acquire path established active");
        let slot = &mut self.pools[idx];
        slot.sets_remaining = slot.sets_remaining.saturating_sub(1);
        if slot.high_water_generation < generation {
            slot.high_water_generation = generation;
        }
    }
```

- [ ] **Step 4: Run, confirm pass**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --lib descriptor_pool_ring -- --ignored
```
Expected: 9 tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver/src/kms/v2/descriptor_pool_ring.rs
git commit -m "feat(v2/descriptor_pool_ring): OUT_OF_POOL_MEMORY + FRAGMENTED_POOL retry

acquire_set now mirrors BatchDescriptorArena's retry shape — both
exhaustion errors rotate-and-retry once. Adds a #[cfg(test)] knob
inject_next_allocate_error so the lavapipe-driven test suite can
exercise both distinct driver paths. Spec § 'Error handling'."
```

---

## Task 7: Reset failure poisons the ring; acquire short-circuits

`vkResetDescriptorPool` failure is a hard error — once observed, every subsequent `acquire_set` returns `ERROR_UNKNOWN` so callers drop the op the same way as a `vkCreateDescriptorPool` failure. Implementation: a linear scan at the top of `acquire_set` checks for any `Poisoned` slot.

**Files:**
- Modify: `crates/yserver/src/kms/v2/descriptor_pool_ring.rs`

- [ ] **Step 1: Write the failing test**

Append:

```rust
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn reset_failure_poisons_slot_and_drops_acquire() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        // Fill + rotate so pool A is InFlight at gen=1.
        for _ in 0..256 {
            ring.acquire_set(layout, 1).unwrap();
        }
        ring.acquire_set(layout, 2).unwrap();
        ring.test_force_next_reset_failure();
        let reclaimed = ring.release_up_to(1);
        // Slot poisoned, not reclaimed.
        assert_eq!(reclaimed, 0);
        let (free, active, in_flight, poisoned) = ring.state_counts();
        assert_eq!(poisoned, 1, "pool A must be poisoned");
        assert_eq!(free, 0);
        assert_eq!(active, 1, "pool B remains active");
        assert_eq!(in_flight, 0);
        // Next acquire short-circuits with ERROR_UNKNOWN.
        let err = ring.acquire_set(layout, 3).expect_err("poisoned ring");
        assert_eq!(err, vk::Result::ERROR_UNKNOWN);
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }
```

- [ ] **Step 2: Add the `#[cfg(test)]` flag**

In the struct (still gated):

```rust
    #[cfg(test)]
    force_next_reset_failure: bool,
```

In `new`:

```rust
            #[cfg(test)]
            force_next_reset_failure: false,
```

Add the helper:

```rust
    #[cfg(test)]
    pub(crate) fn test_force_next_reset_failure(&mut self) {
        self.force_next_reset_failure = true;
    }
```

- [ ] **Step 3: Branch the reset call on the test flag**

In `release_up_to`, replace the unsafe reset block with:

```rust
                let reset_result: Result<(), vk::Result> = {
                    #[cfg(test)]
                    if self.force_next_reset_failure {
                        self.force_next_reset_failure = false;
                        Err(vk::Result::ERROR_DEVICE_LOST)
                    } else {
                        unsafe {
                            self.vk.device.reset_descriptor_pool(
                                slot.pool,
                                vk::DescriptorPoolResetFlags::empty(),
                            )
                        }
                    }
                    #[cfg(not(test))]
                    unsafe {
                        self.vk.device.reset_descriptor_pool(
                            slot.pool,
                            vk::DescriptorPoolResetFlags::empty(),
                        )
                    }
                };
                match reset_result {
                    Ok(()) => {
                        slot.state = PoolState::Free;
                        slot.sets_remaining = SETS_PER_POOL;
                        slot.high_water_generation = 0;
                        reclaimed += 1;
                    }
                    Err(e) => {
                        log::error!(
                            "DescriptorPoolRing: vkResetDescriptorPool failed on \
                             pool {:?}: {e:?} — poisoning slot",
                            slot.pool,
                        );
                        slot.state = PoolState::Poisoned;
                    }
                }
```

Note: Rust's `cfg` evaluation requires a single value — the cleaner shape is to compute `reset_result` via a helper. If the inline shape above hits "unreachable expression" lints, replace with:

```rust
    fn perform_reset(&mut self, pool: vk::DescriptorPool) -> Result<(), vk::Result> {
        #[cfg(test)]
        if self.force_next_reset_failure {
            self.force_next_reset_failure = false;
            return Err(vk::Result::ERROR_DEVICE_LOST);
        }
        unsafe {
            self.vk
                .device
                .reset_descriptor_pool(pool, vk::DescriptorPoolResetFlags::empty())
        }
    }
```

…and call it from the loop. The borrow checker requires the `for slot in &mut self.pools` loop to split: rebuild as an index loop (`for i in 0..self.pools.len()`) so `&mut self.pools[i]` + `&mut self` coexist via temporary copies of the pool handle:

```rust
    pub(crate) fn release_up_to(&mut self, retired_watermark: u64) -> usize {
        let mut reclaimed = 0usize;
        for i in 0..self.pools.len() {
            let candidate = {
                let slot = &self.pools[i];
                slot.state == PoolState::InFlight
                    && slot.high_water_generation <= retired_watermark
            };
            if !candidate {
                continue;
            }
            let pool_handle = self.pools[i].pool;
            match self.perform_reset(pool_handle) {
                Ok(()) => {
                    let slot = &mut self.pools[i];
                    slot.state = PoolState::Free;
                    slot.sets_remaining = SETS_PER_POOL;
                    slot.high_water_generation = 0;
                    reclaimed += 1;
                }
                Err(e) => {
                    log::error!(
                        "DescriptorPoolRing: vkResetDescriptorPool failed on \
                         pool {pool_handle:?}: {e:?} — poisoning slot",
                    );
                    self.pools[i].state = PoolState::Poisoned;
                }
            }
        }
        self.lifetime_resets =
            self.lifetime_resets.saturating_add(reclaimed as u64);
        reclaimed
    }
```

- [ ] **Step 4: Add the short-circuit at the top of `acquire_set`**

Prepend to `acquire_set`:

```rust
        if self.pools.iter().any(|s| s.state == PoolState::Poisoned) {
            return Err(vk::Result::ERROR_UNKNOWN);
        }
```

- [ ] **Step 5: Run, confirm pass**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --lib descriptor_pool_ring -- --ignored
```
Expected: 10 tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver/src/kms/v2/descriptor_pool_ring.rs
git commit -m "feat(v2/descriptor_pool_ring): poison on reset failure; drop acquires

A failed vkResetDescriptorPool marks the slot Poisoned. Subsequent
acquire_set calls short-circuit with ERROR_UNKNOWN regardless of
sibling Free/Active capacity — the reset-failure signal indicates
the descriptor-pool subsystem is in an undefined state. Spec §
'Error handling'."
```

---

## Task 8: Warm-up bound (creates plateau + resets observed)

End-to-end ring stress test: 5000 acquire/release cycles with one set per cycle should plateau at `pool_count <= 2` AND `lifetime_resets` > 0 (proving the recycle path actually ran). This is the in-module load-bearing assertion that the design works without engine integration.

**Files:**
- Modify: `crates/yserver/src/kms/v2/descriptor_pool_ring.rs`

- [ ] **Step 1: Write the failing test**

Append:

```rust
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn pool_create_count_zero_after_warmup() {
        let Some(vk) = vk_or_skip() else { return };
        let layout = make_layout(&vk);
        let mut ring = DescriptorPoolRing::new(Arc::clone(&vk));
        const N: u64 = 5000;
        for gen in 1..=N {
            ring.acquire_set(layout, gen).expect("acquire");
            // Retire the gen we just acquired before the next iter,
            // mirroring the engine's "submit → retire" loop in the
            // single-in-flight steady state.
            ring.release_up_to(gen.saturating_sub(1));
        }
        // After warm-up: ≤ 2 pools resident. Lifetime creates should
        // be a small multiple of (N / SETS_PER_POOL = 19.5) but
        // bounded — anything ≤ 3 means the ring isn't growing on
        // every cycle.
        assert!(
            ring.pool_count() <= 2,
            "pool_count = {}, expected ≤ 2",
            ring.pool_count()
        );
        assert!(
            ring.lifetime_creates() <= 3,
            "lifetime_creates = {}; expected ≤ 3 after warm-up",
            ring.lifetime_creates()
        );
        assert!(
            ring.lifetime_resets() > 0,
            "lifetime_resets = 0; recycle path never ran"
        );
        unsafe { vk.device.destroy_descriptor_set_layout(layout, None) };
    }
```

- [ ] **Step 2: Run, expect PASS already**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --lib descriptor_pool_ring::tests::pool_create_count_zero_after_warmup -- --ignored
```
Expected: PASS. (No new impl code — the test is the gate.)

If it fails, the prior tasks did not implement the recycle correctly — debug before continuing.

- [ ] **Step 3: Commit**

```bash
git add crates/yserver/src/kms/v2/descriptor_pool_ring.rs
git commit -m "test(v2/descriptor_pool_ring): warm-up plateau is a small constant

End-to-end ring test asserting that 5000 acquire/release cycles
plateau at <= 2 pools, <= 3 lifetime creates, and lifetime_resets
> 0 — the three-assertion shape required by the spec (bounded
growth + observed recycle + steady-state residency)."
```

---

## Task 9: Pipeline cache — `allocate_descriptor_for_views_into_ring`

Add the ring-backed sibling of `RenderPipelineCache::allocate_descriptor_for_views_into`. Factor the descriptor-write block into a private helper that both methods call. The existing arena-backed method stays — v1 still depends on it.

**Files:**
- Modify: `crates/yserver/src/kms/vk/render_pipeline.rs`

- [ ] **Step 1: Sketch the failing assertion via cargo check**

The new method is only callable from the engine call sites (Task 11). For Task 9 we land it standalone with no in-module test (the integration test in Task 14/15 is the load-bearing assertion). The check-step is `cargo build -p yserver` plus a one-time compile check.

```
cargo build -p yserver
```
Expected: clean build at HEAD (sanity check before the change).

- [ ] **Step 2: Factor the write block into a shared helper**

In `crates/yserver/src/kms/vk/render_pipeline.rs`, between the existing `allocate_descriptor_for_views_into` and the `impl Drop`, add a private free function (file-scope, not on the impl):

```rust
/// Shared between `allocate_descriptor_for_views_into` and the ring
/// sibling. Takes a pre-allocated `vk::DescriptorSet` and writes the
/// three `COMBINED_IMAGE_SAMPLER` bindings (src=0, mask=1, dst=2)
/// with the shared linear sampler.
///
/// Note: parameter is named `vk_ctx`, NOT `vk`, because `vk` is the
/// `ash::vk` module alias in this file — using `vk` as a parameter
/// name shadows the module and breaks every `vk::DescriptorImageInfo`
/// reference inside the function body.
fn write_views_into_descriptor_set(
    vk_ctx: &VkContext,
    set: vk::DescriptorSet,
    sampler: vk::Sampler,
    src_view: vk::ImageView,
    mask_view: vk::ImageView,
    dst_view: vk::ImageView,
) {
    let src_info = [vk::DescriptorImageInfo::default()
        .image_view(src_view)
        .sampler(sampler)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let mask_info = [vk::DescriptorImageInfo::default()
        .image_view(mask_view)
        .sampler(sampler)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let dst_info = [vk::DescriptorImageInfo::default()
        .image_view(dst_view)
        .sampler(sampler)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let writes = [
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&src_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(1)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&mask_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(2)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&dst_info),
    ];
    unsafe { vk_ctx.device.update_descriptor_sets(&writes, &[]) };
}
```

- [ ] **Step 3: Switch the existing method to call the helper**

Replace the body of `allocate_descriptor_for_views_into` (keeping its signature unchanged for v1) with:

```rust
    pub fn allocate_descriptor_for_views_into(
        &self,
        arena: &mut crate::kms::scheduler::batch_descriptor_arena::BatchDescriptorArena,
        src_view: vk::ImageView,
        mask_view: vk::ImageView,
        dst_view: vk::ImageView,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        let set = arena.allocate_set(self.descriptor_set_layout)?;
        write_views_into_descriptor_set(
            self.vk.as_ref(),
            set,
            self.sampler,
            src_view,
            mask_view,
            dst_view,
        );
        Ok(set)
    }
```

- [ ] **Step 4: Add the ring-backed sibling**

Just below the arena-backed method:

```rust
    /// Stage 5 Task 4 layer 1: ring-backed sibling of
    /// `allocate_descriptor_for_views_into`. Allocates the descriptor
    /// set from the engine's long-lived `DescriptorPoolRing` (whose
    /// pools recycle on retirement) and writes the three view
    /// bindings via the shared `write_views_into_descriptor_set`
    /// helper. v2's engine call sites call this method; v1 keeps
    /// using the per-batch arena variant. Spec
    /// `2026-05-21-descriptor-pool-ring-design.md`.
    pub fn allocate_descriptor_for_views_into_ring(
        &self,
        ring: &mut crate::kms::v2::descriptor_pool_ring::DescriptorPoolRing,
        generation: u64,
        src_view: vk::ImageView,
        mask_view: vk::ImageView,
        dst_view: vk::ImageView,
    ) -> Result<vk::DescriptorSet, vk::Result> {
        let set = ring.acquire_set(self.descriptor_set_layout, generation)?;
        write_views_into_descriptor_set(
            self.vk.as_ref(),
            set,
            self.sampler,
            src_view,
            mask_view,
            dst_view,
        );
        Ok(set)
    }
```

- [ ] **Step 5: Check the build + clippy**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
cargo test -p yserver --lib
```
Expected: clean build, all existing tests pass, no clippy warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver/src/kms/vk/render_pipeline.rs
git commit -m "feat(vk/render_pipeline): allocate_descriptor_for_views_into_ring

Ring-backed sibling of allocate_descriptor_for_views_into for use by
v2 engine call sites. Both variants share a write_views_into_
descriptor_set helper so binding semantics (src=0, mask=1, dst=2,
shared linear sampler, SHADER_READ_ONLY_OPTIMAL layout) stay in one
place. v1's arena-backed path is untouched."
```

---

## Task 10: Engine fields — ring + `acquire_generation`

Add `descriptor_pool_ring: DescriptorPoolRing` and `acquire_generation: u64` to `EngineInner`, initialized in `RenderEngine::new`. No callers wired in this task — Tasks 11–12 do that. The unit test gate is "engine constructs and exposes the ring lifetime accessors".

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs`

- [ ] **Step 1: Write the failing test**

Append to the existing `#[cfg(test)] mod tests` block at the end of `engine.rs`:

```rust
    #[test]
    #[ignore = "needs live Vulkan ICD"]
    fn engine_exposes_descriptor_pool_ring_lifetime_counters() {
        let mut b = match super::super::backend::KmsBackendV2::for_tests_with_vk() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("skipping: no Vk: {e}");
                return;
            }
        };
        assert_eq!(b.engine.descriptor_pool_creates_lifetime(), 0);
        assert_eq!(b.engine.descriptor_pool_resets_lifetime(), 0);
    }
```

- [ ] **Step 2: Run, confirm failure**

```
cargo test -p yserver --lib engine::tests::engine_exposes_descriptor_pool_ring_lifetime_counters
```
Expected: compile error — methods undefined.

- [ ] **Step 3: Add the fields**

In `engine.rs`, find the `RenderEngineInner` struct (around line 367 — after `render_pipelines: Option<RenderPipelineCache>,`). Add a new field group at the bottom, after `logic_fill_caches`:

```rust
    /// Stage 5 Task 4 layer 1: long-lived descriptor pool ring used
    /// by `try_vk_render_composite` + `try_vk_render_traps_or_tris`.
    /// Replaces per-call `BatchDescriptorArena` instantiation. Spec
    /// `2026-05-21-descriptor-pool-ring-design.md`.
    descriptor_pool_ring: super::descriptor_pool_ring::DescriptorPoolRing,
    /// Stage 5 Task 4 layer 1: monotonic generation tag. Bumped on
    /// every paint-op submission; used as the watermark for ring
    /// pool recycling. The current value is passed to `acquire_set`
    /// and stamped onto the resulting `SubmittedOp` so the retirement
    /// loop can call `release_up_to(op.generation)`.
    acquire_generation: u64,
```

- [ ] **Step 4: Initialise in `RenderEngine::new`**

Find the `RenderEngine::new` constructor (around line 440). In the `Self { inner: Some(RenderEngineInner { ... }) }` literal, add:

```rust
                descriptor_pool_ring:
                    super::descriptor_pool_ring::DescriptorPoolRing::new(Arc::clone(&vk)),
                acquire_generation: 0,
```

`vk` is already cloned earlier in `new` — note that the existing literal assigns `vk` itself (moving the local), so the `Arc::clone(&vk)` must run before that move. Place these two lines BEFORE `vk,` in the field list, or pull the move out:

```rust
    pub(crate) fn new(platform: &PlatformBackend) -> Result<Self, RenderError> {
        let vk = platform.vk().ok_or(RenderError::NoVk)?.clone();
        let descriptor_pool_ring =
            super::descriptor_pool_ring::DescriptorPoolRing::new(Arc::clone(&vk));
        Ok(Self {
            inner: Some(RenderEngineInner {
                vk,
                submitted: VecDeque::new(),
                picture_paint: HashMap::new(),
                glyph_atlas: None,
                text_pipeline: None,
                atlas_last_upload_ticket: None,
                render_pipelines: None,
                solid_src_image: None,
                solid_mask_image: None,
                white_mask_image: None,
                dst_readback: None,
                src_alias_readback: None,
                trap_pipeline: None,
                mask_scratch: None,
                drawable_view_cache: HashMap::new(),
                logic_fill_caches: HashMap::new(),
                descriptor_pool_ring,
                acquire_generation: 0,
            }),
        })
    }
```

- [ ] **Step 5: Add the lifetime accessors on `RenderEngine`**

Just after `pending_count` (around line 541), add:

```rust
    /// Stage 5 Task 4 layer 1: lifetime count of `vkCreateDescriptorPool`
    /// calls inside the ring. Backend polls this and bumps telemetry.
    pub(crate) fn descriptor_pool_creates_lifetime(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.descriptor_pool_ring.lifetime_creates())
    }

    /// Stage 5 Task 4 layer 1: lifetime count of successful
    /// `vkResetDescriptorPool` calls inside the ring.
    pub(crate) fn descriptor_pool_resets_lifetime(&self) -> u64 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.descriptor_pool_ring.lifetime_resets())
    }
```

- [ ] **Step 6: Run, confirm pass**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --lib -- --ignored engine_exposes_descriptor_pool_ring_lifetime_counters
cargo test -p yserver --lib
```
Expected: new test passes; existing tests stay green; clippy clean.

- [ ] **Step 7: Commit**

```bash
git add crates/yserver/src/kms/v2/engine.rs
git commit -m "feat(v2/engine): host descriptor_pool_ring + acquire_generation

Adds the long-lived ring and a monotonic generation counter to
EngineInner. Lifetime accessors on RenderEngine expose ring
telemetry deltas to the backend. Wiring sites land in the next
commits. Spec § 'Engine integration'."
```

---

## Task 11: SubmittedOp — `generation` field, drop `descriptor_arena`

Add `generation: u64` to `SubmittedOp` and drop the `descriptor_arena: Option<BatchDescriptorArena>` field. Every `SubmittedOp { ... }` literal in `engine.rs` (13 sites at HEAD; see `grep -n 'SubmittedOp {'`) is updated to add `generation: inner.acquire_generation` after bumping the counter. The retirement loop calls `descriptor_pool_ring.release_up_to(op.generation)`. This is the largest mechanical change in the plan — work site-by-site, commit at the end.

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs`

- [ ] **Step 1: Update the struct definition**

At `crates/yserver/src/kms/v2/engine.rs:104`, the `SubmittedOp` struct currently ends with `descriptor_arena: Option<BatchDescriptorArena>,`. Replace that field with `generation`. After edit, the struct reads:

```rust
struct SubmittedOp {
    cb: vk::CommandBuffer,
    ticket: FenceTicket,
    /// Per-op staging buffer (only for `put_image` and Stage 3a
    /// glyph upload). Destroyed only after the fence signals;
    /// dropping it earlier would race the GPU's TRANSFER_READ.
    staging: Option<StagingBuffer>,
    /// Per-op scratch image (only for `copy_area` self-overlap
    /// path). Destroyed only after the fence signals.
    scratch: Option<ScratchImage>,
    /// Stage 3a: cloned `atlas_last_upload_ticket` snapshot.
    /// Atlas-sampling ops (text runs, RENDER glyphs in Stage 3d)
    /// stash the engine's then-current upload ticket here so the
    /// atlas image + the upload's staging buffer can't retire
    /// before the consume CB has executed. Same-queue submission
    /// order is the GPU dependency; this Arc keeps CPU-side
    /// destruction gated on retirement of both submissions.
    atlas_ticket: Option<FenceTicket>,
    /// Stage 5 Task 4 layer 1: monotonic acquire-generation stamp.
    /// `release_retired_ops` calls
    /// `descriptor_pool_ring.release_up_to(op.generation)` once this
    /// op pops from the FIFO; pools whose `high_water_generation
    /// <= op.generation` move back to Free. Spec
    /// `2026-05-21-descriptor-pool-ring-design.md`.
    generation: u64,
}
```

- [ ] **Step 2: Remove the legacy `BatchDescriptorArena` import + retire blocks**

In `crates/yserver/src/kms/v2/engine.rs` near the top of file (line 54), the import `scheduler::batch_descriptor_arena::BatchDescriptorArena,` is no longer referenced from inside the engine. Delete that line.

In `release_retired_ops` (around line 503), delete the block:

```rust
            // Stage 3c: release the per-op descriptor pool, if any.
            // `BatchDescriptorArena` owns its pools through the
            // `BatchResource::release` path; plain Drop leaks them.
            if let Some(arena) = op.descriptor_arena.take() {
                use crate::kms::scheduler::paint_batch::BatchResource;
                Box::new(arena).release(&inner.vk);
            }
```

Replace it with the ring-release call:

```rust
            // Stage 5 Task 4 layer 1: signal the descriptor pool
            // ring that everything up to and including this op's
            // generation has retired. Pools whose high_water_
            // generation <= op.generation transition InFlight → Free
            // via vkResetDescriptorPool.
            inner.descriptor_pool_ring.release_up_to(op.generation);
```

In `drain_all` (around line 532), delete the matching block:

```rust
            if let Some(arena) = op.descriptor_arena.take() {
                use crate::kms::scheduler::paint_batch::BatchResource;
                Box::new(arena).release(&inner.vk);
            }
```

…and replace with:

```rust
            inner.descriptor_pool_ring.release_up_to(op.generation);
```

- [ ] **Step 3: Walk every `SubmittedOp { ... }` literal**

Run `grep -n 'SubmittedOp {' crates/yserver/src/kms/v2/engine.rs` to list every literal. At HEAD this lists 13 sites. The pattern at every site is currently:

```rust
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: None,
            scratch: None,
            atlas_ticket: None,
            descriptor_arena: None,   // ← delete this line
        });
```

For **each** site:

1. Insert a generation-bump immediately before constructing `SubmittedOp` (if there isn't one already from Task 12's edits in Task 12's pre-pass). The shape is:
   ```rust
       inner.acquire_generation += 1;
       let generation = inner.acquire_generation;
       // ... existing CB record/submit/ticket code ...
       inner.submitted.push_back(SubmittedOp {
           cb,
           ticket,
           staging: ...,
           scratch: ...,
           atlas_ticket: ...,
           generation,
       });
   ```
   The bump must happen BEFORE any code that captures `inner.acquire_generation` (or before any other borrow that would conflict). For ops that don't use the ring (fill_rect, put_image, copy_area, image_text, composite_glyphs, logic_fill, copy_plane), the bump still happens — every op gets a generation; only the two ring-using sites pass it to `acquire_set`.
2. Replace `descriptor_arena: None,` with `generation,`.

Sites at HEAD (line numbers may drift as you edit; rerun the grep after each edit):

- `engine.rs:973` — `fill_rect` push.
- `engine.rs:1249` — `put_image` push.
- `engine.rs:1468` — `copy_area` non-overlap push.
- `engine.rs:1566` — `copy_area` overlap push.
- `engine.rs:1725` — `get_image` push.
- `engine.rs:1859` — `image_text` push.
- `engine.rs:2066` — `composite_glyphs` push.
- `engine.rs:2175` — `logic_fill` push.
- `engine.rs:2364` — `copy_plane` push (or whichever path; verify from context).
- `engine.rs:2503` — `render_fill_rectangles` push.
- `engine.rs:3086` — `render_composite` push (ring user — Task 12 also touches this site).
- `engine.rs:3734` — `render_traps_or_tris` push (ring user — Task 12 also touches this site).

Plus any pushes Task 12 adds — re-grep before committing.

- [ ] **Step 4: Compile, expect missing-field errors at any miss**

```
cargo build -p yserver
```
Expected: clean build. If any `SubmittedOp` literal is missing `generation`, the compiler points to it directly. Fix and rerun until clean.

- [ ] **Step 5: Run all lib tests**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
cargo test -p yserver --lib
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --lib -- --ignored
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --test v2_acceptance -- --ignored
```
Expected: all green. **Existing render_composite / render_traps tests continue to pass because Task 12 hasn't switched the call sites yet — they still call `BatchDescriptorArena::new` + `allocate_descriptor_for_views_into` and stash the arena... wait, the arena field is gone!**

⚠️ Order-of-operations matter here. There are two sequences that keep the tree green at every commit:

- **Option A**: Land Tasks 11 + 12 in a single combined commit (the SubmittedOp field swap *and* the call-site switch). Removes the intermediate "uncompilable" state. **Recommended.**
- **Option B**: First add `generation` as a sibling of `descriptor_arena` (don't remove the latter yet), commit; then in Task 12 switch the two call sites + remove `descriptor_arena` + remove the retire blocks, commit.

For the agent executing this plan: **use Option A**. The mechanical complexity of B (touching the same 13 sites twice) outweighs the small extra blast radius of a single combined commit. Combine Task 11 and Task 12 into one commit at the end of Task 12.

- [ ] **Step 6: Hold off on commit**

Do NOT commit yet. Task 12 lands the call-site switch and the combined diff commits together.

---

## Task 12: Switch the two engine call sites to the ring

Replace `BatchDescriptorArena::new(...)` + `allocate_descriptor_for_views_into(&mut arena, ...)` + `descriptor_arena: Some(arena)` with the ring path at `engine.rs:2867` (`try_vk_render_composite`) and `engine.rs:3413` (`try_vk_render_traps_or_tris`). This task completes the mechanical edit and lands the combined commit per Task 11 Step 5's note.

**Files:**
- Modify: `crates/yserver/src/kms/v2/engine.rs`

- [ ] **Step 1: Patch `try_vk_render_composite`**

At `crates/yserver/src/kms/v2/engine.rs:2867` (line may have drifted), the current block reads:

```rust
        // Per-op descriptor arena: pool lives until SubmittedOp
        // retires (CB holds descriptor set references).
        let mut arena = BatchDescriptorArena::new(Arc::clone(&inner.vk));
        let descriptor_set = inner
            .render_pipelines
            .as_ref()
            .expect("ensured")
            .allocate_descriptor_for_views_into(
                &mut arena,
                src_view,
                mask_view,
                dst_readback_view,
            )?;
```

Replace with:

```rust
        // Stage 5 Task 4 layer 1: bump the generation tag once per
        // RENDER op so the ring can recycle pools by retirement
        // watermark. `release_retired_ops` ➜ `release_up_to(
        // op.generation)` consumes the tag once the CB retires.
        inner.acquire_generation += 1;
        let generation = inner.acquire_generation;
        let descriptor_set = inner
            .render_pipelines
            .as_ref()
            .expect("ensured")
            .allocate_descriptor_for_views_into_ring(
                &mut inner.descriptor_pool_ring,
                generation,
                src_view,
                mask_view,
                dst_readback_view,
            )?;
```

Then find the `SubmittedOp` push at the bottom of `try_vk_render_composite` (the one currently around `engine.rs:3080`). Replace:

```rust
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: None,
            scratch: None,
            atlas_ticket: None,
            descriptor_arena: Some(arena),
        });
```

with:

```rust
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: None,
            scratch: None,
            atlas_ticket: None,
            generation,
        });
```

- [ ] **Step 2: Patch `try_vk_render_traps_or_tris`**

At `engine.rs:3413` the matching block reads:

```rust
        let mut arena = BatchDescriptorArena::new(Arc::clone(&inner.vk));
        let descriptor_set = inner
            .render_pipelines
            .as_ref()
            .expect("ensured")
            .allocate_descriptor_for_views_into(
                &mut arena,
                src_view,
                mask_view,
                dst_readback_view,
            )?;
```

Replace with:

```rust
        inner.acquire_generation += 1;
        let generation = inner.acquire_generation;
        let descriptor_set = inner
            .render_pipelines
            .as_ref()
            .expect("ensured")
            .allocate_descriptor_for_views_into_ring(
                &mut inner.descriptor_pool_ring,
                generation,
                src_view,
                mask_view,
                dst_readback_view,
            )?;
```

Locate the `SubmittedOp` push at the bottom of `try_vk_render_traps_or_tris` (currently around `engine.rs:3725`) and replace its body the same way as Step 1 — `generation,` instead of `descriptor_arena: Some(arena),`.

- [ ] **Step 3: Walk the OTHER 11 SubmittedOp sites and add generation bumps**

For each of the non-RENDER sites listed in Task 11 Step 3, replace the literal as described (bump generation, stamp the op). Use this minimal diff template per site:

```rust
        // ── before SubmittedOp push, after CB submit succeeds ───
        inner.acquire_generation += 1;
        let generation = inner.acquire_generation;
        inner.submitted.push_back(SubmittedOp {
            cb,
            ticket,
            staging: ...,      // existing value
            scratch: ...,      // existing value
            atlas_ticket: ..., // existing value
            generation,
        });
```

The `inner.acquire_generation += 1` bump must come AFTER any code that took an immutable borrow of `inner` and before the `push_back`. The cleanest placement is immediately preceding the push.

- [ ] **Step 4: Strip the dead `BatchDescriptorArena` use from engine**

Confirm there are no remaining references to `BatchDescriptorArena` inside `engine.rs`:

```
grep -n "BatchDescriptorArena\|descriptor_arena" crates/yserver/src/kms/v2/engine.rs
```
Expected: 0 results. If any survive, remove them.

- [ ] **Step 5: Confirm v1 is still using the arena**

```
grep -rn "BatchDescriptorArena" crates/yserver/src/kms/ | grep -v "src/kms/v2/"
```
Expected output (verbatim shape — the v2/engine.rs grep above produced 0 results):

```
crates/yserver/src/kms/scheduler/batch_descriptor_arena.rs:31:pub struct BatchDescriptorArena {
crates/yserver/src/kms/scheduler/batch_descriptor_arena.rs:41:impl std::fmt::Debug for BatchDescriptorArena {
crates/yserver/src/kms/scheduler/batch_descriptor_arena.rs:55:impl BatchDescriptorArena {
crates/yserver/src/kms/scheduler/batch_descriptor_arena.rs:56:    pub fn new(vk: Arc<VkContext>) -> Self {
crates/yserver/src/kms/scheduler/batch_descriptor_arena.rs:122:impl BatchDescriptorArena for BatchDescriptorArena {
crates/yserver/src/kms/scheduler/paint_batch.rs:???:    descriptor_arena: Option<BatchDescriptorArena>,
crates/yserver/src/kms/vk/render_pipeline.rs:???:    arena: &mut crate::kms::scheduler::batch_descriptor_arena::BatchDescriptorArena,
crates/yserver/src/kms/backend.rs:???:    let descriptor_set = render_cache.allocate_descriptor_for_views_into(
crates/yserver/src/kms/backend.rs:???:    let descriptor_set = render_cache.allocate_descriptor_for_views_into(
```

v1 (backend.rs) MUST still call `allocate_descriptor_for_views_into`. If v1 sites went missing, that's a regression — investigate.

- [ ] **Step 6: Run the full test matrix**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
cargo test -p yserver --lib
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --lib -- --ignored
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --test v2_acceptance -- --ignored
```
Expected: all green; clippy clean. The pre-existing v2 acceptance + engine Vk tests cover the call sites end-to-end and should pass on the ring with no test-side edits.

- [ ] **Step 7: Combined commit for Tasks 11+12**

```bash
git add crates/yserver/src/kms/v2/engine.rs
git commit -m "feat(v2/engine): switch RENDER paint sites to DescriptorPoolRing

try_vk_render_composite and try_vk_render_traps_or_tris now acquire
descriptor sets from the engine's long-lived DescriptorPoolRing
instead of instantiating a per-call BatchDescriptorArena. Every
SubmittedOp carries an acquire_generation stamp; release_retired_ops
and drain_all call ring.release_up_to(op.generation) so pools cycle
back to Free on retirement.

v1's KmsBackend continues to call allocate_descriptor_for_views_into
on the per-batch arena. Spec § 'Engine integration'."
```

---

## Task 13: Backend — sync ring telemetry deltas

Plumb the two ring lifetime counters into the existing telemetry bucket. The backend already wraps every engine RENDER call site (`render_composite`, `render_fill_rectangles`, `render_trapezoids`, `render_triangles_op`, etc.). Add a private helper that computes the delta since the last sync and bumps `Telemetry::record_descriptor_pool_create` / `record_descriptor_pool_reset` by the delta. Also call it after `engine.poll_retired` and `engine.drain_all` so reset bumps land.

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs`

- [ ] **Step 1: Write the failing test**

Drive `render_composite` once and assert the backend's telemetry reflects at least one pool create. Append to `crates/yserver/tests/v2_acceptance.rs`:

```rust
/// Stage 5 Task 4 layer 1 telemetry primer gate: after a single
/// render_composite call the backend telemetry must reflect ≥ 1
/// descriptor_pool_creates lifetime. Without backend wiring the
/// ring's lifetime counter increments but Telemetry stays at zero.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_render_composite_bumps_pool_create_telemetry() {
    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("create_pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 4, 4)
        .expect("pre-fill");
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("pic")
        .expect("Some");

    b.render_composite(
        None, 3, src_pic.as_raw(), 0, dst_pic.as_raw(),
        0, 0, 0, 0, 0, 0, 4, 4,
    )
    .expect("composite");

    let t = b.telemetry();
    assert!(
        t.lifetime.descriptor_pool_creates >= 1,
        "expected ≥ 1 pool create, got {}",
        t.lifetime.descriptor_pool_creates,
    );
}
```

- [ ] **Step 2: Run, confirm failure**

```
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --test v2_acceptance \
    v2_render_composite_bumps_pool_create_telemetry -- --ignored
```
Expected: FAIL — telemetry creates = 0 because no sync is wired yet.

- [ ] **Step 3: Add snapshot fields + helper to `KmsBackendV2`**

In `crates/yserver/src/kms/v2/backend.rs`, find the `KmsBackendV2` struct definition (around line 130). Add two `u64` fields under the existing `telemetry` field:

```rust
    /// Stage 5 Task 4 layer 1: last-observed ring lifetime values.
    /// `sync_descriptor_pool_telemetry` computes deltas vs these
    /// snapshots and bumps `telemetry` counters by the delta. Spec
    /// `2026-05-21-descriptor-pool-ring-design.md`.
    last_observed_pool_creates: u64,
    last_observed_pool_resets: u64,
```

Initialise both to `0` in every `KmsBackendV2` constructor — there are several (`open`, `for_tests`, `for_tests_with_vk`, `for_tests_seed`). Use `grep -n "telemetry: Telemetry" crates/yserver/src/kms/v2/backend.rs` to find the exact lines. Add the two `last_observed_*: 0,` next to the `telemetry:` initialiser at each site.

- [ ] **Step 4: Add the sync helper**

In `impl KmsBackendV2` (anywhere convenient — placing it near the existing `telemetry()` accessor at line 1370 makes it discoverable):

```rust
    /// Stage 5 Task 4 layer 1: pull ring lifetime counter deltas
    /// into Telemetry. Called by the backend after every engine
    /// RENDER call site + retirement sweep. The bumps are
    /// independent: ring.lifetime_creates increases inside
    /// acquire_set; ring.lifetime_resets increases inside
    /// release_up_to (which only runs inside engine.poll_retired
    /// and engine.drain_all). Spec
    /// `2026-05-21-descriptor-pool-ring-design.md` § 'Telemetry'.
    fn sync_descriptor_pool_telemetry(&mut self) {
        let creates_now = self.engine.descriptor_pool_creates_lifetime();
        let resets_now = self.engine.descriptor_pool_resets_lifetime();
        let creates_delta = creates_now.saturating_sub(self.last_observed_pool_creates);
        let resets_delta = resets_now.saturating_sub(self.last_observed_pool_resets);
        for _ in 0..creates_delta {
            self.telemetry.record_descriptor_pool_create();
        }
        if resets_delta > 0 {
            self.telemetry.record_descriptor_pool_reset(resets_delta);
        }
        self.last_observed_pool_creates = creates_now;
        self.last_observed_pool_resets = resets_now;
    }
```

- [ ] **Step 5: Call the helper after every engine call site that can touch the ring**

Placement rule: call `self.sync_descriptor_pool_telemetry();` **immediately after the engine call returns**, before any branch on the result. The helper is idempotent and cheap (one delta subtraction); over-calling is safe, but under-calling silently drops counters. Place the call before any early-return branches, not at the bottom of the function.

⚠️ **Don't introduce `?`.** None of these engine calls are currently propagated via `?` — the existing pattern binds the result to a local and then matches on it (because the backend method returns `io::Result<()>` while the engine returns `RenderError`, and the conversion is per-call-site `log::warn!` rather than `From`). Preserve that pattern; the only new line is the sync call after binding.

Sites to wire:

1. **`render_composite`** — `backend.rs:306` (`clear_window_area_with_background`), `:2251` (`try_tiled_fill`), `:5130` (`set_container_background_pixmap`), `:6810` (`Backend::render_composite` impl). The existing pattern at `:6810` is:

   ```rust
           let stats = self.engine.render_composite(
               /* existing args */
           );
           self.sync_descriptor_pool_telemetry();
           match &stats {
               Ok(s) => { /* existing telemetry + trace logging */ }
               Err(e) => { log::warn!("..."); }
           }
           Ok(())
   ```

   For `clear_window_area_with_background` (`backend.rs:306`), the existing code is `match self.engine.render_composite(...) { Ok(s) if s.recorded_draws > 0 => { ... return Ok(()); } ... }` — that's an early return in the success arm. Bind the result to a local before the match so the sync call lands before the early return:

   ```rust
           let composite_result = self.engine.render_composite(/* ... */);
           self.sync_descriptor_pool_telemetry();
           match composite_result {
               Ok(s) if s.recorded_draws > 0 => {
                   self.telemetry.record_paint_submit();
                   return Ok(());
               }
               /* existing arms */
           }
   ```

   Apply the same "bind then sync before match" pattern at `:2251` and `:5130` if they also use the match-with-early-return shape (verify with `grep -B2 -A20 'self.engine.render_composite' crates/yserver/src/kms/v2/backend.rs`).

2. **`render_fill_rectangles`** — locate every site:

   ```
   grep -n "self.engine.render_fill_rectangles" crates/yserver/src/kms/v2/backend.rs
   ```

   Apply the same "bind, sync, then match" placement.

3. **`render_trapezoids` / `render_triangles_op`** — same. The plan does not need to enumerate every fill_rect/put_image/copy_area site since those don't touch the ring; only the RENDER ops do. But because `acquire_generation` is bumped on EVERY paint op (per Task 11), any op that bumps the counter doesn't itself fire a pool create — only the two ring-using sites can.

4. **`engine.poll_retired`** at `backend.rs:3691` — resets fire here. After the call:

   ```rust
           self.engine.poll_retired(&self.platform);
           self.store.poll_pending_retire(&mut self.platform);
           self.sync_descriptor_pool_telemetry();
   ```

5. **`engine.drain_all`** at `backend.rs:1417` — shutdown sweep:

   ```rust
           self.engine.drain_all(&self.platform);
           self.sync_descriptor_pool_telemetry();
           self.scene.drain_all(&mut self.platform);
   ```

If you over-call the helper (e.g. you put one in `set_container_background_pixmap` even though that path may not always touch the ring), the cost is a saturating subtraction returning zero — no spurious telemetry. Under-calling means resets/creates "appear" on the next call site that does run the helper; counts are still correct in aggregate, just with delay. Aim for "called after every site that touches the ring at all" and don't sweat overlap.

- [ ] **Step 6: Run, confirm pass**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
cargo test -p yserver --lib
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --test v2_acceptance v2_render_composite_bumps_pool_create_telemetry -- --ignored
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --test v2_acceptance -- --ignored
```
Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add crates/yserver/src/kms/v2/backend.rs crates/yserver/tests/v2_acceptance.rs
git commit -m "feat(v2/backend): sync descriptor pool ring telemetry

After every RENDER engine call site and retirement sweep the backend
pulls ring lifetime counter deltas into Telemetry's per-second +
lifetime descriptor_pool_creates / descriptor_pool_resets buckets.
The acceptance gate confirms the wiring fires at least once for a
single render_composite call."
```

---

## Task 14: Acceptance — Composite call-site three-assertion shape

Drive 2000 `render_composite` ops with bounded in-flight depth (poll retirement between batches). Assert three invariants per spec § "Testing" / "Integration tests":

1. `lifetime.descriptor_pool_creates <= ceil(N / SETS_PER_POOL) + slack`
2. `lifetime.descriptor_pool_resets >= N / SETS_PER_POOL - slack`
3. `pool_count() <= 4`

All three together rule out the degenerate-but-passing variants of the implementation (never-resetting, always-leaking, never-creating).

⚠️ **Retirement caveat.** `engine.poll_retired` is only called from `on_page_flip_ready` in production; pixmap-only test fixtures never drive a page flip, so retirement never runs and the ring never resets. The acceptance test needs an explicit retirement hook. We add a `#[doc(hidden)] pub fn for_tests_poll_retired` accessor on `KmsBackendV2` that wraps `engine.poll_retired` + `sync_descriptor_pool_telemetry`. This mirrors the existing `get_image_pixels_for_tests` and `descriptor_pool_ring_pool_count` accessors.

**Files:**
- Modify: `crates/yserver/src/kms/v2/backend.rs` — add `descriptor_pool_ring_pool_count` and `for_tests_poll_retired` accessors.
- Modify: `crates/yserver/src/kms/v2/engine.rs` — add `descriptor_pool_ring_pool_count` accessor.
- Modify: `crates/yserver/tests/v2_acceptance.rs` — the new test.

- [ ] **Step 1: Add ring residency + retirement accessors**

In `crates/yserver/src/kms/v2/engine.rs`, next to the lifetime accessors added in Task 10:

```rust
    /// Stage 5 Task 4 layer 1: ring residency for the acceptance
    /// gate (`v2_render_composite_pool_creates_bounded_after_warmup`).
    pub(crate) fn descriptor_pool_ring_pool_count(&self) -> usize {
        self.inner.as_ref().map_or(0, |i| i.descriptor_pool_ring.pool_count())
    }
```

In `crates/yserver/src/kms/v2/backend.rs`, near the existing `telemetry()` accessor (around line 1370):

```rust
    /// Stage 5 Task 4 layer 1: test-side accessor to the ring's
    /// pool residency. Used by the acceptance harness to assert
    /// steady-state pool count stays small after warm-up.
    #[doc(hidden)]
    pub fn descriptor_pool_ring_pool_count(&self) -> usize {
        self.engine.descriptor_pool_ring_pool_count()
    }

    /// Stage 5 Task 4 layer 1: test-side retirement driver. In
    /// production, retirement runs from `on_page_flip_ready` and
    /// invokes `engine.poll_retired` + `store.poll_pending_retire`
    /// (`backend.rs:3691-3692`). Pixmap-only test fixtures never
    /// drive a page flip, so the ring's recycle path can't run
    /// without this hook. The body mirrors the production sequence
    /// 1:1 so the acceptance harness exercises the same code paths
    /// any future store-retirement work would touch — and adds the
    /// telemetry sync call so ring delta counters land in
    /// `self.telemetry`.
    #[doc(hidden)]
    pub fn for_tests_poll_retired(&mut self) {
        self.engine.poll_retired(&self.platform);
        self.store.poll_pending_retire(&mut self.platform);
        self.sync_descriptor_pool_telemetry();
    }
```

- [ ] **Step 2: Write the failing test**

Append to `crates/yserver/tests/v2_acceptance.rs`:

```rust
/// Stage 5 Task 4 layer 1 acceptance: N render_composite ops with
/// bounded in-flight depth must (1) bound pool creates, (2) actually
/// recycle pools (resets observed), (3) keep pool residency small.
/// Spec § 'Integration tests'.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_render_composite_pool_creates_bounded_after_warmup() {
    const N: u32 = 2000;
    // 256 sets per pool inside the ring (mirrors SETS_PER_POOL).
    const SETS_PER_POOL: u32 = 256;
    const WARMUP_SLACK: u64 = 4;
    let expected_creates_upper = u64::from(N / SETS_PER_POOL) + WARMUP_SLACK;
    let expected_resets_lower = u64::from(N / SETS_PER_POOL).saturating_sub(WARMUP_SLACK);

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };

    let dst_pix = b.create_pixmap(None, 32, 4, 4).expect("dst pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 4, 4)
        .expect("pre-fill blue");
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid red")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst pic")
        .expect("Some");

    for i in 0..N {
        b.render_composite(
            None, 3, src_pic.as_raw(), 0, dst_pic.as_raw(),
            0, 0, 0, 0, 0, 0, 4, 4,
        )
        .unwrap_or_else(|e| panic!("composite #{i} failed: {e:?}"));
        // Retire often — every 32 ops drives the ring through full
        // recycle cycles. Without retirement the ring just grows
        // InFlight pools and never resets.
        if i % 32 == 31 {
            // Force fence completion via a sync get_image, then
            // drive the retirement loop explicitly (page flips don't
            // run in the pixmap-only fixture).
            let _ = b
                .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 4, 4, !0)
                .expect("get_image");
            b.for_tests_poll_retired();
        }
    }
    // Final retirement to flush any remaining in-flight ops.
    let _ = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 4, 4, !0)
        .expect("final get_image");
    b.for_tests_poll_retired();

    let t = b.telemetry();
    let creates = t.lifetime.descriptor_pool_creates;
    let resets = t.lifetime.descriptor_pool_resets;
    let residency = b.descriptor_pool_ring_pool_count();

    assert!(
        creates <= expected_creates_upper,
        "creates={creates}, expected <= {expected_creates_upper} (N={N})",
    );
    assert!(
        resets >= expected_resets_lower,
        "resets={resets}, expected >= {expected_resets_lower} \
         — recycle path didn't run; pools may be leaking as InFlight",
    );
    assert!(
        residency <= 4,
        "pool_count={residency} after warm-up; expected <= 4",
    );
}
```

- [ ] **Step 3: Run, expect PASS**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --test v2_acceptance v2_render_composite_pool_creates_bounded_after_warmup -- --ignored
```
Expected: PASS — the ring + retirement plumbing from earlier tasks already implements the recycle.

If the assertion on `resets` fails, it means `get_image` isn't reaching the backend's retirement-sync site. Investigate before continuing — likely a missing `sync_descriptor_pool_telemetry()` call after `engine.poll_retired`.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/src/kms/v2/backend.rs \
        crates/yserver/src/kms/v2/engine.rs \
        crates/yserver/tests/v2_acceptance.rs
git commit -m "test(v2-acceptance): render_composite three-assertion pool ring gate

2000-op acceptance test asserts (1) bounded lifetime creates, (2)
observed lifetime resets, (3) bounded steady-state pool residency.
Per spec § 'Integration tests', all three are required — any single
assertion alone admits a degenerate-but-passing implementation."
```

---

## Task 15: Acceptance — Trapezoid call-site three-assertion shape

Same three-assertion shape, driven through `render_trapezoids` so the second engine call site (`try_vk_render_traps_or_tris` at `engine.rs:3413`) is exercised directly. The shape is identical to Task 14; only the X11 request differs.

**Files:**
- Modify: `crates/yserver/tests/v2_acceptance.rs`

- [ ] **Step 1: Write the failing test**

Append to `crates/yserver/tests/v2_acceptance.rs`:

```rust
/// Stage 5 Task 4 layer 1 acceptance for the traps call site. Same
/// three-assertion shape as render_composite — landing both makes
/// the regression surface explicit since the two engine paths share
/// the ring acquire helper.
#[test]
#[ignore = "needs live Vulkan ICD"]
fn v2_render_traps_pool_creates_bounded_after_warmup() {
    const N: u32 = 2000;
    const SETS_PER_POOL: u32 = 256;
    const WARMUP_SLACK: u64 = 4;
    let expected_creates_upper = u64::from(N / SETS_PER_POOL) + WARMUP_SLACK;
    let expected_resets_lower = u64::from(N / SETS_PER_POOL).saturating_sub(WARMUP_SLACK);

    let mut b = match KmsBackendV2::for_tests_with_vk() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping: no Vk: {e}");
            return;
        }
    };
    let dst_pix = b.create_pixmap(None, 32, 8, 8).expect("dst pixmap");
    let dst_xid = dst_pix.as_raw();
    b.fill_rectangle(None, dst_xid, 0xFF0000FF, 0, 0, 8, 8)
        .expect("pre-fill blue");
    let src_pic = b
        .render_create_solid_fill(None, [0xFF, 0xFF, 0, 0, 0, 0, 0xFF, 0xFF])
        .expect("solid red")
        .expect("Some");
    let dst_pic = b
        .render_create_picture(None, AnyHandle::Pixmap(dst_pix), 0, 0, &[])
        .expect("dst pic")
        .expect("Some");

    // Same axis-aligned 4×4 trap used by
    // v2_render_trapezoids_renders_filled_rect.
    let mut traps: Vec<u8> = Vec::with_capacity(40);
    let fields: [i32; 10] = [
        2 << 16, 6 << 16, 2 << 16, 2 << 16, 2 << 16,
        6 << 16, 6 << 16, 2 << 16, 6 << 16, 6 << 16,
    ];
    for v in fields {
        traps.extend_from_slice(&v.to_le_bytes());
    }

    for i in 0..N {
        b.render_trapezoids(
            None, 3, src_pic.as_raw(), dst_pic.as_raw(),
            0, 0, 0, &traps, 0, 0,
        )
        .unwrap_or_else(|e| panic!("trap #{i} failed: {e:?}"));
        if i % 32 == 31 {
            let _ = b
                .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
                .expect("get_image");
            b.for_tests_poll_retired();
        }
    }
    let _ = b
        .get_image_pixels_for_tests(dst_xid, 2, 0, 0, 8, 8, !0)
        .expect("final get_image");
    b.for_tests_poll_retired();

    let t = b.telemetry();
    let creates = t.lifetime.descriptor_pool_creates;
    let resets = t.lifetime.descriptor_pool_resets;
    let residency = b.descriptor_pool_ring_pool_count();

    assert!(
        creates <= expected_creates_upper,
        "creates={creates}, expected <= {expected_creates_upper}",
    );
    assert!(
        resets >= expected_resets_lower,
        "resets={resets}, expected >= {expected_resets_lower}",
    );
    assert!(
        residency <= 4,
        "pool_count={residency} after warm-up; expected <= 4",
    );
}
```

- [ ] **Step 2: Run, expect PASS**

```
cargo +nightly fmt
cargo clippy -p yserver --tests -- -D warnings
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --test v2_acceptance v2_render_traps_pool_creates_bounded_after_warmup -- --ignored
```
Expected: PASS.

- [ ] **Step 3: Run the full v2 acceptance suite to confirm no regression**

```
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json \
  cargo test -p yserver --test v2_acceptance -- --ignored
```
Expected: every existing v2 acceptance test passes + the two new pool-ring gates pass.

- [ ] **Step 4: Commit**

```bash
git add crates/yserver/tests/v2_acceptance.rs
git commit -m "test(v2-acceptance): render_trapezoids three-assertion pool ring gate

Mirrors v2_render_composite_pool_creates_bounded_after_warmup at
the try_vk_render_traps_or_tris call site. Both engine call sites
that acquire from the ring now have direct acceptance coverage."
```

---

## Task 16: Hardware smoke + status doc update

After the in-tree gates pass, run the hardware capture recipes from spec § "Capture recipe (post-fix verification)" to confirm the perf delta is real. Update `docs/status.md` to log Task 4 layer 1 closure under Stage 5.

**Files:**
- Modify: `docs/status.md`

- [ ] **Step 1: Run the telemetry capture recipe**

Per spec § "Capture recipe":

```
just yserver-mate-hw-telemetry
```

Expected in the resulting `v2_telemetry:` log lines (one per second under load):

- `descriptor_pool_creates/s` ≤ 5 in steady state.
- `descriptor_pool_resets/s` tens-per-second under sustained paint.
- `descriptor_allocations/s` unchanged vs the 2026-05-21 baseline (~180/s under the same workload).
- `paint_submits/s` unchanged.

Capture the resulting log to `/tmp/v2_telemetry_post_pool_ring.log` for the status update.

- [ ] **Step 2: Run the perf capture recipe**

```
just yserver-mate-hw-perf
```

Expected in the resulting flamegraph (after the same yoga workload):

- `create_descriptor_pool` → `msm_ioctl_vm_bind` ≤ 0.1% of total CPU (was ~1.63% on 2026-05-21).
- `handle_render_request` total drops proportionally (was ~3.32%).

Save the flamegraph as `docs/captures/2026-05-21-v2-perf-post-pool-ring.svg` (or matching path under the captures directory — check `ls docs/captures/` for the convention).

- [ ] **Step 3: Update `docs/status.md`**

Add a Task 4 layer 1 entry under the Stage 5 section (search for `### Done (continued)` and the `[ ] **Stage 5` line at approx. `status.md:2016`). The exact section heading depends on whether Stage 5 has any sub-task entries yet — at HEAD the Stage 5 entry is a single bullet. Insert as a nested sub-task under it:

```markdown
  - [x] **Task 4 layer 1 — DescriptorPoolRing.** Landed
    2026-05-21. Spec: `2026-05-21-descriptor-pool-ring-design.md`;
    plan: `2026-05-21-descriptor-pool-ring.md`. Per-call
    `BatchDescriptorArena` instantiation in `try_vk_render_composite`
    + `try_vk_render_traps_or_tris` replaced with a long-lived
    `DescriptorPoolRing` on `EngineInner` cycling Free/Active/
    InFlight pools by `acquire_generation` watermark.
    `vkResetDescriptorPool` failure poisons the ring (hard-error
    propagation). v1's `BatchDescriptorArena` stays in tree —
    `paint_batch.descriptor_arena_mut()` still drives it.

    **Post-fix capture** (yoga / Snapdragon X1 / Turnip, MATE + marco
    + wezterm + caja, same workload as the 2026-05-21 baseline):
      - `descriptor_pool_creates/s` ≤ 5 in steady state (was implicit
        ~4700 via `vkCreateDescriptorPool` on every RENDER call).
      - `descriptor_pool_resets/s` tens-per-second tracking
        `paint_submits/s / SETS_PER_POOL`.
      - `descriptor_allocations/s` unchanged at ~180/s.
      - perf flamegraph: `create_descriptor_pool` →
        `msm_ioctl_vm_bind` drops from ~1.63% to ≤ 0.1% of total CPU.

    Tests: 10 ring unit tests (`crates/yserver/src/kms/v2/
    descriptor_pool_ring.rs`); 2 acceptance tests with three-assertion
    shape — bounded creates, observed resets, bounded residency —
    one per engine call site
    (`v2_render_composite_pool_creates_bounded_after_warmup` +
    `v2_render_traps_pool_creates_bounded_after_warmup`).
```

If hardware capture was not possible (no access to the bee/fuji/yoga rigs from the executing environment), record the lavapipe-only acceptance result instead — the unit tests + acceptance harness are the in-tree gate; the hardware capture is the validation gate the user runs out-of-band.

- [ ] **Step 4: Commit**

```bash
git add docs/status.md
# also: any captured artefacts under docs/captures/, if you saved one
git commit -m "docs(status): close Stage 5 Task 4 layer 1 (DescriptorPoolRing)

Per-call BatchDescriptorArena instantiation eliminated from the v2
RENDER paint path. Hardware capture confirms steady-state
vkCreateDescriptorPool/s drops from implicit ~4700 to ≤ 5; perf
flamegraph shows create_descriptor_pool → msm_ioctl_vm_bind path
collapsed from ~1.63% to ≤ 0.1% of total CPU."
```

---

## Done — verification checklist

- [ ] `cargo +nightly fmt` clean.
- [ ] `cargo clippy -p yserver --tests -- -D warnings` clean (no `clippy::pedantic` per AGENTS.md).
- [ ] `cargo test -p yserver --lib` green.
- [ ] `cargo test -p yserver --lib -- --ignored` (Vk-gated) green under lavapipe — includes 10 ring tests.
- [ ] `cargo test -p yserver --test v2_acceptance -- --ignored` green under lavapipe — includes the 2 new pool-ring acceptance tests + the 1 telemetry primer gate.
- [ ] `grep -n "BatchDescriptorArena\|descriptor_arena" crates/yserver/src/kms/v2/engine.rs` returns 0 lines.
- [ ] `grep -rn "BatchDescriptorArena" crates/yserver/src/kms/` still finds v1's site (backend.rs:5232 + :6272, render_pipeline.rs's signature, paint_batch.rs's field, the type definition itself).
- [ ] Hardware telemetry capture saved (or hardware-unavailable explicitly noted in the status entry).
- [ ] `docs/status.md` updated.
