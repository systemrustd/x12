//! Per-bo state machine + Vulkan-first scanout-bo allocation
//! (sub-phase 4.1.2).
//!
//! Spec: docs/superpowers/specs/2026-05-07-phase4-1-vulkan-compositor-design.md
//! §"Per-buffer release fence" — table of transitions and fence-handle
//! ownership rules.
//!
//! ## Allocation direction
//!
//! Vulkan-first: each [`ScanoutBo`] owns a `VkImage` allocated with
//! `VK_IMAGE_TILING_LINEAR` and bound to device memory carrying
//! `VkExportMemoryAllocateInfo(handleType=DMA_BUF)`. The memory is
//! exported as a dma-buf via `vkGetMemoryFdKHR`, the dma-buf is
//! imported into the DRM device via `PRIME_FD_TO_HANDLE`, and the
//! resulting GEM handle is registered as a DRM framebuffer with
//! `add_fb2` (no modifier flag — linear, untagged).
//!
//! Why Vulkan-first instead of GBM-first:
//! - Works on RADV gfx8/Polaris (no `VK_EXT_image_drm_format_modifier`
//!   needed; linear images don't need a modifier handle to communicate
//!   to KMS).
//! - Works through Mesa Venus (host-allocated memory wrapped as a
//!   virtgpu blob → guest dma-buf → guest virtio-gpu DRM. Same
//!   direction Mesa+Wayland use; the reverse direction
//!   `vkGetMemoryResourcePropertiesMESA`'d on a guest GBM bo aborts
//!   the Venus driver).
//! - One renderer; no driver-specific skip paths inside the bo
//!   constructor.

use std::{
    io,
    os::fd::{AsFd, FromRawFd, OwnedFd},
    sync::Arc,
};

use ash::vk;
use drm::{
    buffer::{DrmFourcc, DrmModifier, Handle as DrmBufferHandle, PlanarBuffer as DrmPlanarBuffer},
    control::{Device as DrmControlDevice, FbCmd2Flags, framebuffer},
};

use super::device::VkContext;

/// Per-bo phase. The lifecycle is roughly
/// `Free → Recording → Submitted → Pending → OnScreen → Retiring → Free`.
/// `Submitted` can also revert to `Recording` on atomic-EBUSY, or jump
/// to `Free` on modeset preempt.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub enum BoPhase {
    /// Not in flight; GPU may write into it. No fences attached.
    #[default]
    Free,
    /// Composite CB being recorded for this bo.
    Recording,
    /// `vkQueueSubmit2` issued; we still own the `IN_FENCE_FD` until
    /// the atomic commit either accepts (kernel consumes it) or
    /// rejects (we close it).
    Submitted,
    /// `drmModeAtomicCommit` accepted; `IN_FENCE_FD` ownership
    /// transferred to kernel; we now own the `OUT_FENCE_FD` (the
    /// release fence).
    Pending,
    /// Pageflip-complete arrived. Bo is on-screen. The release fence
    /// is signal-pending (KMS signals it when the next flip retires
    /// this bo).
    OnScreen,
    /// A later flip's pageflip-complete arrived; this bo is no
    /// longer on screen. Release fence is signalled. Returns to
    /// `Free` once all GPU readers (e.g. damage-diff sources)
    /// complete.
    Retiring,
}

/// Fence-fd handles + the current phase. Owns no DRM/Vulkan state
/// directly — callers thread the actual `VkImage` / framebuffer
/// alongside.
#[derive(Debug, Default)]
pub struct BoState {
    pub phase: BoPhase,
    /// Fence we exported from `vkGetSemaphoreFdKHR` after submit and
    /// will pass to KMS as `IN_FENCE_FD`. We own it until the kernel
    /// consumes it on atomic accept.
    pub in_fence_fd: Option<i32>,
    /// Fence the kernel allocated and handed back via `OUT_FENCE_PTR`.
    /// Signalled when the next flip retires this bo.
    pub release_fence_fd: Option<i32>,
}

impl BoState {
    /// `Free → Recording`: acquire for next frame's render target.
    pub fn transition_to_recording(&mut self) {
        debug_assert_eq!(self.phase, BoPhase::Free);
        self.phase = BoPhase::Recording;
    }

    /// `Recording → Submitted`: `vkQueueSubmit2` issued. Caller
    /// already exported `IN_FENCE_FD` and passes it in.
    pub fn transition_to_submitted(&mut self, in_fence_fd: i32) {
        debug_assert_eq!(self.phase, BoPhase::Recording);
        self.phase = BoPhase::Submitted;
        self.in_fence_fd = Some(in_fence_fd);
    }

    /// `Submitted → Pending`: atomic accepted. Returns the in-fence
    /// fd so the caller can close it (the kernel takes a reference
    /// to the underlying `sync_file` during the commit but does NOT
    /// own the fd — userspace must close it). Adopts the out-fence
    /// fd from KMS as the release fence.
    #[must_use = "in-fence fd must be closed by the caller"]
    pub fn transition_to_pending(&mut self, out_fence_fd: i32) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Submitted);
        self.phase = BoPhase::Pending;
        let in_fence = self.in_fence_fd.take();
        self.release_fence_fd = Some(out_fence_fd);
        in_fence
    }

    /// `Submitted → Recording`: atomic returned `-EBUSY`. Caller is
    /// responsible for closing the returned in-fence fd.
    pub fn transition_to_recording_after_atomic_reject(&mut self) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Submitted);
        self.phase = BoPhase::Recording;
        self.in_fence_fd.take()
    }

    /// `Submitted → Free`: modeset preempts (CRTC reconfigure
    /// mid-flight). Caller has already host-waited on the in-flight
    /// GPU work and must close the returned fd.
    pub fn transition_to_free_after_modeset_preempt(&mut self) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Submitted);
        self.phase = BoPhase::Free;
        self.in_fence_fd.take()
    }

    /// `Pending → OnScreen`: first pageflip-complete event for this
    /// bo arrived.
    pub fn transition_to_on_screen(&mut self) {
        debug_assert_eq!(self.phase, BoPhase::Pending);
        self.phase = BoPhase::OnScreen;
    }

    /// `OnScreen → Retiring`: next flip's pageflip-complete arrived.
    /// Release fence is now signal-pending (will be signalled by
    /// KMS).
    pub fn transition_to_retiring(&mut self) {
        debug_assert_eq!(self.phase, BoPhase::OnScreen);
        self.phase = BoPhase::Retiring;
    }

    /// `Retiring → Free`: all GPU readers are done; caller will close
    /// the returned release fence fd.
    pub fn transition_to_free_after_retire(&mut self) -> Option<i32> {
        debug_assert_eq!(self.phase, BoPhase::Retiring);
        self.phase = BoPhase::Free;
        self.release_fence_fd.take()
    }

    /// `any → Free` on modeset reset (hotunplug, mode change). Caller
    /// must close every returned fd. The two slots may both be
    /// populated if the bo was Submitted-then-immediately-Pending
    /// somehow; in normal flow only one is.
    pub fn transition_to_free_after_modeset_reset(&mut self) -> ModesetReleased {
        let in_fence = self.in_fence_fd.take();
        let release_fence = self.release_fence_fd.take();
        self.phase = BoPhase::Free;
        ModesetReleased {
            in_fence,
            release_fence,
        }
    }
}

/// Fences released when a bo is force-reset on modeset. Caller closes
/// each `Some(fd)` exactly once.
#[derive(Debug)]
pub struct ModesetReleased {
    pub in_fence: Option<i32>,
    pub release_fence: Option<i32>,
}

/// One scanout buffer object: a Vulkan-allocated `VkImage` exported
/// as a dma-buf and imported into the DRM device for KMS scanout.
///
/// All fields are populated after `allocate()` returns successfully.
/// Drop unwinds them in the right order (DRM framebuffer → GEM handle
/// close → VkImage → memory → semaphore → command pool).
#[allow(dead_code)] // most fields used by 4.1.2.5+ atomic-commit driver.
pub struct ScanoutBo {
    pub state: BoState,
    pub width: u32,
    pub height: u32,
    /// Row pitch in bytes — what the driver chose for our
    /// `TILING_LINEAR` image. Passed to KMS as `pitch[0]` and to the
    /// blit copy as the destination row stride.
    pub pitch: u32,
    pub vk_image: vk::Image,
    pub vk_memory: vk::DeviceMemory,
    /// Color image view bound by the composite pass's
    /// `vkCmdBeginRendering` as the color attachment. Lives as long
    /// as `vk_image`. Built lazily on first use to avoid forcing
    /// every PixmanShadow-only deployment to allocate a view it
    /// never reads.
    pub vk_image_view: vk::ImageView,
    /// Long-lived binary semaphore used as `signalSemaphore` on the
    /// per-frame composite submit. Its payload is exported as a
    /// SYNC_FD after every submit and handed to KMS as `IN_FENCE_FD`.
    /// Object reused for the bo's whole lifetime; only the fd
    /// payload churns.
    pub vk_semaphore: vk::Semaphore,
    /// DRM framebuffer registered against this bo's GEM handle.
    /// `Option` so Drop can take it.
    pub fb_handle: Option<framebuffer::Handle>,
    /// GEM handle from `PRIME_FD_TO_HANDLE`. Closed via `GEM_CLOSE`
    /// in Drop. `Option` so Drop can take it.
    pub gem_handle: Option<DrmBufferHandle>,
    /// Per-bo transfer resources: command pool + a single command
    /// buffer recycled across frames, a host-mapped staging buffer
    /// sized for the bo (XRGB8888 → 4 bytes × width × height), and
    /// the device memory backing it.
    pub vk_transfer: TransferResources,
    /// Shared DRM device handle (for un-registering the framebuffer
    /// + closing the GEM handle in Drop).
    drm: Arc<crate::drm::Device>,
    /// Held to keep image+memory destructors anchored to a live
    /// device. Cloned per bo from the pool's Arc so individual bos
    /// can be moved/dropped independently.
    vk: Arc<VkContext>,
}

/// Per-bo transfer-side resources (command pool/buffer + staging
/// buffer).
#[allow(dead_code)] // exercised by 4.1.2.5 atomic-commit driver.
pub struct TransferResources {
    pub command_pool: vk::CommandPool,
    pub command_buffer: vk::CommandBuffer,
    pub staging_buffer: vk::Buffer,
    pub staging_memory: vk::DeviceMemory,
    pub staging_mapped: std::ptr::NonNull<u8>,
    pub staging_size: u64,
}

// `NonNull<u8>` isn't `Send` by default. ScanoutBo is single-thread
// (KmsBackend is single-thread; Backend trait is used from one
// thread). Once 4.1.3+ adds threading, revisit.
unsafe impl Send for TransferResources {}
unsafe impl Sync for TransferResources {}

/// One pool per CRTC; holds N bos that rotate through the state
/// machine. Three is the documented sweet spot (design §2): one
/// scanning out, one queued, one being recorded into.
#[allow(dead_code)] // wired in via KmsBackend in a later commit.
pub struct ScanoutBoPool {
    pub bos: Vec<ScanoutBo>,
    pub width: u32,
    pub height: u32,
}

impl ScanoutBo {
    /// Allocate one Vulkan-first scanout bo: VkImage TILING_LINEAR +
    /// dma-buf-exportable memory + DRM framebuffer registration.
    /// All steps must succeed; partial allocations are unwound on
    /// error so the returned `Err` leaves no resources leaked.
    pub fn allocate(
        vk: Arc<VkContext>,
        drm: Arc<crate::drm::Device>,
        width: u32,
        height: u32,
    ) -> io::Result<Self> {
        // 1. VkImage + memory + dma-buf export.
        let img = allocate_vk_scanout_image(&vk, width, height)
            .map_err(|e| io::Error::other(format!("vk scanout image: {e}")))?;
        let VkScanoutImage {
            image,
            memory,
            dmabuf,
            pitch,
        } = img;

        // 2. PRIME_FD_TO_HANDLE on the DRM device.
        let gem_handle = match drm.prime_fd_to_buffer(dmabuf.as_fd()) {
            Ok(h) => h,
            Err(e) => {
                unsafe {
                    vk.device.destroy_image(image, None);
                    vk.device.free_memory(memory, None);
                }
                return Err(io::Error::other(format!("drm prime_fd_to_buffer: {e}")));
            }
        };
        // The GEM handle holds its own reference; close the dma-buf
        // fd we no longer need.
        drop(dmabuf);

        // 3. add_fb2 (no modifiers — linear, untagged).
        let fb_handle = match drm.add_planar_framebuffer(
            &VkScanoutFb {
                gem_handle,
                width,
                height,
                pitch,
            },
            FbCmd2Flags::empty(),
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = drm.close_buffer(gem_handle);
                unsafe {
                    vk.device.destroy_image(image, None);
                    vk.device.free_memory(memory, None);
                }
                return Err(io::Error::other(format!("drm add_fb (linear): {e}")));
            }
        };

        // 4. Long-lived export semaphore.
        let vk_semaphore = match create_export_semaphore(&vk) {
            Ok(s) => s,
            Err(e) => {
                let _ = drm.destroy_framebuffer(fb_handle);
                let _ = drm.close_buffer(gem_handle);
                unsafe {
                    vk.device.destroy_image(image, None);
                    vk.device.free_memory(memory, None);
                }
                return Err(io::Error::other(format!("vk semaphore: {e}")));
            }
        };

        // 5. Per-bo transfer resources (always present now —
        //    every bo has a live VkImage to upload into).
        let vk_transfer = match allocate_transfer_resources(&vk, width, height) {
            Ok(t) => t,
            Err(e) => {
                unsafe {
                    vk.device.destroy_semaphore(vk_semaphore, None);
                    vk.device.destroy_image(image, None);
                    vk.device.free_memory(memory, None);
                }
                let _ = drm.destroy_framebuffer(fb_handle);
                let _ = drm.close_buffer(gem_handle);
                return Err(io::Error::other(format!("vk transfer: {e}")));
            }
        };

        // 6. Color image view used by the 4.1.3.4 composite pass
        //    `vkCmdBeginRendering` as the color attachment.
        let view_info = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::B8G8R8A8_UNORM)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let vk_image_view = match unsafe { vk.device.create_image_view(&view_info, None) } {
            Ok(v) => v,
            Err(e) => {
                unsafe {
                    vk.device.unmap_memory(vk_transfer.staging_memory);
                    vk.device.destroy_buffer(vk_transfer.staging_buffer, None);
                    vk.device.free_memory(vk_transfer.staging_memory, None);
                    vk.device
                        .destroy_command_pool(vk_transfer.command_pool, None);
                    vk.device.destroy_semaphore(vk_semaphore, None);
                    vk.device.destroy_image(image, None);
                    vk.device.free_memory(memory, None);
                }
                let _ = drm.destroy_framebuffer(fb_handle);
                let _ = drm.close_buffer(gem_handle);
                return Err(io::Error::other(format!("vk image view: {e}")));
            }
        };

        Ok(Self {
            state: BoState::default(),
            width,
            height,
            pitch,
            vk_image: image,
            vk_memory: memory,
            vk_image_view,
            vk_semaphore,
            fb_handle: Some(fb_handle),
            gem_handle: Some(gem_handle),
            vk_transfer,
            drm,
            vk,
        })
    }

    /// Export a SYNC_FD payload from this bo's signal semaphore. Call
    /// this after `vkQueueSubmit2` with `signalSemaphore = vk_semaphore`
    /// — it returns the freshly-payloaded fd to hand KMS as
    /// `IN_FENCE_FD`. KMS consumes the fd on atomic accept (kernel
    /// closes it).
    #[allow(dead_code)] // wired in by Task 2.5 (atomic-commit fence path).
    pub fn export_signaled_fd(&self) -> Result<OwnedFd, vk::Result> {
        let ext = self.vk.external_semaphore_fd.clone();
        let info = vk::SemaphoreGetFdInfoKHR::default()
            .semaphore(self.vk_semaphore)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
        let raw_fd = unsafe { ext.get_semaphore_fd(&info)? };
        // SAFETY: vkGetSemaphoreFdKHR returns a fresh fd that the
        // caller owns. Wrap in OwnedFd so close() runs on Drop unless
        // the fd is consumed (e.g. handed to KMS via IN_FENCE_FD).
        Ok(unsafe { OwnedFd::from_raw_fd(raw_fd) })
    }
}

impl Drop for ScanoutBo {
    fn drop(&mut self) {
        // Defensive fence-fd cleanup. If the bo was Submitted /
        // Pending / OnScreen / Retiring at drop time (mid-flight
        // shutdown, or modeset that didn't go through the explicit
        // drain path), close any held fence fds so they don't leak.
        // Kernel-side sync_file refs survive our fd close until the
        // DRM device closes — atomic flip will still complete or
        // fail safely on its own.
        let released = self.state.transition_to_free_after_modeset_reset();
        if let Some(fd) = released.in_fence {
            // SAFETY: fd was inserted by transition_to_submitted; we
            // are the unique owner.
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        if let Some(fd) = released.release_fence {
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }

        // DRM-side teardown next: framebuffer references the GEM
        // handle; both must be released before we free the underlying
        // memory the dma-buf was exported from.
        if let Some(fb) = self.fb_handle.take()
            && let Err(e) = self.drm.destroy_framebuffer(fb)
        {
            log::warn!("drm destroy_framebuffer failed: {e}");
        }
        if let Some(h) = self.gem_handle.take()
            && let Err(e) = self.drm.close_buffer(h)
        {
            log::warn!("drm close_buffer (gem) failed: {e}");
        }

        unsafe {
            // Transfer resources (staging mapping must release before
            // memory is freed; command pool releases its CB).
            let t = std::mem::replace(
                &mut self.vk_transfer,
                TransferResources {
                    command_pool: vk::CommandPool::null(),
                    command_buffer: vk::CommandBuffer::null(),
                    staging_buffer: vk::Buffer::null(),
                    staging_memory: vk::DeviceMemory::null(),
                    staging_mapped: std::ptr::NonNull::dangling(),
                    staging_size: 0,
                },
            );
            if t.command_pool != vk::CommandPool::null() {
                self.vk.device.unmap_memory(t.staging_memory);
                self.vk.device.destroy_buffer(t.staging_buffer, None);
                self.vk.device.free_memory(t.staging_memory, None);
                self.vk.device.destroy_command_pool(t.command_pool, None);
            }

            // Image view before image, image before memory, then
            // semaphore.
            if self.vk_image_view != vk::ImageView::null() {
                self.vk.device.destroy_image_view(self.vk_image_view, None);
            }
            self.vk.device.destroy_image(self.vk_image, None);
            self.vk.device.free_memory(self.vk_memory, None);
            if self.vk_semaphore != vk::Semaphore::null() {
                self.vk.device.destroy_semaphore(self.vk_semaphore, None);
            }
        }
    }
}

impl ScanoutBoPool {
    /// Reset every bo in the pool to `Free`, draining any in-flight
    /// fence fds. Used by the modeset / hot-config path (resize, mode
    /// change, hotplug — design §2 "Modeset / hot-config events").
    ///
    /// Order of operations:
    ///
    /// 1. `vkDeviceWaitIdle` on the device — heavy hammer that waits
    ///    for any in-flight `vkQueueSubmit2` work to complete. Cheap
    ///    in steady state (no work) and conservatively correct for
    ///    `Submitted`-phase bos which would otherwise have GPU work
    ///    racing the DRM tear-down.
    /// 2. For each bo, advance state machine to `Free` via
    ///    `transition_to_free_after_modeset_reset` and close any
    ///    returned fence fds.
    ///
    /// Pool dimensions stay the same; this is "reset state machine,
    /// keep the bos." Re-allocating bos with new dimensions is the
    /// caller's responsibility (drop the pool, allocate a fresh one
    /// with `ScanoutBoPool::allocate`).
    #[allow(dead_code)] // wired in by 4.1.2.6 modeset path; today's only consumer is Drop.
    pub fn drain_all_pending(&mut self, vk: &VkContext) {
        if let Err(e) = unsafe { vk.device.device_wait_idle() } {
            log::warn!("scanout pool drain: vkDeviceWaitIdle: {e}");
        }
        for bo in &mut self.bos {
            let released = bo.state.transition_to_free_after_modeset_reset();
            if let Some(fd) = released.in_fence {
                // SAFETY: fd inserted by transition_to_submitted; unique owner.
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
            if let Some(fd) = released.release_fence {
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }

    /// Allocate `count` Vulkan-first bos for one output. Phase 4.1.2
    /// uses 3 bos per pool (design §2). On failure the partial pool
    /// is dropped (each successfully-allocated bo destroys its own
    /// resources via `ScanoutBo::Drop`).
    pub fn allocate(
        vk: Arc<VkContext>,
        drm: Arc<crate::drm::Device>,
        width: u32,
        height: u32,
        count: usize,
    ) -> io::Result<Self> {
        let mut bos = Vec::with_capacity(count);
        for _ in 0..count {
            bos.push(ScanoutBo::allocate(
                Arc::clone(&vk),
                Arc::clone(&drm),
                width,
                height,
            )?);
        }
        Ok(Self { bos, width, height })
    }
}

/// Outputs of [`allocate_vk_scanout_image`]: a freshly-bound VkImage,
/// its memory, the dma-buf fd we exported from that memory, and the
/// row pitch the driver chose.
struct VkScanoutImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    dmabuf: OwnedFd,
    pitch: u32,
}

/// Allocate a `VK_IMAGE_TILING_LINEAR` `VkImage` whose memory is
/// dma-buf-exportable; bind memory; export the dma-buf; query the
/// row pitch the driver picked.
fn allocate_vk_scanout_image(
    vk: &VkContext,
    width: u32,
    height: u32,
) -> Result<VkScanoutImage, vk::Result> {
    let ext_memory_fd = vk
        .external_memory_fd
        .as_ref()
        .ok_or(vk::Result::ERROR_EXTENSION_NOT_PRESENT)?;

    // 1. VkImage with TILING_LINEAR + DMA_BUF external-memory hint.
    //    Linear is what avoids needing
    //    VK_EXT_image_drm_format_modifier; KMS just sees a regular
    //    untagged framebuffer.
    let mut external_info = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::B8G8R8A8_UNORM)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::LINEAR)
        .usage(
            vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::TRANSFER_DST
                | vk::ImageUsageFlags::SAMPLED,
        )
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external_info);

    let image = unsafe { vk.device.create_image(&image_info, None)? };

    // 2. Row pitch from the driver. We need this for KMS add_fb2.
    let layout = unsafe {
        vk.device.get_image_subresource_layout(
            image,
            vk::ImageSubresource {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                array_layer: 0,
            },
        )
    };
    let pitch = u32::try_from(layout.row_pitch).unwrap_or(u32::MAX);

    // 3. Memory: dma-buf-exportable + dedicated to this image.
    let mem_reqs = unsafe { vk.device.get_image_memory_requirements(image) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let memory_type_index = match pick_memory_type(
        &mem_props,
        mem_reqs.memory_type_bits,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    )
    .or_else(|| {
        pick_memory_type(
            &mem_props,
            mem_reqs.memory_type_bits,
            vk::MemoryPropertyFlags::empty(),
        )
    }) {
        Some(i) => i,
        None => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
        }
    };

    let mut export_info = vk::ExportMemoryAllocateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index)
        .push_next(&mut export_info)
        .push_next(&mut dedicated);

    let memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe { vk.device.destroy_image(image, None) };
            return Err(e);
        }
    };

    if let Err(e) = unsafe { vk.device.bind_image_memory(image, memory, 0) } {
        unsafe {
            vk.device.free_memory(memory, None);
            vk.device.destroy_image(image, None);
        }
        return Err(e);
    }

    // 4. Export the bound memory as a dma-buf fd.
    let get_fd_info = vk::MemoryGetFdInfoKHR::default()
        .memory(memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw_fd = match unsafe { ext_memory_fd.get_memory_fd(&get_fd_info) } {
        Ok(fd) => fd,
        Err(e) => {
            unsafe {
                vk.device.free_memory(memory, None);
                vk.device.destroy_image(image, None);
            }
            return Err(e);
        }
    };
    // SAFETY: vkGetMemoryFdKHR returns a fresh fd we own.
    let dmabuf = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    Ok(VkScanoutImage {
        image,
        memory,
        dmabuf,
        pitch,
    })
}

/// Adapter that lets a freshly-imported GEM handle be passed to
/// drm 0.15's `add_planar_framebuffer` as a `PlanarBuffer`. Single
/// plane, linear (no modifier).
struct VkScanoutFb {
    gem_handle: DrmBufferHandle,
    width: u32,
    height: u32,
    pitch: u32,
}

impl DrmPlanarBuffer for VkScanoutFb {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn format(&self) -> DrmFourcc {
        DrmFourcc::Xrgb8888
    }
    fn modifier(&self) -> Option<DrmModifier> {
        // Linear, no explicit modifier — empty FbCmd2Flags goes
        // through the legacy add_fb2 code path.
        None
    }
    fn pitches(&self) -> [u32; 4] {
        [self.pitch, 0, 0, 0]
    }
    fn handles(&self) -> [Option<DrmBufferHandle>; 4] {
        [Some(self.gem_handle), None, None, None]
    }
    fn offsets(&self) -> [u32; 4] {
        [0, 0, 0, 0]
    }
}

/// Create a binary `VkSemaphore` whose payload can be exported as a
/// SYNC_FD via `vkGetSemaphoreFdKHR`. Reused for the bo's full
/// lifetime; the fd payload churns per submit.
fn create_export_semaphore(vk: &VkContext) -> Result<vk::Semaphore, vk::Result> {
    let mut export_info = vk::ExportSemaphoreCreateInfo::default()
        .handle_types(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let create_info = vk::SemaphoreCreateInfo::default().push_next(&mut export_info);
    unsafe { vk.device.create_semaphore(&create_info, None) }
}

/// Allocate per-bo transfer resources: command pool + 1 command
/// buffer; staging buffer + host-mapped device memory sized for one
/// XRGB8888 frame at (width × height).
fn allocate_transfer_resources(
    vk: &VkContext,
    width: u32,
    height: u32,
) -> Result<TransferResources, vk::Result> {
    let pool_info = vk::CommandPoolCreateInfo::default()
        .queue_family_index(vk.graphics_queue_family)
        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    let command_pool = unsafe { vk.device.create_command_pool(&pool_info, None)? };

    let cb_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(command_pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    let command_buffers = match unsafe { vk.device.allocate_command_buffers(&cb_info) } {
        Ok(cbs) => cbs,
        Err(e) => {
            unsafe { vk.device.destroy_command_pool(command_pool, None) };
            return Err(e);
        }
    };
    let command_buffer = command_buffers[0];

    let staging_size: u64 = u64::from(width) * u64::from(height) * 4;
    let buf_info = vk::BufferCreateInfo::default()
        .size(staging_size)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let staging_buffer = match unsafe { vk.device.create_buffer(&buf_info, None) } {
        Ok(b) => b,
        Err(e) => {
            unsafe { vk.device.destroy_command_pool(command_pool, None) };
            return Err(e);
        }
    };

    let mem_reqs = unsafe { vk.device.get_buffer_memory_requirements(staging_buffer) };
    let mem_props = unsafe {
        vk.instance
            .get_physical_device_memory_properties(vk.physical_device)
    };
    let want_strict = vk::MemoryPropertyFlags::HOST_VISIBLE
        | vk::MemoryPropertyFlags::HOST_COHERENT
        | vk::MemoryPropertyFlags::DEVICE_LOCAL;
    let want_loose = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let memory_type_index = pick_memory_type(&mem_props, mem_reqs.memory_type_bits, want_strict)
        .or_else(|| pick_memory_type(&mem_props, mem_reqs.memory_type_bits, want_loose))
        .ok_or(vk::Result::ERROR_OUT_OF_DEVICE_MEMORY);
    let memory_type_index = match memory_type_index {
        Ok(i) => i,
        Err(e) => {
            unsafe {
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };

    let alloc_info = vk::MemoryAllocateInfo::default()
        .allocation_size(mem_reqs.size)
        .memory_type_index(memory_type_index);
    let staging_memory = match unsafe { vk.device.allocate_memory(&alloc_info, None) } {
        Ok(m) => m,
        Err(e) => {
            unsafe {
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };
    if let Err(e) = unsafe {
        vk.device
            .bind_buffer_memory(staging_buffer, staging_memory, 0)
    } {
        unsafe {
            vk.device.free_memory(staging_memory, None);
            vk.device.destroy_buffer(staging_buffer, None);
            vk.device.destroy_command_pool(command_pool, None);
        }
        return Err(e);
    }

    let mapped_ptr = match unsafe {
        vk.device
            .map_memory(staging_memory, 0, staging_size, vk::MemoryMapFlags::empty())
    } {
        Ok(p) => p,
        Err(e) => {
            unsafe {
                vk.device.free_memory(staging_memory, None);
                vk.device.destroy_buffer(staging_buffer, None);
                vk.device.destroy_command_pool(command_pool, None);
            }
            return Err(e);
        }
    };
    let staging_mapped =
        std::ptr::NonNull::new(mapped_ptr.cast::<u8>()).expect("vkMapMemory returned non-null");

    Ok(TransferResources {
        command_pool,
        command_buffer,
        staging_buffer,
        staging_memory,
        staging_mapped,
        staging_size,
    })
}

fn pick_memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    required: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        let candidate = type_bits & (1 << i) != 0;
        candidate
            && props.memory_types[i as usize]
                .property_flags
                .contains(required)
    })
}

// Compile-only check that `export_signaled_fd`'s call into
// `external_semaphore_fd::Device::get_semaphore_fd` keeps the same
// argument shape. If ash bumps and the signature changes, this
// function fails to compile and breaks the build before any
// integration test runs.
#[cfg(test)]
#[allow(dead_code)]
fn _compile_check_export_signature(
    ext: &ash::khr::external_semaphore_fd::Device,
    semaphore: vk::Semaphore,
) {
    let info = vk::SemaphoreGetFdInfoKHR::default()
        .semaphore(semaphore)
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let _: Result<i32, vk::Result> = unsafe { ext.get_semaphore_fd(&info) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_bo_is_free() {
        let bo = BoState::default();
        assert_eq!(bo.phase, BoPhase::Free);
        assert!(bo.in_fence_fd.is_none());
        assert!(bo.release_fence_fd.is_none());
    }

    #[test]
    fn record_then_submit_transitions_to_submitted() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        assert_eq!(bo.phase, BoPhase::Recording);
        bo.transition_to_submitted(/* in_fence */ 42);
        assert_eq!(bo.phase, BoPhase::Submitted);
        assert_eq!(bo.in_fence_fd, Some(42));
    }

    #[test]
    fn atomic_accept_returns_in_fence_for_caller_to_close_and_stores_out_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(42);
        let reclaimed = bo.transition_to_pending(/* out_fence */ 99);
        assert_eq!(bo.phase, BoPhase::Pending);
        assert_eq!(
            reclaimed,
            Some(42),
            "caller closes the in-fence fd; kernel only refs the sync_file"
        );
        assert!(bo.in_fence_fd.is_none(), "moved out into reclaimed");
        assert_eq!(bo.release_fence_fd, Some(99));
    }

    #[test]
    fn atomic_reject_returns_to_recording_and_we_still_own_in_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(42);
        let reclaimed = bo.transition_to_recording_after_atomic_reject();
        assert_eq!(bo.phase, BoPhase::Recording);
        assert_eq!(reclaimed, Some(42), "caller closes the fd");
        assert!(bo.in_fence_fd.is_none(), "moved out into reclaimed");
    }

    #[test]
    fn modeset_preempt_from_submitted_returns_in_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(7);
        let in_fence = bo.transition_to_free_after_modeset_preempt();
        assert_eq!(bo.phase, BoPhase::Free);
        assert_eq!(in_fence, Some(7));
    }

    #[test]
    fn pending_then_onscreen_then_retiring_then_free_releases_fence() {
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(11);
        let _ = bo.transition_to_pending(22);
        assert_eq!(bo.phase, BoPhase::Pending);

        bo.transition_to_on_screen();
        assert_eq!(bo.phase, BoPhase::OnScreen);
        assert_eq!(
            bo.release_fence_fd,
            Some(22),
            "release fence stays attached while on-screen"
        );

        bo.transition_to_retiring();
        assert_eq!(bo.phase, BoPhase::Retiring);

        let release = bo.transition_to_free_after_retire();
        assert_eq!(bo.phase, BoPhase::Free);
        assert_eq!(release, Some(22), "caller closes the release fence");
        assert!(bo.release_fence_fd.is_none());
    }

    /// 4.1.2.7 fence-cycle integration test (host-pure variant per
    /// the plan: "mock the GPU side if Vulkan creation under
    /// lavapipe is awkward inside `cargo test`"). Drives a 3-bo
    /// pool through 6 frames in the steady-state cycle and asserts
    /// every fence fd issued is closed exactly once. No real GPU.
    /// The accounting catches state-machine bugs that leak fence
    /// fds (which is exactly the class of bug we hit on bare metal
    /// with the original IN_FENCE_FD ownership confusion).
    #[test]
    fn six_frames_cycle_through_pool_without_leaking_fences() {
        let mut bos: Vec<BoState> = (0..3).map(|_| BoState::default()).collect();
        let mut issued = 0u32;
        let mut closed = 0u32;
        let mut next_fd = 100i32;

        let alloc_fd = |issued: &mut u32, next_fd: &mut i32| -> i32 {
            *issued += 1;
            let fd = *next_fd;
            *next_fd += 1;
            fd
        };
        let close = |fd: Option<i32>, closed: &mut u32| {
            if fd.is_some() {
                *closed += 1;
            }
        };

        for _frame in 0..6 {
            // 1. Acquire Free bo and submit.
            let bo_idx = bos.iter().position(|b| b.phase == BoPhase::Free).expect(
                "with 3 bos and the cycle-advance below, at least one bo \
                 should be Free every frame",
            );
            let bo = &mut bos[bo_idx];
            bo.transition_to_recording();
            let in_fence = alloc_fd(&mut issued, &mut next_fd);
            bo.transition_to_submitted(in_fence);

            // 2. Atomic accept → Pending; closes the in-fence we just
            //    issued.
            let out_fence = alloc_fd(&mut issued, &mut next_fd);
            close(bo.transition_to_pending(out_fence), &mut closed);

            // 3. Pageflip-complete advance (mirrors
            //    `advance_pool_on_pageflip_complete` in backend.rs).
            let phases: Vec<BoPhase> = bos.iter().map(|b| b.phase).collect();
            for (i, phase) in phases.into_iter().enumerate() {
                match phase {
                    BoPhase::Retiring => {
                        close(bos[i].transition_to_free_after_retire(), &mut closed);
                    }
                    BoPhase::OnScreen => bos[i].transition_to_retiring(),
                    BoPhase::Pending => bos[i].transition_to_on_screen(),
                    _ => {}
                }
            }
        }

        // Drain remaining bos (simulates shutdown).
        for bo in &mut bos {
            let r = bo.transition_to_free_after_modeset_reset();
            close(r.in_fence, &mut closed);
            close(r.release_fence, &mut closed);
        }

        assert_eq!(
            issued, closed,
            "every fence fd issued must be closed exactly once \
             (issued={issued}, closed={closed})"
        );
        assert_eq!(
            issued, 12,
            "6 frames × (1 in_fence + 1 release_fence) = 12 fds expected"
        );
    }

    #[test]
    fn modeset_reset_returns_all_currently_held_fences() {
        // Pending: in-fence already returned to caller for closing,
        // live release fence still held by the bo.
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(5);
        let _ = bo.transition_to_pending(60);
        let released = bo.transition_to_free_after_modeset_reset();
        assert_eq!(bo.phase, BoPhase::Free);
        assert_eq!(released.in_fence, None);
        assert_eq!(released.release_fence, Some(60));

        // Submitted: still own the in-fence.
        let mut bo = BoState::default();
        bo.transition_to_recording();
        bo.transition_to_submitted(5);
        let released = bo.transition_to_free_after_modeset_reset();
        assert_eq!(released.in_fence, Some(5));
        assert_eq!(released.release_fence, None);

        // Recording: nothing held.
        let mut bo = BoState::default();
        bo.transition_to_recording();
        let released = bo.transition_to_free_after_modeset_reset();
        assert_eq!(released.in_fence, None);
        assert_eq!(released.release_fence, None);
    }
}
