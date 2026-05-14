//! Backend-owned pool of recycled `VkImage` + `VkImageView` +
//! `VkDeviceMemory` triples for server-owned X pixmaps.
//!
//! Motivation: adapta-nokto theme apply + mate-cc launcher fire
//! hundreds of `CreatePixmap`/`FreePixmap` cycles per second for
//! 16×16 / 32×32 widget pixmaps. The kernel allocator (amdgpu /
//! i915) serializes under that burst rate. This pool recycles the
//! Vulkan allocations so a fresh `CreatePixmap` of a recently-freed
//! `(extent, format)` hits the pool instead of round-tripping the
//! kernel.
//!
//! Keyed by `(width, height, format)`. `usage` is the constant
//! `COLOR_ATTACHMENT | TRANSFER_DST | TRANSFER_SRC | SAMPLED`
//! across all server-owned pixmaps, so it's not part of the key.
//!
//! Per-bucket cap (`PIXMAP_POOL_BUCKET_CAP`). Max pooled dimension
//! (`MAX_POOLED_DIM`) — pixmaps above this skip the pool (both on
//! return and on take) since they exhibit much lower reuse rates
//! and have quadratically larger backing memory.
//!
//! Lifetime: pool entries are returned via a `BatchResource`
//! adopted into the currently-open paint batch (Phase 5 T2
//! defer-release mechanism). When the batch retires, the
//! BatchResource's `release` returns the entry to the pool if the
//! bucket has room, else destroys it directly.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, Weak},
};

use ash::vk;

use crate::kms::{scheduler::paint_batch::BatchResource, vk::device::VkContext};

/// Per-bucket cap. 32 BGRA8 32×32 images is ~128 KB per bucket;
/// across ~10 active buckets the worst-case pool memory is ~1.5 MB.
pub const PIXMAP_POOL_BUCKET_CAP: usize = 32;

/// Pixmaps with `width > MAX_POOLED_DIM || height > MAX_POOLED_DIM`
/// skip the pool. Above this size reuse rates drop and per-entry
/// memory grows quadratically.
pub const MAX_POOLED_DIM: u32 = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PixmapPoolKey {
    pub width: u32,
    pub height: u32,
    pub format: vk::Format,
}

/// One recycled pixmap-backing triple.
#[derive(Debug)]
pub struct PooledPixmapImage {
    pub image: vk::Image,
    pub view: vk::ImageView,
    pub memory: vk::DeviceMemory,
    pub current_layout: vk::ImageLayout,
}

/// Pool statistics for synthetic tests + telemetry. Reset on
/// backend shutdown.
#[derive(Debug, Default, Clone, Copy)]
pub struct PixmapPoolStats {
    pub total_takes_hit: u64,
    pub total_takes_miss: u64,
    pub total_returns_accepted: u64,
    pub total_returns_rejected_bucket_full: u64,
    pub total_returns_rejected_oversize: u64,
}

/// Telemetry-side handle to the latest constructed pool. Set by
/// `PixmapPool::new`; read by the telemetry thread in
/// `yserver::run` to log per-second deltas. `Weak` so the pool can
/// still drop cleanly on backend teardown.
pub static GLOBAL_LATEST_POOL: Mutex<Weak<PixmapPool>> = Mutex::new(Weak::new());

/// Capture-the-most-recent-pool hook. Called by `PixmapPool::new`
/// via an `Arc::new_cyclic`-style indirection — but since the pool
/// is constructed via plain `Arc::new(PixmapPool::new(..))` we
/// expose a helper the construction site uses immediately after.
pub fn register_for_telemetry(pool: &Arc<PixmapPool>) {
    if let Ok(mut g) = GLOBAL_LATEST_POOL.lock() {
        *g = Arc::downgrade(pool);
    }
}

/// Telemetry-side snapshot accessor. Returns `None` if no pool has
/// been registered, or the registered pool has been dropped.
#[must_use]
pub fn telemetry_snapshot() -> Option<PixmapPoolStats> {
    let weak = GLOBAL_LATEST_POOL.lock().ok()?.clone();
    weak.upgrade().map(|p| p.stats())
}

pub struct PixmapPool {
    vk: Arc<VkContext>,
    // Mutex (not RefCell) so PooledPixmapReturn's Arc<PixmapPool>
    // satisfies BatchResource's Send bound. Single-threaded core
    // loop means contention is zero; Mutex is the cheapest Send-safe
    // option (one atomic CAS per pool op).
    buckets: Mutex<HashMap<PixmapPoolKey, VecDeque<PooledPixmapImage>>>,
    stats: Mutex<PixmapPoolStats>,
}

impl std::fmt::Debug for PixmapPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // VkContext does not implement Debug; show bucket count +
        // stats so logs are still useful without trying to print
        // raw Vulkan handles.
        let buckets_len = self.buckets.lock().map(|b| b.len()).unwrap_or(usize::MAX);
        f.debug_struct("PixmapPool")
            .field("buckets", &buckets_len)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl PixmapPool {
    pub fn new(vk: Arc<VkContext>) -> Self {
        Self {
            vk,
            buckets: Mutex::new(HashMap::new()),
            stats: Mutex::new(PixmapPoolStats::default()),
        }
    }

    /// True if the pool would accept an entry for `key`. Used by
    /// callers to skip building a `PooledPixmapReturn` for sizes
    /// the pool won't accept anyway.
    #[must_use]
    pub fn eligible(key: PixmapPoolKey) -> bool {
        key.width <= MAX_POOLED_DIM && key.height <= MAX_POOLED_DIM
    }

    /// Take a recycled entry for `key`, or `None` if the bucket is
    /// empty.
    pub fn try_take(&self, key: PixmapPoolKey) -> Option<PooledPixmapImage> {
        if !Self::eligible(key) {
            return None;
        }
        let mut buckets = self
            .buckets
            .lock()
            .expect("pixmap pool buckets mutex poisoned");
        let mut stats = self.stats.lock().expect("pixmap pool stats mutex poisoned");
        let entry = buckets.get_mut(&key).and_then(VecDeque::pop_front);
        if entry.is_some() {
            stats.total_takes_hit += 1;
        } else {
            stats.total_takes_miss += 1;
        }
        entry
    }

    /// Try to return `entry` to the pool. Returns `Ok(())` if
    /// accepted; `Err(entry)` if the bucket was full or the key is
    /// ineligible — caller must destroy the entry.
    pub fn try_return(
        &self,
        key: PixmapPoolKey,
        entry: PooledPixmapImage,
    ) -> Result<(), PooledPixmapImage> {
        if !Self::eligible(key) {
            self.stats
                .lock()
                .expect("pixmap pool stats mutex poisoned")
                .total_returns_rejected_oversize += 1;
            return Err(entry);
        }
        let mut buckets = self
            .buckets
            .lock()
            .expect("pixmap pool buckets mutex poisoned");
        let bucket = buckets.entry(key).or_default();
        if bucket.len() >= PIXMAP_POOL_BUCKET_CAP {
            self.stats
                .lock()
                .expect("pixmap pool stats mutex poisoned")
                .total_returns_rejected_bucket_full += 1;
            return Err(entry);
        }
        bucket.push_back(entry);
        self.stats
            .lock()
            .expect("pixmap pool stats mutex poisoned")
            .total_returns_accepted += 1;
        Ok(())
    }

    /// Synchronously destroy every pooled entry. Called on backend
    /// shutdown after the scheduler has drained its in-flight
    /// batches (so no `BatchResource` can still hold a back-ref).
    pub fn drain(&self) {
        let mut buckets = self
            .buckets
            .lock()
            .expect("pixmap pool buckets mutex poisoned");
        for (_, bucket) in buckets.drain() {
            for entry in bucket {
                self.destroy_entry(entry);
            }
        }
    }

    fn destroy_entry(&self, entry: PooledPixmapImage) {
        unsafe {
            self.vk.device.destroy_image_view(entry.view, None);
            self.vk.device.destroy_image(entry.image, None);
            self.vk.device.free_memory(entry.memory, None);
        }
    }

    #[must_use]
    pub fn stats(&self) -> PixmapPoolStats {
        *self.stats.lock().expect("pixmap pool stats mutex poisoned")
    }
}

impl Drop for PixmapPool {
    fn drop(&mut self) {
        // Defensive: callers should have called `drain()` after the
        // scheduler drained its in-flight batches. If we reach Drop
        // with entries remaining, destroy them — there's no race
        // (single-threaded core loop) and the VkContext is still
        // alive (Drop order: pixmap_pool before VkContext).
        unsafe {
            let _ = self.vk.device.queue_wait_idle(self.vk.graphics_queue);
        }
        let entries: Vec<_> = self
            .buckets
            .lock()
            .expect("pixmap pool buckets mutex poisoned")
            .drain()
            .flat_map(|(_, bucket)| bucket.into_iter())
            .collect();
        for entry in entries {
            self.destroy_entry(entry);
        }
    }
}

/// `BatchResource` impl that releases by attempting to return the
/// pixmap-backing to a pool. Adopted into the open paint batch via
/// `RenderScheduler::defer_resource_release`.
#[derive(Debug)]
pub struct PooledPixmapReturn {
    pub pool: Arc<PixmapPool>,
    pub key: PixmapPoolKey,
    pub entry: Option<PooledPixmapImage>,
}

impl BatchResource for PooledPixmapReturn {
    fn release(mut self: Box<Self>, _vk: &VkContext) {
        let Some(entry) = self.entry.take() else {
            // Defensive: already released. Shouldn't happen but no UB.
            return;
        };
        if let Err(entry) = self.pool.try_return(self.key, entry) {
            self.pool.destroy_entry(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // PixmapPool needs Arc<VkContext> to construct, which is not
    // unit-testable without a real Vulkan device. Pure-decision
    // logic (eligible, bucket-cap check, key hashing) is testable
    // standalone via these helpers.

    #[test]
    fn eligible_under_max_dim() {
        assert!(PixmapPool::eligible(PixmapPoolKey {
            width: 32,
            height: 32,
            format: vk::Format::B8G8R8A8_UNORM,
        }));
        assert!(PixmapPool::eligible(PixmapPoolKey {
            width: MAX_POOLED_DIM,
            height: MAX_POOLED_DIM,
            format: vk::Format::R8_UNORM,
        }));
    }

    #[test]
    fn ineligible_over_max_dim() {
        assert!(!PixmapPool::eligible(PixmapPoolKey {
            width: MAX_POOLED_DIM + 1,
            height: 32,
            format: vk::Format::B8G8R8A8_UNORM,
        }));
        assert!(!PixmapPool::eligible(PixmapPoolKey {
            width: 32,
            height: MAX_POOLED_DIM + 1,
            format: vk::Format::B8G8R8A8_UNORM,
        }));
    }

    #[test]
    fn key_hash_distinguishes_dims_and_formats() {
        use std::collections::HashMap;
        let mut m: HashMap<PixmapPoolKey, u32> = HashMap::new();
        m.insert(
            PixmapPoolKey {
                width: 16,
                height: 16,
                format: vk::Format::R8_UNORM,
            },
            1,
        );
        m.insert(
            PixmapPoolKey {
                width: 16,
                height: 16,
                format: vk::Format::B8G8R8A8_UNORM,
            },
            2,
        );
        m.insert(
            PixmapPoolKey {
                width: 32,
                height: 16,
                format: vk::Format::R8_UNORM,
            },
            3,
        );
        assert_eq!(m.len(), 3);
    }
}
