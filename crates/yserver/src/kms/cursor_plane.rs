//! Hardware cursor plane — replaces the Vulkan-composited cursor
//! quad with a kernel-managed DRM cursor overlay.
//!
//! Why: the cursor quad was tied to compositor cadence. Every cursor
//! position change waited for the next `composite_and_flip`, which
//! is stalled by per-op `vkQueueWaitIdle` in the paint pipeline
//! (notably when hovering over GTK widgets that schedule
//! gradient/emboss repaints — observed as severe pointer lag in
//! mate-control-center on fuji). The DRM hardware cursor plane is
//! a separate overlay the kernel positions independently —
//! `drmModeMoveCursor` is one ioctl, microseconds, doesn't touch
//! the GPU.
//!
//! The legacy `set_cursor2` / `move_cursor` ioctls are marked
//! deprecated in the drm crate in favor of the atomic cursor-plane
//! API, but they're universally supported on every mainstream
//! desktop GPU. Atomic-plane upgrade is a follow-up if/when more
//! cursor plane features are wanted.

use std::{io, mem, ptr::NonNull, sync::Arc};

use drm::{
    buffer::{Buffer, DrmFourcc},
    control::{Device as ControlDevice, crtc, dumbbuffer::DumbBuffer},
};

use crate::drm::Device;

/// Universal minimum hardware cursor size on every Intel / AMD /
/// mainstream-Mali iGPU since ~2010. X11 cursor themes are usually
/// ≤ 32×32; cursors larger than this fall back to the Vulkan
/// composite path.
pub const HW_CURSOR_W: u32 = 64;
pub const HW_CURSOR_H: u32 = 64;

/// A single shared DRM dumb buffer holding the current cursor image,
/// plus per-CRTC visibility state. The same buffer is bound to every
/// CRTC's cursor plane via `set_cursor2`; each CRTC has its own
/// position tracked independently by the kernel.
pub struct CursorPlane {
    device: Arc<Device>,
    dumb: Option<DumbBuffer>,
    ptr: NonNull<u8>,
    len: usize,
    stride: u32,
    /// True after `show` succeeded; flipped back by `hide`. Tracked
    /// per-plane (not per-CRTC) for now — multi-output enable/disable
    /// races aren't observable today.
    visible: bool,
}

// SAFETY: ptr is an mmap'd kernel buffer that lives as long as
// `dumb`; no thread does interior mutation through the raw pointer
// without exclusive `&mut self`.
unsafe impl Send for CursorPlane {}

impl CursorPlane {
    /// Allocate the cursor dumb buffer + mmap it. The buffer is
    /// zero-filled so an initial `show` before any image lands
    /// doesn't display random bytes.
    ///
    /// # Errors
    /// `create_dumb_buffer` or `map_dumb_buffer` ioctl failures.
    pub fn new(device: Arc<Device>) -> io::Result<Self> {
        let mut dumb =
            device.create_dumb_buffer((HW_CURSOR_W, HW_CURSOR_H), DrmFourcc::Argb8888, 32)?;
        let stride = dumb.pitch();
        // Map; on failure leak the dumb buffer — the kernel
        // reclaims it when `device` is dropped. We can't call
        // `destroy_dumb_buffer(dumb)` in the Err arm because the
        // `Result<DumbMapping<'_>, _>` discriminant keeps `dumb`
        // mutably borrowed until end of match scope.
        let mapping = device.map_dumb_buffer(&mut dumb)?;
        let len = mapping.len();
        let ptr =
            NonNull::new(mapping.as_ptr() as *mut u8).expect("non-null mmap for cursor plane");
        // Leak the mapping handle; mmap stays alive via the dumb
        // buffer kept by `Self::dumb`. Released in Drop.
        mem::forget(mapping);
        // Zero-fill the plane buffer up front (the kernel doesn't
        // guarantee zeroed contents on create_dumb_buffer).
        unsafe { std::ptr::write_bytes(ptr.as_ptr(), 0, len) };
        Ok(Self {
            device,
            dumb: Some(dumb),
            ptr,
            len,
            stride,
            visible: false,
        })
    }

    /// Copy a cursor image into the plane buffer. `bgra_bytes` is a
    /// tightly-packed `width × height × 4` BGRA8 buffer matching the
    /// DRM `ARGB8888` byte order in little-endian. The image lands at
    /// (0, 0); the remainder of the 64×64 buffer is zero-filled
    /// (transparent).
    ///
    /// Returns `Err(InvalidInput)` if the image is larger than
    /// `HW_CURSOR_W × HW_CURSOR_H` — caller falls back to the
    /// compositor cursor path.
    pub fn load_image(&mut self, image_w: u32, image_h: u32, bgra_bytes: &[u8]) -> io::Result<()> {
        if image_w == 0 || image_h == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero-sized cursor",
            ));
        }
        if image_w > HW_CURSOR_W || image_h > HW_CURSOR_H {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cursor exceeds hardware plane size",
            ));
        }
        let img_stride = (image_w as usize) * 4;
        let expected_bytes = img_stride * image_h as usize;
        if bgra_bytes.len() < expected_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cursor bytes shorter than width*height*4",
            ));
        }
        // Clear so a smaller cursor doesn't leave previous pixels.
        unsafe { std::ptr::write_bytes(self.ptr.as_ptr(), 0, self.len) };
        for row in 0..(image_h as usize) {
            let src_off = row * img_stride;
            let dst_off = row * (self.stride as usize);
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bgra_bytes.as_ptr().add(src_off),
                    self.ptr.as_ptr().add(dst_off),
                    img_stride,
                );
            }
        }
        Ok(())
    }

    /// Make the cursor visible on `crtc` with `hotspot = (hot_x, hot_y)`.
    /// Idempotent — repeated calls just re-bind the same buffer.
    #[allow(deprecated)]
    pub fn show(&mut self, crtc: crtc::Handle, hotspot: (i32, i32)) -> io::Result<()> {
        let Some(dumb) = self.dumb.as_ref() else {
            return Err(io::Error::other("cursor plane already destroyed"));
        };
        self.device.set_cursor2(crtc, Some(dumb), hotspot)?;
        self.visible = true;
        Ok(())
    }

    /// Detach the cursor from `crtc`. The plane buffer is retained so
    /// a future `show` doesn't have to re-allocate.
    #[allow(deprecated)]
    pub fn hide(&mut self, crtc: crtc::Handle) -> io::Result<()> {
        self.device.set_cursor2::<DumbBuffer>(crtc, None, (0, 0))?;
        self.visible = false;
        Ok(())
    }

    /// Move the cursor on `crtc` to `(x, y)` in CRTC-local coords. The
    /// kernel clips to the CRTC's pixel rect; passing coords outside
    /// the visible area just hides it on that output.
    #[allow(deprecated)]
    pub fn move_to(&self, crtc: crtc::Handle, x: i32, y: i32) -> io::Result<()> {
        self.device.move_cursor(crtc, (x, y))
    }

    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible
    }
}

impl Drop for CursorPlane {
    fn drop(&mut self) {
        if let Some(dumb) = self.dumb.take() {
            let _ = self.device.destroy_dumb_buffer(dumb);
        }
    }
}
