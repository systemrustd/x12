//! Shared scaffolding for `crates/yserver/tests/*.rs` integration tests.
//!
//! Each integration-test binary in this directory compiles `common`
//! independently and uses a different subset of its surface. The
//! `dead_code` allow is the standard Rust pattern for shared test
//! modules — each test crate's view of `common` is partial.

#![allow(dead_code)]

use std::os::fd::OwnedFd;
use yserver::kms::vk::device::VkContext;

/// Create an already-signaled `sync_file` fd by exporting a Vulkan binary
/// semaphore that was signaled via a signal-only `vkQueueSubmit2`.
///
/// The kernel's dma-buf IMPORT_SYNC_FILE ioctl requires a sync_file fd
/// representing a fence that is already signaled or will be signaled
/// eventually.  For tests we need one that is already done.
///
/// Panics if Vulkan operations fail — this is test scaffolding.
pub fn signaled_sync_file(vk: &VkContext) -> OwnedFd {
    use ash::vk;

    // 1. Create a binary semaphore with SYNC_FD export capability.
    let mut export_info = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let create_info = vk::SemaphoreCreateInfo::default().push_next(&mut export_info);
    let semaphore =
        unsafe { vk.device.create_semaphore(&create_info, None) }.expect("create export semaphore");

    // 2. Signal it via a signal-only vkQueueSubmit2 (no wait semaphores,
    //    no command buffers — only the signal semaphore info).
    let sig_info = [vk::SemaphoreSubmitInfo::default()
        .semaphore(semaphore)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
    let submit = [vk::SubmitInfo2::default().signal_semaphore_infos(&sig_info)];
    unsafe {
        vk.device
            .queue_submit2(vk.graphics_queue, &submit, vk::Fence::null())
    }
    .expect("signal-only queue_submit2");

    // Wait for the submit to complete so the semaphore payload is ready
    // to export.
    unsafe { vk.device.queue_wait_idle(vk.graphics_queue) }
        .expect("queue_wait_idle after signal submit");

    // 3. Export the signaled payload as a sync_file fd.
    let sync_fd = yserver::kms::vk::sync::export_sync_file(vk, semaphore)
        .expect("export_sync_file on signaled semaphore");

    // The semaphore payload has been exported (consumed by SYNC_FD
    // semantics); destroy the Vulkan handle.
    // SAFETY: we created `semaphore`, the device is alive, and the prior
    // queue_wait_idle guarantees no submission still references it.
    unsafe { vk.device.destroy_semaphore(semaphore, None) };

    sync_fd
}
