//! Per-second telemetry counters for v2 (Stage 2f).
//!
//! Per rendering-model-v2 spec §"Required counters / log lines"
//! and Stage 2 plan §"Acceptance discipline". The Stage-3+ perf
//! gates (no `vkQueueWaitIdle` on hot path; queue_submit2 rate
//! ≤ v1 baseline; damage_fraction noticeably <1.0 on window-drag;
//! sustained `full_redraw_fallback == 0`) are only enforceable
//! if v2 emits the named counters that let us judge them.
//!
//! Emission shape: per-second summary line under
//! `YSERVER_LOOP_TELEMETRY=1`, parsable by grep+awk. Each counter
//! is a simple monotonic accumulator; the per-second emitter
//! resets per-second-window state on each emit. Counter sites are
//! the [`Telemetry::record_*`] methods called by the engine,
//! scene, and platform layers.

#![allow(
    dead_code,
    reason = "Counter accessors are consumed by Stage 3+ perf gates + harness"
)]

use std::time::Instant;

use crate::kms::v2::submit_trace::{SubmitEvent, SubmitTrace};

/// Single-second accumulator. Reset on every emission tick.
#[derive(Debug, Default, Clone, Copy)]
pub struct Bucket {
    pub paint_submits: u64,
    pub composite_submits: u64,
    pub one_shot_submits: u64,
    pub queue_submit2: u64,
    pub vk_queue_wait_idle: u64,
    pub cpu_fence_wait_ns: u64,
    pub cpu_fence_wait_count: u64,
    pub damaged_pixels: u64,
    pub output_pixels: u64,
    pub scene_entries_visited: u64,
    pub scene_entries_drawn: u64,
    pub full_redraw_fallback: u64,
    pub storage_allocations: u64,
    pub descriptor_allocations: u64,
    pub image_view_creates: u64,
    pub frame_present_count: u64,
    pub missed_pageflips: u64,
    pub gpu_render_ns: u64,
    pub compose_cb_record_ns: u64,
    pub frames_with_compose: u64,
    // ── Stage 3a glyph/text counters ─────────────────────────
    /// Glyphs successfully interned into the atlas during the
    /// window. One `intern` call that returns `Some(entry)`
    /// without a cache hit increments this.
    pub atlas_intern: u64,
    /// Glyph upload CBs submitted to the graphics queue.
    /// Mirrors `atlas_intern` for the miss path; cache-hit
    /// interns don't touch this. Useful for distinguishing
    /// atlas-cache-hit vs miss rate.
    pub glyph_uploads: u64,
    /// Glyphs the run dropped because the atlas was full. Per
    /// Stage 3 plan § 3a: this is a **lifetime** counter only on
    /// the `Telemetry.lifetime` view; the bucket field exists for
    /// the maybe_emit log line but typically reads zero — the
    /// load-bearing fact is whether the value ever became
    /// non-zero, which the lifetime view captures.
    pub glyphs_dropped_atlas_full: u64,
    // ── Stage 3d RENDER glyph counters (wire sites land in 3d;
    //    Stage 3a only adds the storage). ────────────────────
    pub composite_glyphs_dropped_unsupported: u64,
    pub disjoint_readback_count: u64,
    /// Stage 5 Task 4 layer 1: vkCreateDescriptorPool calls in this
    /// second. Should reach a near-zero floor after warm-up under
    /// the descriptor-pool-ring design (spec 2026-05-21).
    pub descriptor_pool_creates: u64,
    /// Stage 5 Task 4 layer 1: vkResetDescriptorPool calls in this
    /// second. Tracks paint_submits/s / SETS_PER_POOL on a healthy
    /// recycle path.
    pub descriptor_pool_resets: u64,
    /// Stage 5 Task 3 POC: count of COW `copy_area` batches flushed
    /// (each maps to one `vkQueueSubmit2`). Per-second + lifetime.
    /// Replaces the per-call `paint_submits` increments that the
    /// pre-POC `copy_area`-to-COW path generated.
    pub cow_batches_flushed: u64,
    /// Stage 5 Task 3 POC: count of individual `copy_area` calls
    /// folded into batches. Per-second + lifetime. Pre-POC
    /// baseline would have produced this many separate
    /// `paint_submits`; `cow_batches_flushed` is the post-POC
    /// equivalent submit count. The ratio of the two is the
    /// average batch size.
    pub cow_copies_coalesced: u64,
    /// Stage 5 Task 3 (render-composite generalization): count of
    /// RENDER batches flushed (each maps to one `vkQueueSubmit2`).
    /// Per-second + lifetime. Parallel to `cow_batches_flushed`.
    pub render_batches_flushed: u64,
    /// Stage 5 Task 3 (render-composite generalization): count of
    /// individual `render_composite` calls folded into batches.
    /// Pre-fix each call would have produced its own `paint_submit`.
    pub render_composites_coalesced: u64,
}

/// v2 telemetry state. One per `KmsBackendV2`. Counter sites
/// call `record_*` directly; the emitter ticks once per second
/// from the core loop (driven through `maybe_emit`).
pub struct Telemetry {
    enabled: bool,
    last_emit: Instant,
    bucket: Bucket,
    /// Lifetime-aggregate counts (not reset per-emit). Useful
    /// for the acceptance harness which compares totals after
    /// driving a test sequence.
    pub lifetime: Bucket,
    /// Stage 5 Task 3 diagnostic: per-submit event log,
    /// enabled by `YSERVER_SUBMIT_TRACE=<path>`. `None` in the
    /// default off case (zero hot-path cost).
    submit_trace: Option<SubmitTrace>,
    /// Bumped at the top of `maybe_composite` (one tick = one
    /// frame_id). Every `record_submit_event` writes the
    /// current value, so paint events recorded between ticks
    /// share the surrounding tick's id.
    frame_id: u64,
}

impl Telemetry {
    /// Construct. Reads `YSERVER_LOOP_TELEMETRY` once at boot to
    /// decide whether the per-second emitter actually logs.
    /// Counter sites always update — the cost of an `+= 1` is
    /// trivial, and tests check `lifetime.*` regardless of the
    /// env var.
    #[must_use]
    pub(crate) fn new() -> Self {
        let enabled = matches!(
            std::env::var_os("YSERVER_LOOP_TELEMETRY")
                .as_deref()
                .and_then(|s| s.to_str()),
            Some("1" | "true" | "yes" | "on")
        );
        Self {
            enabled,
            last_emit: Instant::now(),
            bucket: Bucket::default(),
            lifetime: Bucket::default(),
            submit_trace: SubmitTrace::from_env(),
            frame_id: 0,
        }
    }

    /// Stage 5 Task 3 diagnostic: log one submit event to the
    /// trace file if `YSERVER_SUBMIT_TRACE` is set. No-op
    /// otherwise (and the wrapping `Option::None` lets the
    /// optimizer fold the call away on the default-off path).
    #[inline]
    pub(crate) fn record_submit_event(&mut self, mut event: SubmitEvent) {
        if let Some(trace) = self.submit_trace.as_mut() {
            event.frame_id = self.frame_id;
            trace.record(&event);
        }
    }

    /// Bumped once per main-loop tick (top of `maybe_composite`).
    /// All submit events recorded between calls share the
    /// current frame_id; the `scene_compose` event for tick N
    /// also carries id N.
    pub(crate) fn advance_frame(&mut self) {
        self.frame_id = self.frame_id.wrapping_add(1);
    }

    /// Current frame_id. Mainly exposed for tests + diagnostic
    /// log lines.
    #[must_use]
    pub(crate) fn frame_id(&self) -> u64 {
        self.frame_id
    }

    /// Tick the emitter; if ≥ 1s has elapsed, print the
    /// per-second summary and reset the bucket. Safe to call
    /// every core-loop iteration — no-op when below threshold.
    pub(crate) fn maybe_emit(&mut self) {
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let dt = now.duration_since(self.last_emit);
        if dt < std::time::Duration::from_secs(1) {
            return;
        }
        let b = self.bucket;
        let denom = b.frames_with_compose.max(1);
        let avg_compose_cb_ns = b.compose_cb_record_ns / denom;
        let avg_gpu_render_ns = b.gpu_render_ns / denom;
        let damage_fraction = if b.output_pixels > 0 {
            #[allow(clippy::cast_precision_loss)]
            (b.damaged_pixels as f64 / b.output_pixels as f64)
        } else {
            0.0
        };
        log::info!(
            "v2_telemetry: paint_submits/s={} composite_submits/s={} \
             one_shot_submits/s={} queue_submit2/s={} \
             vk_queue_wait_idle/s={} cpu_fence_wait_ns/s={} \
             cpu_fence_wait_count/s={} damage_fraction={damage_fraction:.3} \
             scene_entries_visited={} scene_entries_drawn={} \
             full_redraw_fallback/s={} storage_allocations/s={} \
             descriptor_allocations/s={} image_view_creates/s={} \
             frame_present_count/s={} missed_pageflips/s={} \
             atlas_intern/s={} glyph_uploads/s={} \
             glyphs_dropped_atlas_full(lifetime)={} \
             composite_glyphs_dropped_unsupported(lifetime)={} \
             disjoint_readback_count/s={} \
             descriptor_pool_creates/s={} descriptor_pool_resets/s={} \
             cow_batches_flushed/s={} cow_copies_coalesced/s={} \
             render_batches_flushed/s={} render_composites_coalesced/s={} \
             avg_gpu_render_ns={avg_gpu_render_ns} \
             avg_compose_cb_record_ns={avg_compose_cb_ns}",
            b.paint_submits,
            b.composite_submits,
            b.one_shot_submits,
            b.queue_submit2,
            b.vk_queue_wait_idle,
            b.cpu_fence_wait_ns,
            b.cpu_fence_wait_count,
            b.scene_entries_visited,
            b.scene_entries_drawn,
            b.full_redraw_fallback,
            b.storage_allocations,
            b.descriptor_allocations,
            b.image_view_creates,
            b.frame_present_count,
            b.missed_pageflips,
            b.atlas_intern,
            b.glyph_uploads,
            self.lifetime.glyphs_dropped_atlas_full,
            self.lifetime.composite_glyphs_dropped_unsupported,
            b.disjoint_readback_count,
            b.descriptor_pool_creates,
            b.descriptor_pool_resets,
            b.cow_batches_flushed,
            b.cow_copies_coalesced,
            b.render_batches_flushed,
            b.render_composites_coalesced,
        );
        self.bucket = Bucket::default();
        self.last_emit = now;
    }

    /// Whether emission is enabled. Tests use this to decide
    /// whether to assert lifetime counts.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    // ── Counter sites ───────────────────────────────────────────

    pub(crate) fn record_paint_submit(&mut self) {
        self.bucket.paint_submits += 1;
        self.bucket.queue_submit2 += 1;
        self.lifetime.paint_submits += 1;
        self.lifetime.queue_submit2 += 1;
    }

    pub(crate) fn record_composite_submit(&mut self) {
        self.bucket.composite_submits += 1;
        self.bucket.queue_submit2 += 1;
        self.bucket.frames_with_compose += 1;
        self.lifetime.composite_submits += 1;
        self.lifetime.queue_submit2 += 1;
        self.lifetime.frames_with_compose += 1;
    }

    pub(crate) fn record_one_shot_submit(&mut self) {
        self.bucket.one_shot_submits += 1;
        self.bucket.queue_submit2 += 1;
        self.lifetime.one_shot_submits += 1;
        self.lifetime.queue_submit2 += 1;
    }

    pub(crate) fn record_vk_queue_wait_idle(&mut self) {
        self.bucket.vk_queue_wait_idle += 1;
        self.lifetime.vk_queue_wait_idle += 1;
    }

    pub(crate) fn record_fence_wait(&mut self, ns: u64) {
        self.bucket.cpu_fence_wait_ns = self.bucket.cpu_fence_wait_ns.saturating_add(ns);
        self.bucket.cpu_fence_wait_count += 1;
        self.lifetime.cpu_fence_wait_ns = self.lifetime.cpu_fence_wait_ns.saturating_add(ns);
        self.lifetime.cpu_fence_wait_count += 1;
    }

    pub(crate) fn record_damage_pixels(&mut self, damaged: u64, output: u64) {
        self.bucket.damaged_pixels = self.bucket.damaged_pixels.saturating_add(damaged);
        self.bucket.output_pixels = self.bucket.output_pixels.saturating_add(output);
        self.lifetime.damaged_pixels = self.lifetime.damaged_pixels.saturating_add(damaged);
        self.lifetime.output_pixels = self.lifetime.output_pixels.saturating_add(output);
    }

    pub(crate) fn record_scene_entries(&mut self, visited: u64, drawn: u64) {
        self.bucket.scene_entries_visited =
            self.bucket.scene_entries_visited.saturating_add(visited);
        self.bucket.scene_entries_drawn = self.bucket.scene_entries_drawn.saturating_add(drawn);
        self.lifetime.scene_entries_visited =
            self.lifetime.scene_entries_visited.saturating_add(visited);
        self.lifetime.scene_entries_drawn = self.lifetime.scene_entries_drawn.saturating_add(drawn);
    }

    pub(crate) fn record_full_redraw_fallback(&mut self) {
        self.bucket.full_redraw_fallback += 1;
        self.lifetime.full_redraw_fallback += 1;
    }

    pub(crate) fn record_storage_allocation(&mut self) {
        self.bucket.storage_allocations += 1;
        self.lifetime.storage_allocations += 1;
    }

    pub(crate) fn record_descriptor_allocations(&mut self, n: u64) {
        self.bucket.descriptor_allocations = self.bucket.descriptor_allocations.saturating_add(n);
        self.lifetime.descriptor_allocations =
            self.lifetime.descriptor_allocations.saturating_add(n);
    }

    pub(crate) fn record_image_view_create(&mut self) {
        self.bucket.image_view_creates += 1;
        self.lifetime.image_view_creates += 1;
    }

    pub(crate) fn record_frame_present(&mut self) {
        self.bucket.frame_present_count += 1;
        self.lifetime.frame_present_count += 1;
    }

    pub(crate) fn record_missed_pageflip(&mut self) {
        self.bucket.missed_pageflips += 1;
        self.lifetime.missed_pageflips += 1;
    }

    pub(crate) fn record_compose_cb_record_ns(&mut self, ns: u64) {
        self.bucket.compose_cb_record_ns = self.bucket.compose_cb_record_ns.saturating_add(ns);
        self.lifetime.compose_cb_record_ns = self.lifetime.compose_cb_record_ns.saturating_add(ns);
    }

    // ── Stage 3a counter sites ──────────────────────────────────

    /// Bumped on every successful `intern` that pushed a new
    /// entry (cache miss). Cache hits do NOT increment this.
    pub(crate) fn record_atlas_intern(&mut self) {
        self.bucket.atlas_intern += 1;
        self.lifetime.atlas_intern += 1;
    }

    /// Bumped on every glyph upload CB submitted to the queue.
    /// Today matches `atlas_intern` 1:1 (per-glyph upload); kept
    /// distinct so Stage 5's batched-upload work can keep the
    /// intern rate honest while collapsing `glyph_uploads`.
    pub(crate) fn record_glyph_upload(&mut self) {
        self.bucket.glyph_uploads += 1;
        self.bucket.queue_submit2 += 1;
        self.lifetime.glyph_uploads += 1;
        self.lifetime.queue_submit2 += 1;
    }

    /// Bumped when a text run skips a glyph because the atlas is
    /// full (post-eviction would refuse the pack). Lifetime
    /// counter only — Stage 3 plan §"Non-goals" §6: "drop the
    /// glyph + log once + counter."
    pub(crate) fn record_glyph_dropped_atlas_full(&mut self) {
        self.bucket.glyphs_dropped_atlas_full += 1;
        self.lifetime.glyphs_dropped_atlas_full += 1;
    }

    // ── Stage 3d counter sites (sites wired in 3d) ──────────────

    pub(crate) fn record_composite_glyphs_dropped_unsupported(&mut self) {
        self.bucket.composite_glyphs_dropped_unsupported += 1;
        self.lifetime.composite_glyphs_dropped_unsupported += 1;
    }

    pub(crate) fn record_disjoint_readback(&mut self) {
        self.bucket.disjoint_readback_count += 1;
        self.lifetime.disjoint_readback_count += 1;
    }

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
        self.bucket.descriptor_pool_resets = self.bucket.descriptor_pool_resets.saturating_add(n);
        self.lifetime.descriptor_pool_resets =
            self.lifetime.descriptor_pool_resets.saturating_add(n);
    }

    /// Stage 5 Task 3 POC: one cow batch flushed. Also bumps
    /// `paint_submits` + `queue_submit2` so the existing per-second
    /// rates stay accurate (each flush is one real `vkQueueSubmit2`).
    /// `coalesced` is the number of `copy_area` calls the batch
    /// folded.
    pub(crate) fn record_cow_batch_flushed(&mut self, coalesced: u32) {
        self.bucket.cow_batches_flushed += 1;
        self.bucket.cow_copies_coalesced = self
            .bucket
            .cow_copies_coalesced
            .saturating_add(u64::from(coalesced));
        self.bucket.paint_submits += 1;
        self.bucket.queue_submit2 += 1;
        self.lifetime.cow_batches_flushed += 1;
        self.lifetime.cow_copies_coalesced = self
            .lifetime
            .cow_copies_coalesced
            .saturating_add(u64::from(coalesced));
        self.lifetime.paint_submits += 1;
        self.lifetime.queue_submit2 += 1;
    }

    /// Stage 5 Task 3 (render-composite generalization): one
    /// render batch flushed. Mirrors `record_cow_batch_flushed`
    /// — bumps `paint_submits` + `queue_submit2` since each
    /// flush is one real `vkQueueSubmit2`.
    pub(crate) fn record_render_batch_flushed(&mut self, coalesced: u32) {
        self.bucket.render_batches_flushed += 1;
        self.bucket.render_composites_coalesced = self
            .bucket
            .render_composites_coalesced
            .saturating_add(u64::from(coalesced));
        self.bucket.paint_submits += 1;
        self.bucket.queue_submit2 += 1;
        self.lifetime.render_batches_flushed += 1;
        self.lifetime.render_composites_coalesced = self
            .lifetime
            .render_composites_coalesced
            .saturating_add(u64::from(coalesced));
        self.lifetime.paint_submits += 1;
        self.lifetime.queue_submit2 += 1;
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_in_bucket_and_lifetime() {
        let mut t = Telemetry::new();
        t.record_paint_submit();
        t.record_paint_submit();
        t.record_composite_submit();
        t.record_one_shot_submit();
        assert_eq!(t.lifetime.paint_submits, 2);
        assert_eq!(t.lifetime.composite_submits, 1);
        assert_eq!(t.lifetime.one_shot_submits, 1);
        // All three increment queue_submit2 too.
        assert_eq!(t.lifetime.queue_submit2, 4);
        assert_eq!(t.bucket.queue_submit2, 4);
    }

    #[test]
    fn fence_wait_aggregates_ns_and_count() {
        let mut t = Telemetry::new();
        t.record_fence_wait(1_000);
        t.record_fence_wait(2_500);
        assert_eq!(t.lifetime.cpu_fence_wait_ns, 3_500);
        assert_eq!(t.lifetime.cpu_fence_wait_count, 2);
    }

    #[test]
    fn maybe_emit_resets_bucket_after_log() {
        let mut t = Telemetry::new();
        t.record_paint_submit();
        t.bucket.compose_cb_record_ns = 100;
        // Simulate >1s elapsed by adjusting last_emit.
        t.last_emit = Instant::now() - std::time::Duration::from_secs(2);
        // Force enabled so emit body actually runs the reset
        // (logging is suppressed when env var unset, but bucket
        // reset still happens).
        t.enabled = true;
        t.maybe_emit();
        assert_eq!(t.bucket.paint_submits, 0);
        assert_eq!(t.bucket.compose_cb_record_ns, 0);
        // Lifetime preserved.
        assert_eq!(t.lifetime.paint_submits, 1);
    }

    #[test]
    fn vk_queue_wait_idle_counted_separately() {
        let mut t = Telemetry::new();
        t.record_vk_queue_wait_idle();
        t.record_vk_queue_wait_idle();
        assert_eq!(t.lifetime.vk_queue_wait_idle, 2);
        // Target zero in steady state — the gate is "lifetime
        // count stays at 0 except inside get_image".
    }

    #[test]
    fn atlas_counters_disjoint_from_paint_submits() {
        let mut t = Telemetry::new();
        t.record_atlas_intern();
        t.record_atlas_intern();
        t.record_glyph_upload();
        t.record_glyph_upload();
        t.record_glyph_dropped_atlas_full();
        // intern is logical (cache miss) — does NOT bump queue_submit2.
        assert_eq!(t.lifetime.atlas_intern, 2);
        // glyph upload bumps queue_submit2 (one CB per upload).
        assert_eq!(t.lifetime.glyph_uploads, 2);
        assert_eq!(t.lifetime.queue_submit2, 2);
        assert_eq!(t.lifetime.glyphs_dropped_atlas_full, 1);
        // No paint_submits side effect from atlas activity.
        assert_eq!(t.lifetime.paint_submits, 0);
    }

    #[test]
    fn stage3d_counters_lifetime_track() {
        let mut t = Telemetry::new();
        t.record_composite_glyphs_dropped_unsupported();
        t.record_disjoint_readback();
        t.record_disjoint_readback();
        assert_eq!(t.lifetime.composite_glyphs_dropped_unsupported, 1);
        assert_eq!(t.lifetime.disjoint_readback_count, 2);
    }

    #[test]
    fn descriptor_pool_counters_accumulate() {
        let mut t = Telemetry::new();
        t.record_descriptor_pool_create();
        t.record_descriptor_pool_create();
        t.record_descriptor_pool_reset(3);
        assert_eq!(t.lifetime.descriptor_pool_creates, 2);
        assert_eq!(t.lifetime.descriptor_pool_resets, 3);
    }
}
