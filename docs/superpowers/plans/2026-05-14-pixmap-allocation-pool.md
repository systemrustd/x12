# Pixmap-allocation pool — burst-absorbing `VkImage` recycling

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the per-pixmap `VkImage` + `VkDeviceMemory` + `VkImageView` alloc/free traffic on `CreatePixmap` / `FreePixmap` bursts that catastrophically lags bee (RDNA2 + Arch) and fuji (Intel + Arch) under adapta-nokto theme apply or mate-cc launcher first-paint. Reuse small recycled images from a backend-owned pool keyed by `(extent, format)`.

**Architecture:** Three structural additions:

1. **`PixmapPool` on `KmsBackend`.** `HashMap<PixmapPoolKey, VecDeque<PooledPixmapImage>>`. Keyed by `(width, height, format)` (usage is the same constant across server-owned pixmaps). Per-bucket cap of `PIXMAP_POOL_BUCKET_CAP = 32`. Total pool memory bounded by `bucket_cap × bucket_count × image_size`; for the bee workload the bucket count is ~10 (16×16, 32×32, 64×64 in R8 + BGRA + variations), so the worst-case pool memory is bounded at ~few MB.

2. **Defer-release path for `FreePixmap`.** Currently `free_pixmap` does `flush_if_needed(ProtocolBarrier)` (synchronous flush + retire) before dropping the mirror. The pool replaces this with `scheduler.defer_resource_release` (Phase 5 T2 infrastructure) adopting a `PooledPixmapReturn` BatchResource into the open paint batch. When that batch retires, the BatchResource's `release(&vk)` returns the (image, view, memory) to the pool if the bucket has room, else destroys them. **The synchronous flush goes away** — this is the load-bearing change for bee.

3. **CreatePixmap try-take.** Before `DrawableImage::new_server_owned_pixmap`, ask the pool for an entry matching `(width, height, format)`. On hit, build a `DrawableImage` from the pool entry (taking ownership of image, view, memory, format, extent, current_layout); skip `initialize_clear`. On miss, fall through to today's fresh-allocation path.

**Tech Stack:** Rust, ash (Vulkan), existing Phase 5 `defer_resource_release` infrastructure.

---

## Prerequisite — confirm post-Phase-5 baseline

```bash
cd /home/jos/Projects/yserver
git log --oneline graphics-followups | head -10
rg -n 'flush_if_needed.*ProtocolBarrier' crates/yserver/src/kms/backend.rs | wc -l
rg -n 'DrawableImage::new_server_owned_pixmap' crates/yserver/src/kms/backend.rs
```

Expected:
- Branch tip `6217120` (Phase 5 T7) or descendant.
- `ProtocolBarrier` flushes: still many, but `free_pixmap` (~`backend.rs:9583`) is one of them — this plan's T2 retires it.
- Single `new_server_owned_pixmap` caller in `allocate_pixmap_mirror` (~`backend.rs:6849`).
- `cargo +nightly fmt --check`, `cargo clippy -p yserver`, `cargo test --workspace` all green.

If any of the above don't hold, STOP.

## Phase context

The post-3F-2 perf profile pinned the bee/RDNA2 + Arch lag at **amdgpu ioctl rate** under burst pixmap traffic, not at paint-side waits (`submit_and_wait` was 0.09% children). Phase 4 narrowed the close-time wait and Phase 5 retired the readback + scratch-grow waits — neither move closed the bee lag, as their perf-time targets were not the bottleneck.

The actual bottleneck is the kernel allocator: `vkCreateImage` + `vkAllocateMemory` translate into amdgpu `GEM_CREATE` + `VM_BIND` ioctls; under burst rate (mate-cc launcher fires hundreds of 16×16 / 32×32 widget pixmaps in <100ms), the kernel allocator serializes. CPU spends most of its time in `entry_SYSCALL_64` and the cmwq paths. The fix is to NOT make those ioctls — recycle small pixmaps from a userspace pool so a `CreatePixmap` of a recently-freed dimension reuses the existing VkImage instead of round-tripping the kernel.

**Cross-vendor confirmation**: the reproducer was also catastrophic on fuji (Intel + Arch) and slow-but-survivable on imac (Polaris11 + older Ubuntu). The win from this pool is therefore *not* AMD-specific — it cuts the alloc/free traffic that any kernel allocator has to serialize.

### Why the pool helps even when freed pixmaps are still GPU-referenced

Today's `free_pixmap` synchronously flushes the open paint batch before dropping the mirror, so the image is guaranteed not-in-flight by the time `vkDestroyImage` runs. The flush is the *direct* cost: it submits + waits, blocking the input loop. The kernel `vkFreeMemory` is downstream cost.

The pool's defer-release replaces the synchronous flush with `scheduler.defer_resource_release`: the (image, view, memory) tuple is adopted into the currently-open paint batch as a `BatchResource`. When that batch retires (after its fence signals — non-blocking on the input loop), the BatchResource's `release(&vk)` runs and *returns the image to the pool* (or destroys it if the bucket is full). So:

- The synchronous flush goes away. **This is the primary CPU win.**
- The kernel `vkCreateImage` + `vkAllocateMemory` on subsequent `CreatePixmap` of the same dimension goes away too (pool hit). **This is the primary kernel-ioctl win.**

Both effects compound: fewer synchronous flushes lets the input loop service input events smoothly; fewer kernel ioctls lets amdgpu's lock contention drop.

### Out of scope (deferred)

- **Window mirror pooling.** Windows tend to be larger and longer-lived; alloc/free rate is dominated by pixmaps. Pool windows in a follow-up if/when profiling shows a need.
- **DRI3 imported pixmaps.** `ImageBacking::Imported` (dma-buf clients) is a different code path; not pooled. The pool keys on `ServerOwned` pixmaps only.
- **Cursor pixmap pooling.** Cursors are 32×32-or-similar but allocated less frequently; the BGRA8 cursor image lifetime is short. The same pool *could* cover cursor source/mask pixmaps since they're `CreatePixmap`-then-`FreePixmap` per cursor change. Keep them in pool naturally — no special-casing needed.
- **`initialize_clear` fence-narrowing.** The `vkQueueWaitIdle` at `target.rs:735` (deferred from Phase 5) is unchanged. Pool reuse skips `initialize_clear` entirely; fresh allocations still take the existing wait. A future micro-pass can fence-narrow it.
- **Memory budget cap (global).** Per the user's plan-prep answer, the pool uses per-bucket count only, no global memory budget. For worst-case workloads the bucket-count × cap is bounded by client behavior; future tuning can add a global cap if needed.

### Key invariants this plan preserves

1. **Drop order**: `KmsBackend.scheduler` before `KmsBackend.pixmap_pool` before `KmsBackend.ops_command_pool`. Scheduler may have BatchResources holding pool tokens; those must release before the pool drains. The pool must drain before VkContext drops (the pool's drain destroys VkImages).
2. **Mirror layout tracking.** A pooled image's `current_layout` is preserved exactly across the pool sojourn (whatever value `DrawableImage::current_layout` held at return time — likely `SHADER_READ_ONLY_OPTIMAL` in steady state, but could be `UNDEFINED` if the pixmap was created then immediately freed without paint, or `TRANSFER_DST_OPTIMAL` if it was caught mid-upload by a free-during-paint pattern). New tenant's first barrier transitions from whatever-layout-was-saved to the next layout normally. No "always SHADER_READ_ONLY_OPTIMAL" assumption.
3. **Picture rescue path**: `free_pixmap`'s "rescue mirror for live pictures" path stays unchanged. Only the no-rescue-needed branch goes through the pool.
4. **`mark_full_damage` on fresh pixmap**: applied to BOTH fresh and pool-reused mirrors. The first paint will overwrite the entire image; previous tenant contents are invisible. (The X11 spec says `CreatePixmap` contents are undefined; clients always clear or paint first.)
5. **No semantic change in failure modes.** Pool miss → existing path. Pool exhaustion (allocation fails) → existing path. Pool drain errors at shutdown → log + leak (consistent with Phase 4/5 teardown).

## File structure

| File | Role | Touched in |
|---|---|---|
| `crates/yserver/src/kms/vk/pixmap_pool.rs` | New file. `PixmapPool` struct + `PixmapPoolKey` + `PooledPixmapImage` + `PooledPixmapReturn` (BatchResource impl) + `PIXMAP_POOL_BUCKET_CAP` + `MAX_POOLED_DIM`. | T1 |
| `crates/yserver/src/kms/vk/mod.rs` | `pub mod pixmap_pool;` | T1 |
| `crates/yserver/src/kms/vk/target.rs` | `DrawableImage::new_from_pool(...)` constructor. Decompose helper that extracts (image, view, memory, format, extent, current_layout) from `DrawableImage` for pool return — used by T2. | T1 + T2 |
| `crates/yserver/src/kms/backend.rs` | Add `pixmap_pool: Option<PixmapPool>` field. Init at backend construction. Wire `free_pixmap` to defer-release into the pool. Wire `allocate_pixmap_mirror` to try-take from the pool. Drain pool at shutdown. | T2 + T3 + T4 |
| `docs/superpowers/plans/2026-05-14-pixmap-allocation-pool-results.md` | Results doc with hardware smoke section. | T6 |
| `docs/status.md` | Move pixmap-pool from "Remaining" to "Done" after T6. | T6 |

## Pre-task notes (read before starting)

1. **Pool key**: `(width, height, format)` — the format is determined by pixmap depth (depth-1/8 → R8; depth-24/32 → BGRA8) so equivalent-format pixmaps with the same dimensions can swap. Usage is always the same constant for server-owned pixmaps so it's not a key field.

2. **Per-bucket cap (`PIXMAP_POOL_BUCKET_CAP = 32`)**: tuneable, set in `pixmap_pool.rs`. Rationale:
   - mate-cc burst is roughly 100-300 pixmaps in <100ms, dominated by ~5-10 unique (extent, format) keys. A cap of 32 absorbs the burst with headroom.
   - 32 BGRA8 32×32 images is `32 × (32 × 32 × 4 + alignment) ≈ 128 KB per bucket`. Across ~10 active buckets, worst-case pool memory is ~1.5 MB. Acceptable.
   - When a bucket is full, the BatchResource's `release` destroys the image (no growth past cap).

3. **Max pooled dimension (`MAX_POOLED_DIM = 128`)**: pixmaps with `width > MAX_POOLED_DIM || height > MAX_POOLED_DIM` skip the pool (both on free and on alloc). Above 128×128 the per-image memory grows quadratically and the reuse rate drops sharply (clients tend to use unique sizes at that scale). Rationale: keeps the pool's worst-case memory bounded and skips a regime where it doesn't help.

4. **`PooledPixmapReturn` BatchResource — `Arc<Mutex<…>>` required, NOT `RefCell`** (codex P0 from round 1): `BatchResource: Send + std::fmt::Debug` is the existing trait bound (paint_batch.rs:146). `Arc<RefCell<…>>` is NOT `Send` (RefCell isn't Sync). Use `Arc<PixmapPool>` where `PixmapPool` internally uses `std::sync::Mutex<HashMap<…>>` for its buckets + `Mutex<PixmapPoolStats>` for stats. The single-threaded core loop invariant means the Mutex is never contended; the lock cost is one atomic CAS per pool op, well below the per-op savings the pool unlocks.

   Alternative considered + rejected: drop the `Send` bound on `BatchResource`. Per its existing doc-comment ("phase 6.8's single-core invariant means that's the backend thread"), the bound is conservative — kept in case a future phase moves PaintBatch off the backend thread. Dropping it would simplify the pool but constrain future phases; Mutex is the lower-risk choice.

   **Cycle hazard**: `Arc<PixmapPool>` held by both `KmsBackend.pixmap_pool` and any in-flight `PooledPixmapReturn` BatchResource. No cycle — the BatchResource doesn't hold a strong ref to anything that holds the BatchResource. Standard `Arc` shared-ownership pattern.

5. **Pool teardown ordering**: at backend Drop / shutdown, the scheduler drains its `submitted_paint_batches` first (Phase 4 T5 path). Each batch's retire walks its `retire_resources`, which includes any `PooledPixmapReturn` instances; each one's `release` returns the image to the pool (or destroys if bucket full). Once the scheduler-drain returns, the pool holds the survivors and no BatchResource holds an `Arc<PixmapPool>` strong ref. **T4 then calls `pool.drain()` via the existing `Arc` (no `try_unwrap` needed)** — `drain` is a `&self` method that takes the bucket mutex and destroys every remaining entry. The `Arc::strong_count` reaching 1 is the implicit invariant (KmsBackend holds the only remaining ref) but isn't load-bearing on a `try_unwrap` call.

   Defensive check at shutdown (optional, useful for debug): log a warning if `Arc::strong_count(&self.pixmap_pool) > 1` when `drain()` is called — that indicates a BatchResource leaked past scheduler drain, which would be a Phase-4-T5 ordering bug, not a pool bug.

6. **`DrawableImage::new_from_pool`**: takes the pool entry's (image, view, memory, format, extent, current_layout) + an `Arc<VkContext>` and constructs a `DrawableImage` with:
   - `vk_image`, `vk_image_view`, `extent`, `format`, `vk` set from the pool entry.
   - `mask_view: None`, `no_alpha_src_view: None` — these are lazy and per-format-need; the previous tenant may have built them but they get destroyed at pool-return time (cheap to rebuild on demand).
   - `damage: MirrorDamage::default()` — the caller's `mark_full_damage` will set this.
   - `current_layout` from the pool entry — preserves the previous tenant's terminal layout. The first paint's upload transitions from that layout normally.
   - `backing: ImageBacking::ServerOwned { vk_memory: memory }` — same as fresh alloc.

7. **Picture rescue path stays unchanged**. `free_pixmap`'s "if a live picture references the pixmap, move the mirror to `picture_rescued_images`" branch is rare (fvwm cursor pattern) and not pool-relevant — keep the existing direct-Drop semantics. Only the no-rescue-needed branch routes through the pool.

8. **CPU-side layout tracking**: the post-3F-2 invariant that CPU-tracked layout matches GPU-side layout is preserved. Pool entries store the terminal layout (whatever it was when the previous tenant freed); new tenant uses it as the starting `current_layout`. No layout-state races.

9. **Test plan**:
   - Unit: `PixmapPool::try_take` returns `None` on empty bucket, `Some` on populated bucket, removes from bucket. `try_return` adds to bucket up to cap, returns `false` (caller destroys) when full. `MAX_POOLED_DIM` gate.
   - Integration: synthetic burst test — create 100 pixmaps of `(32, 32, depth=24)`, free them, re-create 100, assert no fresh allocations after the first 32. Requires a `PixmapPool::stats` accessor with `total_hits`, `total_misses`, `total_destroyed` counters.
   - Hardware smoke (user-owned): adapta-nokto + mate-cc on bee + fuji — the load-bearing test. T6 captures.

10. **`free_pixmap` synchronous-flush removal**: same argument as Phase 5 T3/T4/T5 scratch grow defer-release. The currently-open batch's CB may reference the pixmap's image; adopting into the open batch ensures the image stays alive until that batch's fence signals; the BatchResource then returns to pool (or destroys). No synchronous wait at `free_pixmap` time.

   **Catch**: if the pool's `Arc<RefCell<…>>` is held by a BatchResource that ends up in a Poisoned batch (which Drop's no-op the BatchResources by leaking them, per Phase 4 contract), the pooled-or-destroyed handle leaks. This matches every other BatchResource's behaviour on Poisoned-batch leak — acceptable. (Defensive note: a Poisoned batch means the renderer is fatal; resource leaks are the least of our concerns.)

11. **`renderer_failed` interaction**: if `self.renderer_failed` is true, skip the pool — go straight to `DrawableImage::Drop` which won't even fire (Phase 5 leaked-Submitted contract). Actually no: if renderer_failed, `paint_resources` short-circuits paint paths, but FreePixmap is a protocol handler that still runs. The mirror's Drop still runs and destroys VkImage/memory synchronously. The pool path can stay; in the renderer-failed state it's a redundant deferral but still correct.

12. **clippy / fmt**: standard pattern. Plain clippy (not pedantic per AGENTS.md). nightly fmt.

---

## Task 1: `PixmapPool` infrastructure (no callers wired yet)

**Goal:** Add the pool struct, key, entry, BatchResource impl, and `DrawableImage::new_from_pool` constructor. Pure addition; no caller wired.

**Files:**
- Create: `crates/yserver/src/kms/vk/pixmap_pool.rs`
- Modify: `crates/yserver/src/kms/vk/mod.rs` (add `pub mod pixmap_pool;`)
- Modify: `crates/yserver/src/kms/vk/target.rs` (add `DrawableImage::new_from_pool`)

### Step 1: Create `pixmap_pool.rs`

- [ ] **Step 1: Add `PixmapPool` + key + entry + BatchResource impl**

```rust
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
    sync::{Arc, Mutex},
};

use ash::vk;

use crate::kms::{
    scheduler::paint_batch::BatchResource,
    vk::device::VkContext,
};

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

#[derive(Debug)]
pub struct PixmapPool {
    vk: Arc<VkContext>,
    // Mutex (not RefCell) so PooledPixmapReturn's Arc<PixmapPool>
    // satisfies BatchResource's Send bound. Single-threaded core
    // loop means contention is zero; Mutex is the cheapest Send-safe
    // option (one atomic CAS per pool op).
    buckets: Mutex<HashMap<PixmapPoolKey, VecDeque<PooledPixmapImage>>>,
    stats: Mutex<PixmapPoolStats>,
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
        let mut buckets = self.buckets.lock().expect("pixmap pool buckets mutex poisoned");
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
        let mut buckets = self.buckets.lock().expect("pixmap pool buckets mutex poisoned");
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
        let mut buckets = self.buckets.lock().expect("pixmap pool buckets mutex poisoned");
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
```

### Step 2: Register module

- [ ] **Step 2: Add `pub mod pixmap_pool;` in `vk/mod.rs`**

Edit `crates/yserver/src/kms/vk/mod.rs` — add `pub mod pixmap_pool;` alongside the other module declarations.

### Step 3: `DrawableImage::new_from_pool`

- [ ] **Step 3: Constructor in `target.rs`**

```rust
impl DrawableImage {
    /// Construct a `DrawableImage` from a pooled entry. Skips
    /// `initialize_clear` — the previous tenant's pixels are
    /// invisible (caller marks `mark_full_damage`; the first paint
    /// overwrites the whole image). The pool entry's
    /// `current_layout` is preserved so the first upload's
    /// pre-barrier transitions correctly.
    ///
    /// Lazy `mask_view` / `no_alpha_src_view` are set to `None`;
    /// they'll be rebuilt on demand if needed.
    pub fn new_from_pool(
        vk: Arc<VkContext>,
        entry: crate::kms::vk::pixmap_pool::PooledPixmapImage,
        format: vk::Format,
        extent: vk::Extent2D,
    ) -> Self {
        Self {
            vk_image: entry.image,
            vk_image_view: entry.view,
            mask_view: None,
            no_alpha_src_view: None,
            extent,
            format,
            backing: ImageBacking::ServerOwned { vk_memory: entry.memory },
            damage: MirrorDamage::default(),
            current_layout: entry.current_layout,
            vk,
        }
    }

    /// Decompose a `DrawableImage` into a pooled-pixmap-shape
    /// (image, view, memory, current_layout) for return-to-pool.
    /// Destroys the lazy views first (they're format-specific and
    /// not pooled). Pool-bound Vulkan handles transfer to the
    /// returned `PooledPixmapImage`; the rest of `self` (notably
    /// the `Arc<VkContext>`) drops normally.
    ///
    /// **Important — does NOT use `mem::forget`.** Codex P1 from
    /// plan review round 1: forgetting `self` would leak the
    /// `Arc<VkContext>` strong-count, eventually preventing
    /// VkContext::Drop's device wait. Instead, the pool-bound
    /// handles are swapped with `vk::*::null()` so the normal
    /// `Drop for DrawableImage` runs but sees null handles —
    /// Vulkan's spec permits destroying null handles as a no-op
    /// (every `destroy_*` and `free_memory` call), so Drop becomes
    /// a no-op on the Vulkan side. The `Arc<VkContext>` and other
    /// non-handle fields drop normally.
    ///
    /// Panics if `backing` is `Imported` (DRI3 dma-buf imports
    /// don't go through the pool; caller must check).
    pub fn into_pool_entry(mut self) -> crate::kms::vk::pixmap_pool::PooledPixmapImage {
        let ImageBacking::ServerOwned { vk_memory } = self.backing else {
            panic!("DrawableImage::into_pool_entry: Imported backing cannot be pooled");
        };
        // Destroy lazy format-specific views (not pooled).
        let mask_view = self.mask_view.take();
        let no_alpha = self.no_alpha_src_view.take();
        unsafe {
            if let Some(v) = mask_view {
                self.vk.device.destroy_image_view(v, None);
            }
            if let Some(v) = no_alpha {
                self.vk.device.destroy_image_view(v, None);
            }
        }
        // Swap pool-bound handles out, leaving nulls in their
        // place so self's Drop is a no-op for these handles.
        let image = std::mem::replace(&mut self.vk_image, vk::Image::null());
        let view = std::mem::replace(&mut self.vk_image_view, vk::ImageView::null());
        // Replace backing memory with null so Drop's free_memory
        // is also a no-op.
        self.backing = ImageBacking::ServerOwned { vk_memory: vk::DeviceMemory::null() };

        // self drops here: Vk handle destruction calls are no-ops
        // on null; vk Arc drops normally; damage / layout fields
        // drop naturally.
        crate::kms::vk::pixmap_pool::PooledPixmapImage {
            image,
            view,
            memory: vk_memory,
            current_layout: self.current_layout,
        }
    }
}
```

**Note**: this approach intentionally uses Vulkan's "null handle = no-op destroy" guarantee instead of `mem::forget`. The implementer should verify against `Drop for DrawableImage`'s body that every handle destruction tolerates null (per Vulkan spec, `destroy_image`, `destroy_image_view`, `free_memory` all permit `VK_NULL_HANDLE`). If `Drop` ever grows a destruction call that doesn't tolerate null, this helper must guard it.

### Step 4: Unit tests for `pixmap_pool.rs`

- [ ] **Step 4: Pure-logic tests in `pixmap_pool.rs`**

Add `#[cfg(test)] mod tests` at the bottom. The pool's HashMap + VecDeque + counter logic is pure Rust; the VkContext is needed only for Drop/destroy. To test without a real device:

```rust
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
        m.insert(PixmapPoolKey { width: 16, height: 16, format: vk::Format::R8_UNORM }, 1);
        m.insert(PixmapPoolKey { width: 16, height: 16, format: vk::Format::B8G8R8A8_UNORM }, 2);
        m.insert(PixmapPoolKey { width: 32, height: 16, format: vk::Format::R8_UNORM }, 3);
        assert_eq!(m.len(), 3);
    }
}
```

Fuller integration tests (try_take + try_return roundtrip) need a `VkContext` — covered by binary smoke + the synthetic burst test in T5.

### Step 5: Gates + commit

- [ ] **Step 5: Validate**

```bash
cargo +nightly fmt
cargo clippy -p yserver
cargo test --workspace
```

Expected:
- fmt clean.
- clippy: 5 pre-existing `doc_lazy_continuation` only.
- tests green; 3 new `pixmap_pool::tests::*` pass.

Commit message:

```text
refactor(kms): add PixmapPool infrastructure (pixmap-pool T1)

Backend-owned pool of recycled (VkImage, VkImageView, VkDeviceMemory)
triples for server-owned X pixmaps. Keyed by (width, height, format);
per-bucket cap 32; max pooled dim 128.

PooledPixmapReturn BatchResource adopts into the open paint batch
via Phase 5 T2 defer-release. When the batch retires, BatchResource
release returns the entry to the pool (or destroys if bucket full).

DrawableImage::new_from_pool builds a fresh DrawableImage from a
pool entry, preserving current_layout. DrawableImage::into_pool_entry
decomposes the DrawableImage (destroying lazy mask/no-alpha views)
and prepares the entry for return.

Pure addition; no caller wired yet (T2 wires free_pixmap; T3 wires
create_pixmap).
```

### Done conditions for T1

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` produces only 5 pre-existing warnings.
3. `cargo test --workspace` green; 3 new `pixmap_pool::tests::*` pass.
4. `crates/yserver/src/kms/vk/pixmap_pool.rs` exists.
5. `DrawableImage::new_from_pool` + `DrawableImage::into_pool_entry` exist in `target.rs`.
6. No call site of these new APIs yet (verified by grep).
7. Single new commit.

---

## Task 2: Wire `free_pixmap` → defer-release into pool

**Goal:** Replace `free_pixmap`'s synchronous `flush_if_needed(ProtocolBarrier)` + Drop with a `defer_resource_release` of a `PooledPixmapReturn`. **This is the load-bearing CPU win** for the bee/fuji adapta workload.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (add `pixmap_pool` field; wire `free_pixmap`)

### Step 1: Add `pixmap_pool` field on `KmsBackend`

- [ ] **Step 1: Field declaration + init + drop-order audit**

In `backend.rs`, add a field on `KmsBackend` (alongside `scheduler` / `ops_command_pool`):

```rust
pub(crate) pixmap_pool: Option<Arc<crate::kms::vk::pixmap_pool::PixmapPool>>,
```

Init at backend construction (both `open_with_commit` and the ynest construction path). The pool is created once `vk` is available:

```rust
let pixmap_pool = vk
    .as_ref()
    .map(|vkctx| Arc::new(crate::kms::vk::pixmap_pool::PixmapPool::new(Arc::clone(vkctx))));
```

Set `backend.pixmap_pool = pixmap_pool` at the same point `backend.ops_staging = Some(...)` is set.

**Drop order**: `KmsBackend.scheduler` before `KmsBackend.pixmap_pool` before `KmsBackend.ops_command_pool` before `KmsBackend.vk`. Struct-field order determines Drop order (Rust drops fields in declaration order). Audit the existing field order and place `pixmap_pool` between `scheduler` and `ops_command_pool`.

### Step 2: Wire `free_pixmap` defer-release path

- [ ] **Step 2: Edit `free_pixmap` (~`backend.rs:9573`)**

Before T2, `free_pixmap` does:

```rust
fn free_pixmap(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
    if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
        log::error!("free_pixmap: pre-destruction flush failed ({e:?}); ...");
        return Err(...);
    }
    if let Some(ps) = self.pixmaps.remove(&host_xid) {
        if let Some(mirror) = ps.vk_mirror {
            // ... picture rescue path ...
            // if no rescue, mirror drops here (Drop releases VkImage/memory)
        }
    }
    Ok(())
}
```

After T2:

```rust
fn free_pixmap(&mut self, _origin: Option<OriginContext>, host_xid: u32) -> io::Result<()> {
    // pixmap-pool T2: no synchronous flush. The mirror's VkImage
    // may be referenced by commands in the currently-open paint
    // batch or in any in-flight batch on submitted_paint_batches.
    // We adopt the mirror's (image, view, memory) into the open
    // batch as a PooledPixmapReturn BatchResource; when that batch
    // retires (fence signals), the BatchResource's release returns
    // the entry to the pool (or destroys if the bucket is full).
    //
    // Replaces Phase 3B's drawable-destruction barrier flush at
    // this site.

    let Some(ps) = self.pixmaps.remove(&host_xid) else {
        return Ok(());
    };
    let Some(mirror) = ps.vk_mirror else {
        return Ok(());
    };

    // Picture rescue path stays unchanged: a live picture
    // referencing this pixmap takes the mirror so its alpha can
    // outlive the FreePixmap (fvwm cursor pattern).
    let mut mirror_opt = Some(mirror);
    for (&pic_xid, pic) in &self.pictures {
        if let PictureState::Drawable { host_xid: xid, .. } = pic
            && *xid == host_xid
            && let Some(m) = mirror_opt.take()
        {
            self.picture_rescued_images.insert(pic_xid, m);
            break;
        }
    }
    let Some(mirror) = mirror_opt else {
        // Rescue took ownership; nothing to pool.
        return Ok(());
    };

    // **Every mirror with a live VkImage MUST go through
    // defer-release**, not direct-drop (codex P0 round 3:
    // DrawableImage::Drop is non-waiting, so direct-dropping a
    // mirror after the synchronous flush is removed is UAF /
    // driver-crash risk for any in-flight VkImage). Eligibility
    // and bucket-cap rejection are handled INSIDE
    // `PooledPixmapReturn::release` via `try_return`'s Err path:
    // ineligible (oversize) and full-bucket entries are destroyed
    // by the BatchResource at batch-retire time — by which point
    // the open batch's fence has signalled and the GPU is done
    // with the image. **This is the load-bearing UAF avoidance.**

    // Acquire defer-release prerequisites. If ANY is missing
    // (pre-init, partial init, post-failure), fall back to the
    // pre-T2 behaviour: synchronous flush + drop. We can't
    // safely defer without `defer_resource_release`'s
    // (vk_arc, pool_handle) inputs, so the only options are
    // (a) flush-then-drop (today's known-safe path), or (b) leak.
    // Pick (a) since it preserves behaviour for these defensive
    // edge cases.
    let (Some(pool), Some(vk_arc), Some(pool_handle)) = (
        self.pixmap_pool.as_ref().cloned(),
        self.vk.as_ref().cloned(),
        self.ops_command_pool.as_ref().map(|p| p.handle()),
    ) else {
        // No defer infrastructure — preserve the pre-T2 flush
        // + direct-drop behaviour for this rare path. Should
        // never trigger post-init.
        if let Err(e) = self.flush_if_needed(BatchFlushReason::ProtocolBarrier) {
            log::error!(
                "free_pixmap fallback path: pre-destruction flush failed ({e:?}); \
                 leaking mirror to avoid UAF"
            );
            // Leak rather than UAF. Renderer is already in a bad
            // state if this branch ran.
            std::mem::forget(mirror);
            return Err(std::io::Error::other(format!(
                "free_pixmap fallback flush failed: {e:?}"
            )));
        }
        drop(mirror);
        return Ok(());
    };

    // Defer-release path (the common case for every Vulkan-up
    // backend). Build the BatchResource — eligibility +
    // bucket-cap are evaluated INSIDE its release(), not here.
    let key = crate::kms::vk::pixmap_pool::PixmapPoolKey {
        width: mirror.extent.width,
        height: mirror.extent.height,
        format: mirror.format,
    };
    let entry = mirror.into_pool_entry();
    let pooled_return = Box::new(crate::kms::vk::pixmap_pool::PooledPixmapReturn {
        pool,
        key,
        entry: Some(entry),
    });

    // Phase 5 T2 defer-release. Adopts into the currently-open
    // paint batch (creating an Idle one if none). When that batch
    // retires (its fence signals), the BatchResource's release
    // runs — try_return attempts to pool; on Err (ineligible /
    // full bucket) destroys.
    self.scheduler.defer_resource_release(vk_arc, pool_handle, pooled_return);

    Ok(())
}
```

**Key invariants preserved**:
- Picture rescue path: same shape as before.
- Renderer-failed: the defer-release path still runs (it's idempotent on a Poisoned batch — pool's BatchResource leaks with the batch, no UAF).
- Oversize pixmaps: skip the pool, direct Drop. The direct-Drop path retains its synchronous `queue_wait_idle` (Phase 5 deferral) — accepted.

**What's removed**: the synchronous `flush_if_needed(ProtocolBarrier)` at the top of `free_pixmap`. This is the win.

### Step 3: Gates + commit

- [ ] **Step 3: Validate**

Run `cargo +nightly fmt`, `cargo clippy -p yserver`, `cargo test --workspace`.

If hardware available, run `just yserver-mate-hw-release` and exercise mate-cc / adapta apply — should feel noticeably better on bee. (User-owned smoke; not blocking for T2 commit.)

Commit message:

```text
refactor(kms): wire free_pixmap → defer-release into PixmapPool (pixmap-pool T2)

Replaces free_pixmap's synchronous flush_if_needed(ProtocolBarrier)
with a defer_resource_release adopting a PooledPixmapReturn into
the currently-open paint batch. When the batch retires (fence
signals — non-blocking on the input loop), the BatchResource's
release returns the (image, view, memory) to the pool if the
bucket has room, else destroys.

Headline CPU win: removes a synchronous flush per FreePixmap. Under
mate-cc / adapta-nokto burst (hundreds of pixmaps/sec) this collapses
hundreds of blocking submits per second into one per composite cycle.

Picture-rescue path unchanged: a live picture referencing the freed
pixmap still takes the mirror via picture_rescued_images. Oversize
pixmaps (>MAX_POOLED_DIM) skip the pool and direct-Drop (synchronous
wait retained for now; Phase 5 follow-up territory).
```

### Done conditions for T2

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` clean.
3. `cargo test --workspace` green.
4. `flush_if_needed(BatchFlushReason::ProtocolBarrier)` is GONE from `free_pixmap`'s body. (Other `ProtocolBarrier` flushes unrelated to free_pixmap stay.)
5. `pixmap_pool` field is `Option<Arc<PixmapPool>>` on `KmsBackend`, initialized at backend construction.
6. `defer_resource_release` is called from `free_pixmap` with a `PooledPixmapReturn`.
7. Picture-rescue path preserved.
8. Single new commit.

---

## Task 3: Wire `CreatePixmap` → try-take from pool

**Goal:** Hit the pool before allocating a fresh `DrawableImage`. The kernel-ioctl win.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (`allocate_pixmap_mirror`)

### Step 1: Edit `allocate_pixmap_mirror`

- [ ] **Step 1: Try-take before fresh alloc**

In `backend.rs:~6839`, today:

```rust
fn allocate_pixmap_mirror(
    &self,
    width: u32,
    height: u32,
    depth: u8,
) -> Option<crate::kms::vk::target::DrawableImage> {
    let vkctx = self.vk.as_ref()?;
    if width == 0 || height == 0 {
        return None;
    }
    match crate::kms::vk::target::DrawableImage::new_server_owned_pixmap(
        std::sync::Arc::clone(vkctx),
        width,
        height,
        depth,
    ) {
        Ok(mut img) => {
            if let Some(pool) = self.ops_command_pool.as_ref()
                && let Err(e) = img.initialize_clear(pool.handle())
            {
                log::warn!("pixmap mirror initialize_clear failed: {e:?}");
            }
            Some(img)
        }
        Err(e) => { ... }
    }
}
```

After T3:

```rust
fn allocate_pixmap_mirror(
    &self,
    width: u32,
    height: u32,
    depth: u8,
) -> Option<crate::kms::vk::target::DrawableImage> {
    let vkctx = self.vk.as_ref()?;
    if width == 0 || height == 0 {
        return None;
    }

    // pixmap-pool T3: try the pool first. Pool keys on
    // (width, height, format); derive format from depth here so
    // we don't peek into DrawableImage internals.
    let format = match depth {
        1 | 8 => ash::vk::Format::R8_UNORM,
        24 | 32 => ash::vk::Format::B8G8R8A8_UNORM,
        _ => ash::vk::Format::B8G8R8A8_UNORM, // matches new_server_owned_pixmap fallback
    };
    let key = crate::kms::vk::pixmap_pool::PixmapPoolKey {
        width,
        height,
        format,
    };
    if let Some(pool) = self.pixmap_pool.as_ref()
        && let Some(entry) = pool.try_take(key)
    {
        // Pool hit. Construct DrawableImage from the entry —
        // current_layout preserved; lazy mask/no-alpha views
        // start at None; skips initialize_clear.
        let img = crate::kms::vk::target::DrawableImage::new_from_pool(
            std::sync::Arc::clone(vkctx),
            entry,
            format,
            ash::vk::Extent2D { width, height },
        );
        return Some(img);
    }

    // Pool miss — fall through to fresh allocation.
    match crate::kms::vk::target::DrawableImage::new_server_owned_pixmap(
        std::sync::Arc::clone(vkctx),
        width,
        height,
        depth,
    ) {
        Ok(mut img) => {
            if let Some(pool_cb) = self.ops_command_pool.as_ref()
                && let Err(e) = img.initialize_clear(pool_cb.handle())
            {
                log::warn!("pixmap mirror initialize_clear failed: {e:?}");
            }
            Some(img)
        }
        Err(e) => {
            log::warn!(
                "DrawableImage::new_server_owned_pixmap({width}x{height} d{depth}): \
                 {e} — pixmap will run pixman-only"
            );
            None
        }
    }
}
```

**Note**: `format` derivation duplicates the logic inside `new_server_owned_pixmap`. To avoid drift, consider exposing `DrawableImage::format_for_depth(depth) -> vk::Format` as a public helper that both call sites use. **Recommended**: factor into a helper, refactor both sites. Plan T3 includes this refactor.

### Step 2: Factor `format_for_depth` helper

- [ ] **Step 2: Helper in `target.rs`**

```rust
impl DrawableImage {
    /// Map an X11 pixmap depth to its server-owned mirror format.
    /// Used by both `new_server_owned_pixmap` (for fresh alloc) and
    /// `KmsBackend::allocate_pixmap_mirror`'s pool-key derivation.
    #[must_use]
    pub fn format_for_pixmap_depth(depth: u8) -> vk::Format {
        match depth {
            1 | 8 => vk::Format::R8_UNORM,
            24 | 32 => vk::Format::B8G8R8A8_UNORM,
            other => {
                log::warn!(
                    "DrawableImage::format_for_pixmap_depth: unhandled depth {other} → \
                     defaulting to B8G8R8A8_UNORM"
                );
                vk::Format::B8G8R8A8_UNORM
            }
        }
    }
}
```

Update `new_server_owned_pixmap` to delegate. Update `allocate_pixmap_mirror` to use this helper.

### Step 3: Gates + commit

- [ ] **Step 3: Validate**

Run the gates. Run hardware smoke if available — mate-cc / adapta-nokto on bee should be substantially better (this is where the kernel-ioctl win compounds with T2's flush removal).

Commit message:

```text
refactor(kms): wire allocate_pixmap_mirror → try-take from PixmapPool (pixmap-pool T3)

CreatePixmap now consults the pool before allocating a fresh VkImage.
Pool key (width, height, format) derived from depth via the new
DrawableImage::format_for_pixmap_depth helper.

On pool hit: DrawableImage::new_from_pool constructs the mirror
from the recycled (image, view, memory) triple — preserves the
previous tenant's terminal layout, skips initialize_clear (the
first paint will overwrite the whole image).

On pool miss: fresh allocation path unchanged.

Combined with T2's flush removal, this collapses the kernel-ioctl
storm on adapta-nokto + mate-cc bursts.
```

### Done conditions for T3

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` clean.
3. `cargo test --workspace` green.
4. `allocate_pixmap_mirror` calls `self.pixmap_pool.as_ref().and_then(...).map(...)` before `new_server_owned_pixmap`.
5. `DrawableImage::format_for_pixmap_depth` exists and is used by both call sites.
6. Single new commit.

---

## Task 4: Shutdown drain

**Goal:** Ensure `PixmapPool::drain` is called at backend shutdown AFTER the scheduler drains in-flight batches. Prevents leaked pool entries (and matches Phase 4 T5's shutdown drain pattern).

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs`

### Step 1: Wire `pixmap_pool.drain()` after `scheduler.drain_submitted_paint_batches()`

- [ ] **Step 1: Edit shutdown path**

The shutdown sequence (search for `drain_submitted_paint_batches` — landed in Phase 4 T5 at `backend.rs:~8287`) currently:

```rust
unsafe { vk.device.device_wait_idle(...) };
self.scheduler.drain_submitted_paint_batches()?;
// other teardown
```

After T4:

```rust
unsafe { vk.device.device_wait_idle(...) };
self.scheduler.drain_submitted_paint_batches()?;
// pixmap-pool T4: every PooledPixmapReturn BatchResource has
// fired by now (scheduler drain walked retire_resources). The
// pool's buckets hold entries to destroy.
if let Some(pool) = self.pixmap_pool.as_ref() {
    pool.drain();
}
// other teardown
```

`PixmapPool::Drop` is the defensive fallback: if `drain()` wasn't called (or if entries arrived after — shouldn't happen), the pool's Drop destroys them with a `queue_wait_idle` guard. Drop order ensures pool drops before VkContext.

### Step 2: Gates + commit

- [ ] **Step 2: Validate**

Run gates. Confirm clean shutdown via `RUST_LOG=info` startup-then-quit cycle — no leaked-pool-entry warnings.

Commit message:

```text
refactor(kms): drain PixmapPool on backend shutdown (pixmap-pool T4)

After scheduler.drain_submitted_paint_batches retires every in-flight
batch (which runs every PooledPixmapReturn's release), the pool's
buckets hold entries the scheduler-drain returned. PixmapPool::drain
destroys them synchronously.

PixmapPool::Drop is the defensive fallback for shutdown paths that
miss the explicit drain.
```

### Done conditions for T4

1. `cargo +nightly fmt --check` clean.
2. `cargo clippy -p yserver` clean.
3. `cargo test --workspace` green.
4. `self.pixmap_pool.as_ref().map(|p| p.drain())` is called in the shutdown sequence after `scheduler.drain_submitted_paint_batches()`.
5. Single new commit.

---

## Task 5: Synthetic burst test + stats accessor

**Goal:** Add an integration test that creates 100 pixmaps of `(32, 32, depth=24)`, frees them, re-creates 100, and asserts the pool absorbed the burst (pool hits dominate after the first 32). Validates the implementation under controlled conditions without needing the full adapta/mate-cc workload.

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs` (add `pixmap_pool_stats()` accessor for tests)
- New: `crates/yserver/tests/pixmap_pool_burst.rs` OR a `#[cfg(test)]` test inside an existing integration test binary

### Step 1: Stats accessor + test-only retire helper

- [ ] **Step 1: Plumb test-only accessors**

Per the codex P1 round-1 finding: a `#[cfg(test)]` `impl` block on `KmsBackend` is NOT visible to integration tests under `crates/yserver/tests/...` (those compile as separate crates and only see `pub` items). Two viable patterns:

- **Pattern A — `pub` (no cfg gating)**: add a permanent `pub fn pixmap_pool_stats(&self) -> Option<PixmapPoolStats>` and `pub fn force_retire_in_flight_for_test(&mut self) -> Result<(), BatchError>` on `KmsBackend`. The stats accessor is a read-only snapshot; the force-retire helper is named `_for_test` and documented as test-only. Risk: pollutes the public API surface.
- **Pattern B — feature flag**: gate behind `#[cfg(any(test, feature = "test-helpers"))]`. Cleaner but requires Cargo.toml `[features]` entry.
- **Pattern C — in-crate unit test**: put the burst test as a `#[cfg(test)] mod` inside `crates/yserver/src/kms/backend.rs` (or a sibling file). Sees `pub(crate)` and `#[cfg(test)]` items. Limitation: needs the harness setup (real VkContext) inline.

**Recommended**: Pattern A. The two methods are stable enough to expose. `pixmap_pool_stats` is genuinely useful telemetry (could ship in a debug HUD); `force_retire_in_flight_for_test`'s `_for_test` suffix flags intent. Document both as "test / introspection only" in their doc comments.

```rust
impl KmsBackend {
    /// Snapshot of `PixmapPool` stats. Returns `None` if the pool
    /// was never initialized (vk absent). Test / introspection only.
    #[must_use]
    pub fn pixmap_pool_stats(&self) -> Option<crate::kms::vk::pixmap_pool::PixmapPoolStats> {
        self.pixmap_pool.as_ref().map(|p| p.stats())
    }

    /// Close the currently-open paint batch (if any) AND drain
    /// every in-flight submitted batch. Test-only — production
    /// code uses `poll_in_flight` + composite ticks. The
    /// `_for_test` suffix is the contract; do not call from
    /// production paths.
    ///
    /// Codex round-1 P1: a simple "drain submitted batches" is
    /// insufficient because `free_pixmap`'s defer-release adopts
    /// into the currently-open batch, which won't be in
    /// `submitted_paint_batches` until something closes it.
    /// This helper closes first, then drains.
    pub fn force_retire_in_flight_for_test(
        &mut self,
    ) -> Result<(), crate::kms::scheduler::paint_batch::BatchError> {
        // Close + submit-async whatever is open (Idle is a no-op).
        self.scheduler.close_and_submit_async(Vec::new())?;
        // Drain every submitted batch, blocking on each fence.
        self.scheduler.drain_submitted_paint_batches()?;
        Ok(())
    }
}
```

### Step 2: Burst test

- [ ] **Step 2: Add a `#[cfg(test)] mod tests` burst test inside `backend.rs` (or a sibling lib module)**

```rust
#[cfg(test)]
mod pixmap_pool_burst_tests {
    use super::*;

    // Requires a real VkContext. Gate on env var so the test
    // skips cleanly in CI sandboxes without GPU access.
    fn vulkan_available() -> bool {
        std::env::var_os("YSERVER_TEST_VULKAN").is_some()
    }

    #[test]
    fn burst_of_100_32x32_pixmaps_hits_pool() {
        if !vulkan_available() {
            eprintln!("skipping: YSERVER_TEST_VULKAN not set");
            return;
        }
        let mut backend = KmsBackend::new_for_test(/* harness args */);
        let n: u64 = 100;

        // Round 1: fresh allocations only (pool empty).
        let pixmaps: Vec<u32> = (0..n)
            .map(|_| backend.create_pixmap(None, 24, 32, 32).unwrap().as_raw())
            .collect();
        let s1 = backend.pixmap_pool_stats().unwrap();
        assert_eq!(s1.total_takes_hit, 0);
        assert_eq!(s1.total_takes_miss, n);

        // Free all → defer-release into the open batch.
        for xid in pixmaps {
            backend.free_pixmap(None, xid).unwrap();
        }

        // Force the open batch to close + retire so the
        // PooledPixmapReturn BatchResources release into the pool.
        backend.force_retire_in_flight_for_test().unwrap();

        let s2 = backend.pixmap_pool_stats().unwrap();
        let bucket_cap = crate::kms::vk::pixmap_pool::PIXMAP_POOL_BUCKET_CAP as u64;
        assert_eq!(
            s2.total_returns_accepted, bucket_cap,
            "expected {bucket_cap} returns accepted (cap); got {}",
            s2.total_returns_accepted
        );
        assert_eq!(
            s2.total_returns_rejected_bucket_full, n - bucket_cap,
            "expected {} returns rejected (cap full); got {}",
            n - bucket_cap, s2.total_returns_rejected_bucket_full
        );

        // Round 2: re-create N. First `bucket_cap` should be pool
        // hits; rest fresh.
        for _ in 0..n {
            backend.create_pixmap(None, 24, 32, 32).unwrap();
        }
        let s3 = backend.pixmap_pool_stats().unwrap();
        assert_eq!(s3.total_takes_hit, bucket_cap);
        assert_eq!(s3.total_takes_miss, n + (n - bucket_cap));
    }
}
```

**Important notes for the implementer**:
- The existing test harness already provides `KmsBackend::for_tests_with_vk()` (see `crates/yserver/src/kms/backend.rs:~1501` and `crates/yserver/tests/common/server_fixture.rs:~48`). Use that constructor — don't add a parallel `new_for_test`.
- The `YSERVER_TEST_VULKAN` env-var gate keeps the test skippable in CI sandboxes that lack a real GPU. The local + virtme-ng harness sets it.
- If the in-crate `#[cfg(test) mod]` path doesn't work cleanly with `for_tests_with_vk()`'s visibility, fall back to an integration test under `crates/yserver/tests/` using the existing `server_fixture` harness; the assertion shape above stays identical.

### Step 3: Gates + commit

- [ ] **Step 3: Validate**

Run the new test. Document expected pass shape in the commit message.

Commit message:

```text
test(kms): synthetic pixmap-pool burst test (pixmap-pool T5)

Creates 100 32x32 depth-24 pixmaps, frees them, re-creates 100.
Asserts the pool absorbed at least PIXMAP_POOL_BUCKET_CAP (32) of
the second-round allocations.

Pool stats accessor pixmap_pool_stats() is test-only (cfg(test)
on KmsBackend impl).
```

### Done conditions for T5

1. New test passes.
2. `cargo +nightly fmt --check` clean.
3. `cargo clippy -p yserver --tests` clean.
4. Single new commit.

---

## Task 6: Results doc + status update

**Goal:** Write `docs/superpowers/plans/2026-05-14-pixmap-allocation-pool-results.md`. Mirror the Phase 4/5 results-doc shape. Move the pixmap-pool item from "Remaining" to "Done" in `docs/status.md`.

**Files:**
- Create: `docs/superpowers/plans/2026-05-14-pixmap-allocation-pool-results.md`
- Modify: `docs/status.md`

### Step 1: Results doc

- [ ] **Step 1: Write results**

Sections: Scope landed, preflight checks, cutover greps (FreePixmap flush gone, allocate_pixmap_mirror try_take, pool drain hooked), Done conditions table, Hardware smoke results (TBD for user-owned), Plan bugs caught (codex rounds), Commit summary table, Known deferred items, What's next.

### Step 2: Status doc

- [ ] **Step 2: Edit status.md**

Move the pixmap-pool entry from "Remaining" to "Done". Update commit SHAs. The Phase 6 item promotes to next priority.

### Step 3: Commit

- [ ] **Step 3: Commit**

```text
docs(plans): pixmap-allocation pool validation results
```

### Done conditions for T6

1. Results doc exists.
2. status.md reflects pool done.
3. Single commit.

---

## Phase-level Done conditions

1. `cargo +nightly fmt --check`, `cargo clippy -p yserver`, `cargo test --workspace` all green.
2. `crates/yserver/src/kms/vk/pixmap_pool.rs` exists.
3. `flush_if_needed(BatchFlushReason::ProtocolBarrier)` is NOT in `free_pixmap`'s body.
4. `allocate_pixmap_mirror` consults `self.pixmap_pool.as_ref().and_then(|p| p.try_take(...))` before `new_server_owned_pixmap`.
5. `PixmapPool::drain` is called in the shutdown sequence after `scheduler.drain_submitted_paint_batches`.
6. Synthetic burst test passes.
7. `docs/status.md` reflects pool done; results doc exists.
8. Hardware smoke on bee + fuji: mate-cc launcher first-paint and adapta-nokto apply should be noticeably less laggy (the load-bearing user-observed validation).

---

## Smoke plan (T6 hardware section)

Hardware smoke is user-owned. Three checks:

### Check 1 — bee (RDNA2 + Arch) under adapta-nokto + mate-cc

The load-bearing test. Apply adapta-nokto theme with mate-cc visible. Pre-pool: catastrophic lag (per `project_amd_lag_investigation.md`). Post-pool: should be smooth or near-smooth.

### Check 2 — fuji (Intel + Arch)

Cross-vendor confirmation. mate-cc launcher first-paint should be quick. Adapta-nokto theme apply should be quick. Pre-pool: slow. Post-pool: noticeable improvement expected.

### Check 3 — non-regression smoke

Standard `just yserver-mate-hw-release` session — no rendering corruption, no leaked pool entries at shutdown (`RUST_LOG=info` startup-then-quit cycle).

### Check 4 — rendercheck regression

`just rendercheck-yserver` — no regressions vs the Phase 5 baseline.

---

## Codex review checkpoints

Per the Phase 4/5 pattern: codex review after each task's commit; fold P0/P1 findings as fix-up commits.

Particular focus areas:

- **T1**: `Arc<RefCell<PixmapPool>>` lifetime safety under single-threaded invariant. `PooledPixmapReturn` BatchResource Drop semantics in Poisoned-batch leak path (matches existing BatchResource leak contract).
- **T2**: the synchronous-flush removal — same correctness argument as Phase 5 scratch-grow defer-release. The mirror's CB references are bounded by the open paint batch's submission; defer-release ensures release waits for the batch's fence.
- **T3**: pool-hit short-circuit must not skip any layout-tracking invariant. Verify that a SHADER_READ_ONLY_OPTIMAL → TRANSFER_DST_OPTIMAL transition on the first upload is identical to a fresh-alloc UNDEFINED → TRANSFER_DST_OPTIMAL transition.
- **T4**: shutdown ordering. `Arc::strong_count` should reach 1 at `drain()` time.
- **T5**: test isolation — the synthetic test must not depend on other tests' pool state. Force a fresh `PixmapPool` per test if needed.

---

## Glossary

- **`PixmapPool`**: backend-owned recycling pool for server-owned X pixmap `VkImage`s.
- **`PixmapPoolKey`**: `(width, height, format)` tuple. Bucket key.
- **`PooledPixmapImage`**: one recycled (image, view, memory, current_layout) entry.
- **`PooledPixmapReturn`**: `BatchResource` impl that returns an entry to the pool on `release` (or destroys if bucket full).
- **`MAX_POOLED_DIM`**: pixmaps above this size skip the pool.
- **`PIXMAP_POOL_BUCKET_CAP`**: max entries per bucket.
