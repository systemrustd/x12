//! Per-Vulkan-call rate counters for diagnosing driver-side cost.
//!
//! Background: a `perf record --call-graph fp` profile of bee/RDNA2
//! under a 2-window MATE drag showed 42.86% of CPU in
//! `libvulkan_radeon.so` (Mesa RADV) vs 25.54% in yserver itself.
//! The bottleneck is the rate at which yserver issues Vulkan API
//! calls — each `cmdDraw`, `cmdPipelineBarrier2`, `cmdBindPipeline`,
//! etc. triggers real driver work in RADV (state validation,
//! descriptor table prep, command-stream encoding) even when the
//! call itself is "cheap" individually.
//!
//! Mesa's debug symbols aren't fetchable through Arch's debuginfod,
//! so we can't see WHICH RADV function is hot. This module
//! instruments OUR side instead: every counted Vulkan API call
//! increments an atomic counter. Emit a per-second rollup from
//! `LoopTelemetry::maybe_emit` (gated on `YSERVER_LOOP_TELEMETRY=1`)
//! to see which categories of call drive the call rate.
//!
//! Once we know e.g. that `cmd_pipeline_barrier2` is firing at 30k/s,
//! we can decide if per-op barriers should be coarser per-batch, or
//! if consecutive RENDER Composite ops should batch into one draw
//! with no rebinds.
//!
//! Cost when enabled: one relaxed atomic-add per Vulkan call. Cost
//! when disabled (no telemetry env var): the counters still
//! increment but the emit is suppressed — call sites pay the
//! atomic-add unconditionally. Acceptable; an uncontended
//! `fetch_add(1, Relaxed)` is ~1ns on x86_64.

use std::sync::atomic::{AtomicU64, Ordering};

/// All counters reset to zero each time `snapshot_and_reset` is
/// called. Categories cover the high-volume calls we'd suspect
/// dominate the RADV CPU time during heavy drag traffic.
pub struct VkCallStats {
    pub cmd_pipeline_barrier2: AtomicU64,
    pub cmd_draw: AtomicU64,
    pub cmd_bind_pipeline: AtomicU64,
    pub cmd_bind_descriptor_sets: AtomicU64,
    pub cmd_push_constants: AtomicU64,
    pub cmd_set_viewport: AtomicU64,
    pub cmd_set_scissor: AtomicU64,
    pub cmd_begin_rendering: AtomicU64,
    pub cmd_end_rendering: AtomicU64,
    pub cmd_copy_buffer_to_image: AtomicU64,
    pub cmd_copy_image: AtomicU64,
    pub cmd_copy_image_to_buffer: AtomicU64,
    pub cmd_clear_color_image: AtomicU64,
    pub queue_submit2: AtomicU64,
    pub begin_command_buffer: AtomicU64,
    pub end_command_buffer: AtomicU64,

    // Per-source submit attribution (the per-second emit subdivides
    // the otherwise-opaque queue_submit2=N count). Sum of all submit_*
    // counters should approximately equal queue_submit2 (within ~1 of
    // noise from samples crossing the second boundary).
    /// flush_if_needed(VisibleComposite) — top of composite_and_flip.
    /// Expected ~ refresh rate (60 Hz typical).
    pub submit_visible_composite: AtomicU64,
    /// flush_if_needed(Readback). Synchronous; CPU needs pixels back.
    pub submit_readback: AtomicU64,
    /// flush_if_needed(ExternalSync). DRI3 Present fence, SYNC ext.
    pub submit_external_sync: AtomicU64,
    /// flush_if_needed(ProtocolBarrier). Drawable destruction +
    /// pre-resize-flush gates + Phase-3B run_legacy_paint_op wrapper.
    pub submit_protocol_barrier: AtomicU64,
    pub submit_size_limit: AtomicU64,
    pub submit_latency_limit: AtomicU64,
    pub submit_shutdown: AtomicU64,
    /// run_one_shot_op-driven submits. Each gradient creation,
    /// glyph atlas intern, readback recorder, scanout dump, and
    /// legacy paint-op funnels here. Expected hot if GTK creates
    /// many gradients / glyphs per frame.
    pub submit_one_shot: AtomicU64,
    /// compositor's record_and_present_composite submit. Drives
    /// the actual scanout draw + pageflip. Expected ~ refresh rate.
    pub submit_compositor: AtomicU64,
    /// Fallback bucket for any submit that doesn't get categorised
    /// above. Should be near-zero; any non-zero is a missed
    /// instrumentation site.
    pub submit_other: AtomicU64,

    // ProtocolBarrier subdivision — eight call sites in backend.rs
    // all use flush_if_needed(ProtocolBarrier). Per-site counts tell
    // us which path drives the protocol_barrier=N rollup.
    /// backend.rs ~9663 — drawable / window destruction barrier.
    pub pb_drawable_destroy: AtomicU64,
    /// backend.rs ~9843 — window resize pre-flush gate.
    pub pb_window_resize: AtomicU64,
    /// backend.rs ~10193 — image dealloc fallback (missing defer
    /// prereqs path).
    pub pb_image_dealloc_fallback: AtomicU64,
    /// backend.rs ~10215 — imported dma-buf release before Drop.
    pub pb_dmabuf_release: AtomicU64,
    /// backend.rs ~11699 — Picture destruction (RENDER FreePicture).
    pub pb_picture_destroy: AtomicU64,
    /// backend.rs ~12132 — render_create_linear_gradient pre-build.
    pub pb_gradient_linear: AtomicU64,
    /// backend.rs ~12207 — render_create_radial_gradient pre-build.
    pub pb_gradient_radial: AtomicU64,
    /// backend.rs ~12293 — cursor set_picture_cursor pre-flush.
    pub pb_cursor_picture: AtomicU64,

    // submit_other subdivision — three callers of
    // DrawableImage::initialize_clear; the existing submit_other
    // counter is the sum of these.
    /// backend.rs ~3186 — cursor mirror init clear.
    pub init_clear_cursor: AtomicU64,
    /// backend.rs ~7319 — window mirror init clear.
    pub init_clear_window: AtomicU64,
    /// backend.rs ~7388 — pixmap mirror init clear.
    pub init_clear_pixmap: AtomicU64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct VkCallStatsSnapshot {
    pub cmd_pipeline_barrier2: u64,
    pub cmd_draw: u64,
    pub cmd_bind_pipeline: u64,
    pub cmd_bind_descriptor_sets: u64,
    pub cmd_push_constants: u64,
    pub cmd_set_viewport: u64,
    pub cmd_set_scissor: u64,
    pub cmd_begin_rendering: u64,
    pub cmd_end_rendering: u64,
    pub cmd_copy_buffer_to_image: u64,
    pub cmd_copy_image: u64,
    pub cmd_copy_image_to_buffer: u64,
    pub cmd_clear_color_image: u64,
    pub queue_submit2: u64,
    pub begin_command_buffer: u64,
    pub end_command_buffer: u64,
    pub submit_visible_composite: u64,
    pub submit_readback: u64,
    pub submit_external_sync: u64,
    pub submit_protocol_barrier: u64,
    pub submit_size_limit: u64,
    pub submit_latency_limit: u64,
    pub submit_shutdown: u64,
    pub submit_one_shot: u64,
    pub submit_compositor: u64,
    pub submit_other: u64,
    pub pb_drawable_destroy: u64,
    pub pb_window_resize: u64,
    pub pb_image_dealloc_fallback: u64,
    pub pb_dmabuf_release: u64,
    pub pb_picture_destroy: u64,
    pub pb_gradient_linear: u64,
    pub pb_gradient_radial: u64,
    pub pb_cursor_picture: u64,
    pub init_clear_cursor: u64,
    pub init_clear_window: u64,
    pub init_clear_pixmap: u64,
}

pub static VK_CALLS: VkCallStats = VkCallStats {
    cmd_pipeline_barrier2: AtomicU64::new(0),
    cmd_draw: AtomicU64::new(0),
    cmd_bind_pipeline: AtomicU64::new(0),
    cmd_bind_descriptor_sets: AtomicU64::new(0),
    cmd_push_constants: AtomicU64::new(0),
    cmd_set_viewport: AtomicU64::new(0),
    cmd_set_scissor: AtomicU64::new(0),
    cmd_begin_rendering: AtomicU64::new(0),
    cmd_end_rendering: AtomicU64::new(0),
    cmd_copy_buffer_to_image: AtomicU64::new(0),
    cmd_copy_image: AtomicU64::new(0),
    cmd_copy_image_to_buffer: AtomicU64::new(0),
    cmd_clear_color_image: AtomicU64::new(0),
    queue_submit2: AtomicU64::new(0),
    begin_command_buffer: AtomicU64::new(0),
    end_command_buffer: AtomicU64::new(0),
    submit_visible_composite: AtomicU64::new(0),
    submit_readback: AtomicU64::new(0),
    submit_external_sync: AtomicU64::new(0),
    submit_protocol_barrier: AtomicU64::new(0),
    submit_size_limit: AtomicU64::new(0),
    submit_latency_limit: AtomicU64::new(0),
    submit_shutdown: AtomicU64::new(0),
    submit_one_shot: AtomicU64::new(0),
    submit_compositor: AtomicU64::new(0),
    submit_other: AtomicU64::new(0),
    pb_drawable_destroy: AtomicU64::new(0),
    pb_window_resize: AtomicU64::new(0),
    pb_image_dealloc_fallback: AtomicU64::new(0),
    pb_dmabuf_release: AtomicU64::new(0),
    pb_picture_destroy: AtomicU64::new(0),
    pb_gradient_linear: AtomicU64::new(0),
    pb_gradient_radial: AtomicU64::new(0),
    pb_cursor_picture: AtomicU64::new(0),
    init_clear_cursor: AtomicU64::new(0),
    init_clear_window: AtomicU64::new(0),
    init_clear_pixmap: AtomicU64::new(0),
};

impl VkCallStats {
    /// Snapshot every counter and reset to zero for the next window.
    /// Called from `LoopTelemetry::maybe_emit` once per emit period.
    #[must_use]
    pub fn snapshot_and_reset(&self) -> VkCallStatsSnapshot {
        VkCallStatsSnapshot {
            cmd_pipeline_barrier2: self.cmd_pipeline_barrier2.swap(0, Ordering::Relaxed),
            cmd_draw: self.cmd_draw.swap(0, Ordering::Relaxed),
            cmd_bind_pipeline: self.cmd_bind_pipeline.swap(0, Ordering::Relaxed),
            cmd_bind_descriptor_sets: self.cmd_bind_descriptor_sets.swap(0, Ordering::Relaxed),
            cmd_push_constants: self.cmd_push_constants.swap(0, Ordering::Relaxed),
            cmd_set_viewport: self.cmd_set_viewport.swap(0, Ordering::Relaxed),
            cmd_set_scissor: self.cmd_set_scissor.swap(0, Ordering::Relaxed),
            cmd_begin_rendering: self.cmd_begin_rendering.swap(0, Ordering::Relaxed),
            cmd_end_rendering: self.cmd_end_rendering.swap(0, Ordering::Relaxed),
            cmd_copy_buffer_to_image: self.cmd_copy_buffer_to_image.swap(0, Ordering::Relaxed),
            cmd_copy_image: self.cmd_copy_image.swap(0, Ordering::Relaxed),
            cmd_copy_image_to_buffer: self.cmd_copy_image_to_buffer.swap(0, Ordering::Relaxed),
            cmd_clear_color_image: self.cmd_clear_color_image.swap(0, Ordering::Relaxed),
            queue_submit2: self.queue_submit2.swap(0, Ordering::Relaxed),
            begin_command_buffer: self.begin_command_buffer.swap(0, Ordering::Relaxed),
            end_command_buffer: self.end_command_buffer.swap(0, Ordering::Relaxed),
            submit_visible_composite: self.submit_visible_composite.swap(0, Ordering::Relaxed),
            submit_readback: self.submit_readback.swap(0, Ordering::Relaxed),
            submit_external_sync: self.submit_external_sync.swap(0, Ordering::Relaxed),
            submit_protocol_barrier: self.submit_protocol_barrier.swap(0, Ordering::Relaxed),
            submit_size_limit: self.submit_size_limit.swap(0, Ordering::Relaxed),
            submit_latency_limit: self.submit_latency_limit.swap(0, Ordering::Relaxed),
            submit_shutdown: self.submit_shutdown.swap(0, Ordering::Relaxed),
            submit_one_shot: self.submit_one_shot.swap(0, Ordering::Relaxed),
            submit_compositor: self.submit_compositor.swap(0, Ordering::Relaxed),
            submit_other: self.submit_other.swap(0, Ordering::Relaxed),
            pb_drawable_destroy: self.pb_drawable_destroy.swap(0, Ordering::Relaxed),
            pb_window_resize: self.pb_window_resize.swap(0, Ordering::Relaxed),
            pb_image_dealloc_fallback: self.pb_image_dealloc_fallback.swap(0, Ordering::Relaxed),
            pb_dmabuf_release: self.pb_dmabuf_release.swap(0, Ordering::Relaxed),
            pb_picture_destroy: self.pb_picture_destroy.swap(0, Ordering::Relaxed),
            pb_gradient_linear: self.pb_gradient_linear.swap(0, Ordering::Relaxed),
            pb_gradient_radial: self.pb_gradient_radial.swap(0, Ordering::Relaxed),
            pb_cursor_picture: self.pb_cursor_picture.swap(0, Ordering::Relaxed),
            init_clear_cursor: self.init_clear_cursor.swap(0, Ordering::Relaxed),
            init_clear_window: self.init_clear_window.swap(0, Ordering::Relaxed),
            init_clear_pixmap: self.init_clear_pixmap.swap(0, Ordering::Relaxed),
        }
    }
}

/// Convenience macro: increment a single counter by one. Cheaper to
/// read at call sites than the full path each time.
///
/// Usage:
/// ```ignore
/// vk_count!(cmd_pipeline_barrier2);
/// unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };
/// ```
#[macro_export]
macro_rules! vk_count {
    ($field:ident) => {
        $crate::kms::vk::call_stats::VK_CALLS
            .$field
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    };
}
