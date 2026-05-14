//! Pixmap-pool synthetic burst test (pixmap-pool plan T5).
//!
//! Validates the implementation under controlled conditions without
//! needing the full adapta-nokto / mate-cc workload that motivated
//! the pool:
//!
//!  1. Create 100 32x32 depth-24 pixmaps via `Backend::create_pixmap`.
//!     The pool is empty, so all 100 takes must miss.
//!  2. Free all 100 via `Backend::free_pixmap` — each adopts the
//!     mirror as a `PooledPixmapReturn` BatchResource on the
//!     currently-open paint batch (defer-release).
//!  3. Force the open batch closed + drain in-flight batches via
//!     `force_retire_in_flight_for_test`. After this returns, every
//!     `PooledPixmapReturn::release` has run: the first
//!     `PIXMAP_POOL_BUCKET_CAP` (32) entries return into the bucket;
//!     the remaining 68 are rejected as the bucket is at cap.
//!  4. Re-create 100 32x32 depth-24 pixmaps. The first 32 hit the
//!     pool; the remaining 68 are fresh allocations.
//!
//! Gated on the `YSERVER_TEST_VULKAN` env var so CI sandboxes
//! without a Vulkan ICD skip cleanly. The local + virtme-ng harness
//! sets it.
//!
//! See `docs/superpowers/plans/2026-05-14-pixmap-allocation-pool.md`
//! Task 5.

#![cfg(target_os = "linux")]

use yserver::kms::{KmsBackend, vk::pixmap_pool::PIXMAP_POOL_BUCKET_CAP};
use yserver_core::backend::Backend;

fn vulkan_available() -> bool {
    std::env::var_os("YSERVER_TEST_VULKAN").is_some()
}

#[test]
fn burst_of_100_32x32_pixmaps_hits_pool() {
    if !vulkan_available() {
        eprintln!(
            "skipping pixmap_pool burst test: YSERVER_TEST_VULKAN not set \
             (no Vulkan ICD in this sandbox)"
        );
        return;
    }

    let mut backend = match KmsBackend::for_tests_with_vk() {
        Ok(b) => b,
        Err(err) => {
            // YSERVER_TEST_VULKAN was set but Vulkan init still
            // failed; surface the underlying cause so the harness
            // operator can fix their ICD instead of silently
            // skipping.
            panic!("for_tests_with_vk() failed despite YSERVER_TEST_VULKAN set: {err}");
        }
    };

    let n: u64 = 100;
    let bucket_cap = PIXMAP_POOL_BUCKET_CAP as u64;
    assert!(
        n > bucket_cap,
        "test invariant: n ({n}) must exceed bucket cap ({bucket_cap}) so reject-counter is exercised"
    );

    // ── Round 1: fresh allocations only (pool empty). ───────────────
    let mut pixmaps: Vec<u32> = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let h = backend
            .create_pixmap(None, 24, 32, 32)
            .expect("create_pixmap round 1");
        pixmaps.push(h.as_raw());
    }

    let s1 = backend
        .pixmap_pool_stats()
        .expect("pixmap_pool present (for_tests_with_vk attaches it)");
    assert_eq!(
        s1.total_takes_hit, 0,
        "round 1: pool starts empty, no hits expected"
    );
    assert_eq!(
        s1.total_takes_miss, n,
        "round 1: expected {n} misses, got {}",
        s1.total_takes_miss
    );

    // ── Free all → defer-release into the open paint batch. ─────────
    for xid in pixmaps {
        backend.free_pixmap(None, xid).expect("free_pixmap");
    }

    // Closes the open batch + drains submitted batches so every
    // PooledPixmapReturn::release has run.
    backend
        .force_retire_in_flight_for_test()
        .expect("force_retire_in_flight_for_test");

    let s2 = backend.pixmap_pool_stats().expect("pixmap_pool present");
    assert_eq!(
        s2.total_returns_accepted, bucket_cap,
        "expected {bucket_cap} returns accepted (bucket cap); got {}",
        s2.total_returns_accepted
    );
    assert_eq!(
        s2.total_returns_rejected_bucket_full,
        n - bucket_cap,
        "expected {} returns rejected (bucket full); got {}",
        n - bucket_cap,
        s2.total_returns_rejected_bucket_full
    );

    // ── Round 2: re-create N. First `bucket_cap` hit, rest fresh. ──
    for _ in 0..n {
        backend
            .create_pixmap(None, 24, 32, 32)
            .expect("create_pixmap round 2");
    }

    let s3 = backend.pixmap_pool_stats().expect("pixmap_pool present");
    assert_eq!(
        s3.total_takes_hit, bucket_cap,
        "round 2: expected {bucket_cap} pool hits, got {}",
        s3.total_takes_hit
    );
    // Miss counter is cumulative across both rounds: n (round 1) +
    // (n - bucket_cap) (round 2 misses after the pool is drained).
    assert_eq!(
        s3.total_takes_miss,
        n + (n - bucket_cap),
        "round 2: expected {} cumulative misses, got {}",
        n + (n - bucket_cap),
        s3.total_takes_miss
    );
}
