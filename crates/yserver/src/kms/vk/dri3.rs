//! DRI3 Vulkan helpers — modifier query and dma-buf import.
//!
//! Phase 4.2 design §3.2. The headline entry point is
//! [`supported_modifiers`]; [`import_dmabuf`] (Task 10) will follow.

use ash::vk;
use std::{ffi::c_void, sync::Arc};

use super::{
    device::VkContext,
    target::{DrawableImage, DrawableImageError},
};

/// Linear, untagged DRM format modifier (`DRM_FORMAT_MOD_LINEAR`).
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// All DRM format modifiers the driver advertises as importable for
/// `format` as DMA_BUF (single-plane, EXCLUSIVE sharing).
///
/// Algorithm per design §3.2:
/// 1. Query `VkDrmFormatModifierPropertiesListEXT` via
///    `vkGetPhysicalDeviceFormatProperties2` to enumerate every DRM
///    modifier the driver can use with `format`.
/// 2. For each candidate, query
///    `vkGetPhysicalDeviceImageFormatProperties2` with
///    `VkPhysicalDeviceImageDrmFormatModifierInfoEXT` and
///    `VkPhysicalDeviceExternalImageFormatInfo` chained as **siblings**
///    under `VkPhysicalDeviceImageFormatInfo2`. Keep modifiers where
///    the call succeeds and `compatibleHandleTypes` includes
///    `DMA_BUF_BIT_EXT`.
///
/// When `VK_EXT_image_drm_format_modifier` is unavailable (e.g.
/// lavapipe on older Mesa), returns `[DRM_FORMAT_MOD_LINEAR]` so that
/// LINEAR-tiled imports still work. Per design §3.2 the LINEAR
/// modifier is always implicitly supported by the import path —
/// `VK_IMAGE_TILING_LINEAR` plus a plain `VkExternalMemoryImageCreateInfo`
/// chain (no explicit-modifier struct) is the fallback.
#[must_use]
pub fn supported_modifiers(vk: &VkContext, format: vk::Format) -> Vec<u64> {
    if !vk.image_drm_format_modifier {
        return vec![DRM_FORMAT_MOD_LINEAR];
    }

    let modifier_count = match list_modifier_count(vk, format) {
        Ok(n) if n > 0 => n,
        _ => return vec![DRM_FORMAT_MOD_LINEAR],
    };

    let mut props_storage =
        vec![vk::DrmFormatModifierPropertiesEXT::default(); modifier_count as usize];
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default()
        .drm_format_modifier_properties(&mut props_storage);
    let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
    unsafe {
        vk.instance.get_physical_device_format_properties2(
            vk.physical_device,
            format,
            &mut format_props,
        );
    }
    let entries = list.drm_format_modifier_count as usize;

    let mut accepted = Vec::with_capacity(entries);
    for prop in props_storage.iter().take(entries) {
        if can_import_modifier(vk, format, prop.drm_format_modifier) {
            accepted.push(prop.drm_format_modifier);
        }
    }

    if accepted.is_empty() {
        accepted.push(DRM_FORMAT_MOD_LINEAR);
    }
    accepted
}

fn list_modifier_count(vk: &VkContext, format: vk::Format) -> Result<u32, vk::Result> {
    let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
    let mut format_props = vk::FormatProperties2::default().push_next(&mut list);
    unsafe {
        vk.instance.get_physical_device_format_properties2(
            vk.physical_device,
            format,
            &mut format_props,
        );
    }
    Ok(list.drm_format_modifier_count)
}

/// Probe whether `(format, modifier)` is importable as a DMA_BUF
/// `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` image with the
/// usage flags Phase 4.2 needs.
fn can_import_modifier(vk: &VkContext, format: vk::Format, modifier: u64) -> bool {
    // Build the chain manually: format_info.pNext -> external_info,
    // external_info.pNext -> modifier_info, modifier_info.pNext -> null.
    // ash's `push_next` builder isn't implemented for
    // `PhysicalDeviceExternalImageFormatInfo` ↔
    // `PhysicalDeviceImageDrmFormatModifierInfoEXT` (the lifetime
    // generic prevents it). Per design §3.2 we're set up as siblings
    // under format_info, which the driver walks as a flat list.
    let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
        .drm_format_modifier(modifier)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);

    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    external_info.p_next = std::ptr::from_mut(&mut modifier_info).cast::<c_void>();

    let mut format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
        .usage(
            vk::ImageUsageFlags::SAMPLED
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST
                | vk::ImageUsageFlags::COLOR_ATTACHMENT,
        );
    format_info.p_next = std::ptr::from_mut(&mut external_info).cast::<c_void>();

    let mut external_props = vk::ExternalImageFormatProperties::default();
    let mut props2 = vk::ImageFormatProperties2::default().push_next(&mut external_props);

    let result = unsafe {
        vk.instance.get_physical_device_image_format_properties2(
            vk.physical_device,
            &format_info,
            &mut props2,
        )
    };

    if result.is_err() {
        return false;
    }
    external_props
        .external_memory_properties
        .compatible_handle_types
        .contains(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
}

/// One plane of a multi-plane DMA_BUF import. Phase 4.2 only accepts
/// `num_planes == 1` (RGB single-plane); the multi-plane shape is
/// kept on the API so future YCbCr work doesn't need to widen
/// `import_dmabuf`'s signature.
#[derive(Debug, Clone, Copy)]
pub struct DmabufPlane {
    pub offset: u64,
    pub pitch: u32,
}

/// Export a `DrawableImage`'s backing memory as a fresh dma-buf fd.
/// Phase 4.2 design §3.2 export path. Caller owns the returned fd.
///
/// Single-plane only (matches the import path's scope). Returns the
/// row pitch and total memory size needed by the
/// `BufferFromPixmap` reply.
pub fn export_dmabuf(
    vk: &VkContext,
    drawable: &super::target::DrawableImage,
) -> Result<DmabufExport, vk::Result> {
    let ext = vk
        .external_memory_fd
        .as_ref()
        .ok_or(vk::Result::ERROR_EXTENSION_NOT_PRESENT)?;
    let memory = drawable.backing_memory();
    let layout = unsafe {
        vk.device.get_image_subresource_layout(
            drawable.vk_image,
            vk::ImageSubresource {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                array_layer: 0,
            },
        )
    };
    let size = u32::try_from(layout.size).unwrap_or(u32::MAX);
    let pitch = u32::try_from(layout.row_pitch).unwrap_or(u32::MAX);
    let info = vk::MemoryGetFdInfoKHR::default()
        .memory(memory)
        .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
    let raw_fd = unsafe { ext.get_memory_fd(&info)? };
    let fd = super::owned_fd_from_vk(raw_fd, "vkGetMemoryFdKHR(DMA_BUF)")?;
    Ok(DmabufExport {
        fd,
        size,
        stride: pitch,
    })
}

#[derive(Debug)]
pub struct DmabufExport {
    pub fd: std::os::fd::OwnedFd,
    pub size: u32,
    pub stride: u32,
}

/// Import a client-supplied dma-buf into a `DrawableImage` per design
/// §3.2. Takes ownership of `dma_buf_fd`. On success the fd lifetime
/// is owned by the resulting `DrawableImage`; on failure the OwnedFd
/// drops and closes the fd.
pub fn import_dmabuf(
    vk: Arc<VkContext>,
    dma_buf_fd: std::os::fd::OwnedFd,
    width: u32,
    height: u32,
    format: vk::Format,
    modifier: u64,
    planes: &[DmabufPlane],
) -> Result<DrawableImage, DrawableImageError> {
    let plane_offsets: Vec<u64> = planes.iter().map(|p| p.offset).collect();
    let plane_pitches: Vec<u32> = planes.iter().map(|p| p.pitch).collect();
    DrawableImage::from_dmabuf(
        vk,
        dma_buf_fd,
        width,
        height,
        format,
        modifier,
        &plane_offsets,
        &plane_pitches,
    )
}

/// Outcome of [`wait_dmabuf_read_ready`], for logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmabufReadWait {
    /// No fence attached (buffer already idle) — nothing to wait on.
    Idle,
    /// Producer writes completed before the deadline.
    Ready,
    /// Deadline elapsed with the fence still pending — caller proceeds
    /// anyway (a possibly-incomplete frame, never a hang).
    TimedOut,
    /// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` unsupported / errored — caller
    /// falls back to the prior (no-wait) behaviour.
    Unsupported,
}

// `struct dma_buf_export_sync_file { __u32 flags; __s32 fd; }`
#[repr(C)]
struct DmaBufExportSyncFile {
    flags: u32,
    fd: i32,
}

// `_IOWR(DMA_BUF_BASE='b', 2, struct dma_buf_export_sync_file)` →
// dir=READ|WRITE(3) size=8 type='b'(0x62) nr=2.
const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: libc::c_ulong = 0xc008_6202;
const DMA_BUF_SYNC_READ: u32 = 1 << 0;

/// CPU-wait for a DRI3-imported dma-buf's outstanding producer writes
/// to complete before yserver reads it (e.g. a `PresentPixmap` copy).
///
/// `PresentPixmap` with `wait_fence=0` relies on implicit dma-buf sync;
/// some GPU stacks (Turnip/Adreno, Apple) don't make yserver's read
/// queue honour it, so the copy can race the client's still-pending GPU
/// render and capture a partly-rendered (transparent) frame. This
/// exports the buffer's read fence (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`,
/// `DMA_BUF_SYNC_READ` → the *write* fence a reader must wait on) and
/// `poll()`s it.
///
/// **Bounded / deadlock-safe:** on `timeout_ms` elapse it returns
/// [`DmabufReadWait::TimedOut`] and the caller proceeds — worst case a
/// stale frame, never a stall. This is the CONFIRMATION path; the
/// production fix replaces the CPU poll with a GPU wait-semaphore on the
/// copy submit.
pub fn wait_dmabuf_read_ready(
    dma_buf_fd: std::os::fd::BorrowedFd<'_>,
    timeout_ms: i32,
) -> DmabufReadWait {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let mut export = DmaBufExportSyncFile {
        flags: DMA_BUF_SYNC_READ,
        fd: -1,
    };
    // SAFETY: ioctl on a valid borrowed dma-buf fd with a correctly
    // sized request struct. Returns 0 on success and fills `export.fd`.
    let rc = unsafe {
        libc::ioctl(
            dma_buf_fd.as_raw_fd(),
            DMA_BUF_IOCTL_EXPORT_SYNC_FILE,
            std::ptr::addr_of_mut!(export),
        )
    };
    if rc != 0 {
        return DmabufReadWait::Unsupported;
    }
    if export.fd < 0 {
        // No fences in the reservation object → buffer is idle.
        return DmabufReadWait::Idle;
    }
    // Own the returned sync_file fd so it is always closed.
    // SAFETY: the kernel just handed us an owned fd via the ioctl.
    let sync_fd = unsafe { OwnedFd::from_raw_fd(export.fd) };
    let mut pfd = libc::pollfd {
        fd: sync_fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: single valid pollfd; bounded timeout.
    let pr = unsafe { libc::poll(std::ptr::addr_of_mut!(pfd), 1, timeout_ms) };
    // `sync_fd` drops (closes) here regardless of outcome.
    if pr > 0 && (pfd.revents & libc::POLLIN) != 0 {
        DmabufReadWait::Ready
    } else {
        DmabufReadWait::TimedOut
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drm_format_mod_linear_is_zero() {
        // Sanity: the LINEAR sentinel really is 0 — many call sites
        // bake this in.
        assert_eq!(DRM_FORMAT_MOD_LINEAR, 0);
    }

    // supported_modifiers() and import_dmabuf() are exercised only
    // against a real VkContext (lavapipe LINEAR-fallback leg + Venus
    // modifier-path leg of the §5.5 hardware coverage matrix), via
    // the vng integration smoke. No unit-test seam without faking
    // the entire instance/device, which the workspace doesn't do.
}
