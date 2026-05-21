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
}
