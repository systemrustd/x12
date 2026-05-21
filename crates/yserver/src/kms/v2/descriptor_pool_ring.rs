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

#![allow(dead_code)] // Skeleton; fields + constants wired in Tasks 3-5.

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
        self.ensure_active_with_capacity()?;
        let idx = self
            .active
            .expect("ensure_active_with_capacity sets active");
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
            Ok(vk) => Some(vk),
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
}
