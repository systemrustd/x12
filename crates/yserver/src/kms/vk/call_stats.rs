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
