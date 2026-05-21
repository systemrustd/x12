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
//! an atomic position commit is microseconds and doesn't touch the GPU.
//!
//! Atomic cursor plane (replacement for legacy `set_cursor2` /
//! `move_cursor` ioctls): when `DRM_CLIENT_CAP_ATOMIC` is set,
//! AMD/amdgpu ignores the legacy cursor ioctls even though they
//! succeed. Atomic plane commits work on all drivers including AMD.
//! Legacy ioctls are retained as a per-CRTC fallback when no atomic
//! cursor plane is discovered.
//!
//! Stage 5 Phase B (per-CRTC visibility + upload/show split): the
//! shared dumb buffer is mutated by `load_image` and bound to each
//! CRTC independently. Per-CRTC visibility tracking lets the
//! per-output `PendingAck` design queue a Sw→Hw transition for one
//! output without prematurely binding the plane on outputs that
//! haven't retired the transition yet (the multi-output double-cursor
//! hazard).

use std::{collections::HashMap, io, mem, ptr::NonNull, sync::Arc};

use drm::{
    Device as DrmDevice, DriverCapability,
    buffer::{Buffer, DrmFourcc, Handle as DrmBufferHandle, PlanarBuffer as DrmPlanarBuffer},
    control::{
        AtomicCommitFlags, Device as ControlDevice, FbCmd2Flags, PlaneType, atomic::AtomicModeReq,
        crtc, dumbbuffer::DumbBuffer, framebuffer, plane, property,
    },
};

use crate::drm::{Device, modeset::PropMap};

/// Fallback cursor size when `DRM_CAP_CURSOR_WIDTH/HEIGHT` query fails
/// (very old drivers, broken devices). Every Intel / AMD / mainstream-
/// Mali iGPU since ~2010 supports at least 64×64.
///
/// The ACTUAL dumb buffer is allocated at the dimensions the driver
/// reports via `DriverCapability::CursorWidth/Height` (typically 64 on
/// Intel i915, 128 or 256 on amdgpu, varies on others). Using the
/// driver-reported size is load-bearing: amdgpu's display engine
/// interprets the cursor framebuffer as if it were `cursor_width ×
/// cursor_height`, so allocating smaller causes it to read past our
/// data → cursor vertically squished + intermittent corruption.
pub const HW_CURSOR_FALLBACK_W: u32 = 64;
pub const HW_CURSOR_FALLBACK_H: u32 = 64;

/// Per-CRTC atomic plane state. Populated during `CursorPlane::new()`
/// from `DRM_CLIENT_CAP_UNIVERSAL_PLANES` plane enumeration. When
/// present, all show/move/hide operations use atomic commits; when
/// absent for a CRTC, the legacy `set_cursor2` / `move_cursor` ioctls
/// are used as a fallback (works on Intel; may be no-ops on AMD).
struct PerCrtcAtomicState {
    plane: plane::Handle,
    prop_fb_id: property::Handle,
    prop_crtc_id: property::Handle,
    prop_crtc_x: property::Handle,
    prop_crtc_y: property::Handle,
    prop_crtc_w: property::Handle,
    prop_crtc_h: property::Handle,
    prop_src_x: property::Handle,
    prop_src_y: property::Handle,
    prop_src_w: property::Handle,
    prop_src_h: property::Handle,
}

/// A single shared DRM dumb buffer holding the current cursor image,
/// plus per-CRTC visibility state and optional atomic plane handles.
///
/// Per-CRTC visibility (Stage 5 Phase B refactor): each CRTC tracks
/// whether the plane is currently bound to it. v1's pre-Phase-B global
/// `visible: bool` was correct only on single-output systems and exposed
/// the multi-output double-cursor hazard when one output retired a
/// Sw→Hw transition before another.
pub struct CursorPlane {
    device: Arc<Device>,
    dumb: Option<DumbBuffer>,
    ptr: NonNull<u8>,
    len: usize,
    stride: u32,
    /// Cursor buffer dimensions in pixels. Sourced from
    /// `DriverCapability::CursorWidth/Height`. Mandatory match with the
    /// dumb buffer + atomic plane geometry — see [`HW_CURSOR_FALLBACK_W`].
    width: u32,
    height: u32,
    /// DRM framebuffer wrapping the dumb buffer, used by the atomic path.
    fb: Option<framebuffer::Handle>,
    /// Per-CRTC atomic cursor plane state; absent when cursor plane
    /// discovery failed for a CRTC (legacy ioctls used as fallback).
    per_crtc: HashMap<crtc::Handle, PerCrtcAtomicState>,
    /// Per-CRTC binding state — `Some(true)` when the plane is shown on
    /// that CRTC; `Some(false)` when hidden; absent until first show/hide.
    visible: HashMap<crtc::Handle, bool>,
    /// Stage 5 Phase B — `CursorRecord.version` last memcpy'd into
    /// the dumb buffer. `cursor_plane_upload_image` compares the
    /// requested version against this for upload dedup; `None` after
    /// init / VT-leave / full modeset (forces the next show to
    /// re-upload).
    uploaded_version: Option<u64>,
}

// SAFETY: ptr is an mmap'd kernel buffer that lives as long as
// `dumb`; no thread does interior mutation through the raw pointer
// without exclusive `&mut self`.
unsafe impl Send for CursorPlane {}

impl CursorPlane {
    /// Allocate the cursor dumb buffer + mmap it. Discovers cursor
    /// planes for `crtcs` and creates a DRM framebuffer for the atomic
    /// path. Falls back to legacy ioctls on any per-CRTC discovery
    /// failure.
    ///
    /// # Errors
    /// `create_dumb_buffer` or `map_dumb_buffer` ioctl failures.
    pub fn new(device: Arc<Device>, crtcs: &[crtc::Handle]) -> io::Result<Self> {
        // Query the driver's preferred cursor dimensions. amdgpu commonly
        // reports 128×128 or 256×256; i915 typically 64×64. We MUST use
        // the reported size — see [`HW_CURSOR_FALLBACK_W`] for the
        // load-bearing rationale.
        let width = device
            .get_driver_capability(DriverCapability::CursorWidth)
            .ok()
            .filter(|&w| w >= u64::from(HW_CURSOR_FALLBACK_W))
            .and_then(|w| u32::try_from(w).ok())
            .unwrap_or(HW_CURSOR_FALLBACK_W);
        let height = device
            .get_driver_capability(DriverCapability::CursorHeight)
            .ok()
            .filter(|&h| h >= u64::from(HW_CURSOR_FALLBACK_H))
            .and_then(|h| u32::try_from(h).ok())
            .unwrap_or(HW_CURSOR_FALLBACK_H);
        log::info!("cursor: driver reports CursorWidth={width} CursorHeight={height}");

        let mut dumb = device.create_dumb_buffer((width, height), DrmFourcc::Argb8888, 32)?;
        let stride = dumb.pitch();
        let mapping = device.map_dumb_buffer(&mut dumb)?;
        let len = mapping.len();
        let ptr =
            NonNull::new(mapping.as_ptr() as *mut u8).expect("non-null mmap for cursor plane");
        mem::forget(mapping);
        // Zero-fill the plane buffer up front.
        unsafe { std::ptr::write_bytes(ptr.as_ptr(), 0, len) };

        // Atomic path: create DRM framebuffer from the dumb buffer, then
        // discover cursor planes. Failure is non-fatal; per-CRTC entries
        // are left empty and the legacy-ioctl fallback is used instead.
        let fb = device
            .add_planar_framebuffer(
                &CursorDumbFb {
                    dumb: &dumb,
                    stride,
                    width,
                    height,
                },
                FbCmd2Flags::empty(),
            )
            .map_err(|e| {
                log::warn!(
                    "cursor: add_planar_framebuffer failed ({e}); \
                     atomic cursor unavailable, falling back to legacy ioctls"
                );
                e
            })
            .ok();

        let per_crtc = if let Some(fb_handle) = fb {
            match discover_cursor_planes(&device, crtcs, fb_handle) {
                Ok(map) => {
                    let found = map.len();
                    if found == crtcs.len() {
                        log::info!("cursor: atomic cursor plane ready for {found} CRTC(s)");
                    } else {
                        log::warn!(
                            "cursor: found {found}/{} atomic cursor planes; \
                             remaining CRTCs will use legacy ioctls",
                            crtcs.len()
                        );
                    }
                    map
                }
                Err(e) => {
                    log::warn!(
                        "cursor: cursor plane discovery failed ({e}); \
                         falling back to legacy ioctls for all CRTCs"
                    );
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };

        Ok(Self {
            device,
            dumb: Some(dumb),
            ptr,
            len,
            stride,
            width,
            height,
            fb,
            per_crtc,
            visible: HashMap::new(),
            uploaded_version: None,
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
        if image_w > self.width || image_h > self.height {
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

    /// Stage 5 Phase B — versioned upload. Memcpys `bgra_bytes` into
    /// the shared dumb buffer ONLY when `version` differs from
    /// `uploaded_version`. **Never calls `set_cursor2`**; binding
    /// the buffer to a CRTC is a separate step (`show`).
    /// This split is load-bearing for the per-output transition
    /// state machine — uploading must not prematurely show pixels
    /// on CRTCs whose Sw→Hw retire is still pending.
    ///
    /// # Errors
    /// Same as [`Self::load_image`].
    pub fn upload_image(
        &mut self,
        version: u64,
        image_w: u32,
        image_h: u32,
        bgra_bytes: &[u8],
    ) -> io::Result<()> {
        if self.uploaded_version == Some(version) {
            return Ok(());
        }
        self.load_image(image_w, image_h, bgra_bytes)?;
        self.uploaded_version = Some(version);
        Ok(())
    }

    /// The version currently held in the dumb buffer, if any.
    #[must_use]
    pub fn uploaded_version(&self) -> Option<u64> {
        self.uploaded_version
    }

    /// Invalidate the tracked uploaded version. The next
    /// `upload_image` will memcpy unconditionally regardless of
    /// version. Used by global recovery paths (VT-leave, full
    /// modeset, `drain_all`).
    pub fn invalidate_uploaded_version(&mut self) {
        self.uploaded_version = None;
    }

    /// Make the cursor visible on `crtc` at image-top-left position
    /// `(img_x, img_y)` in CRTC-local coordinates, with the given
    /// `hotspot`. For the atomic path this is a single commit;
    /// for the legacy fallback it is `set_cursor2` + `move_cursor`.
    /// Idempotent — repeated calls just re-bind and reposition.
    ///
    /// # Errors
    /// Ioctl or atomic-commit failure.
    pub fn show(
        &mut self,
        crtc: crtc::Handle,
        hotspot: (i32, i32),
        img_x: i32,
        img_y: i32,
    ) -> io::Result<()> {
        if let (Some(state), Some(fb)) = (self.per_crtc.get(&crtc), self.fb) {
            let mut req = AtomicModeReq::new();
            req.add_raw_property(
                state.plane.into(),
                state.prop_fb_id,
                u64::from(u32::from(fb)),
            );
            req.add_raw_property(
                state.plane.into(),
                state.prop_crtc_id,
                u64::from(u32::from(crtc)),
            );
            req.add_raw_property(state.plane.into(), state.prop_crtc_x, img_x as i64 as u64);
            req.add_raw_property(state.plane.into(), state.prop_crtc_y, img_y as i64 as u64);
            req.add_raw_property(state.plane.into(), state.prop_crtc_w, u64::from(self.width));
            req.add_raw_property(
                state.plane.into(),
                state.prop_crtc_h,
                u64::from(self.height),
            );
            req.add_raw_property(state.plane.into(), state.prop_src_x, 0u64);
            req.add_raw_property(state.plane.into(), state.prop_src_y, 0u64);
            req.add_raw_property(
                state.plane.into(),
                state.prop_src_w,
                u64::from(self.width) << 16,
            );
            req.add_raw_property(
                state.plane.into(),
                state.prop_src_h,
                u64::from(self.height) << 16,
            );
            self.device
                .atomic_commit(AtomicCommitFlags::NONBLOCK, req)?;
            self.visible.insert(crtc, true);
            Ok(())
        } else {
            // Legacy fallback (works on Intel; may be a no-op on AMD with
            // DRM_CLIENT_CAP_ATOMIC, hence the atomic path above).
            self.show_legacy(crtc, hotspot, img_x, img_y)
        }
    }

    #[allow(deprecated)]
    fn show_legacy(
        &mut self,
        crtc: crtc::Handle,
        hotspot: (i32, i32),
        img_x: i32,
        img_y: i32,
    ) -> io::Result<()> {
        let Some(dumb) = self.dumb.as_ref() else {
            return Err(io::Error::other("cursor plane already destroyed"));
        };
        self.device.set_cursor2(crtc, Some(dumb), hotspot)?;
        self.visible.insert(crtc, true);
        self.device.move_cursor(crtc, (img_x, img_y))?;
        Ok(())
    }

    /// Detach the cursor from `crtc`. The plane buffer is retained so
    /// a future `show` doesn't have to re-allocate.
    ///
    /// # Errors
    /// Atomic-commit or `set_cursor2` ioctl failure.
    pub fn hide(&mut self, crtc: crtc::Handle) -> io::Result<()> {
        if let Some(state) = self.per_crtc.get(&crtc) {
            let mut req = AtomicModeReq::new();
            req.add_raw_property(state.plane.into(), state.prop_fb_id, 0u64);
            req.add_raw_property(state.plane.into(), state.prop_crtc_id, 0u64);
            self.device
                .atomic_commit(AtomicCommitFlags::NONBLOCK, req)?;
            self.visible.insert(crtc, false);
            Ok(())
        } else {
            self.hide_legacy(crtc)
        }
    }

    #[allow(deprecated)]
    fn hide_legacy(&mut self, crtc: crtc::Handle) -> io::Result<()> {
        self.device.set_cursor2::<DumbBuffer>(crtc, None, (0, 0))?;
        self.visible.insert(crtc, false);
        Ok(())
    }

    /// Move the cursor on `crtc` to image-top-left `(x, y)` in
    /// CRTC-local coordinates. Uses an atomic position-only commit
    /// when available; falls back to `move_cursor` ioctl otherwise.
    ///
    /// # Errors
    /// Atomic-commit or `move_cursor` ioctl failure.
    pub fn move_to(&self, crtc: crtc::Handle, x: i32, y: i32) -> io::Result<()> {
        if let Some(state) = self.per_crtc.get(&crtc) {
            let mut req = AtomicModeReq::new();
            req.add_raw_property(state.plane.into(), state.prop_crtc_x, x as i64 as u64);
            req.add_raw_property(state.plane.into(), state.prop_crtc_y, y as i64 as u64);
            self.device.atomic_commit(AtomicCommitFlags::NONBLOCK, req)
        } else {
            #[allow(deprecated)]
            self.device.move_cursor(crtc, (x, y))
        }
    }

    /// True iff the plane is currently bound (via `show`) on `crtc`.
    #[must_use]
    pub fn is_visible_on(&self, crtc: crtc::Handle) -> bool {
        self.visible.get(&crtc).copied().unwrap_or(false)
    }

    /// True iff the plane is currently bound on at least one CRTC.
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.visible.values().any(|&v| v)
    }

    /// Iterate every CRTC the plane has ever been bound or hidden
    /// against.
    pub fn known_crtcs(&self) -> impl Iterator<Item = crtc::Handle> + '_ {
        self.visible.keys().copied()
    }

    /// Cursor plane width in pixels (driver-reported).
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Cursor plane height in pixels (driver-reported).
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Diagnostic: write the current dumb buffer contents to a PPM file
    /// at `path`. Reads the kernel-visible bytes (respecting `self.stride`)
    /// so a stride/pitch mismatch between what `load_image` writes and
    /// what the display engine samples shows up as a visible distortion
    /// in the dump.
    ///
    /// # Errors
    /// File I/O failure.
    pub fn dump_to_ppm(&self, path: &str) -> io::Result<()> {
        use std::io::Write;
        let mut file = std::fs::File::create(path)?;
        let w = self.width;
        let h = self.height;
        file.write_all(format!("P6\n{w} {h}\n255\n").as_bytes())?;
        let mut row_buf = vec![0u8; (w as usize) * 3];
        for y in 0..h as usize {
            let row_start = y * (self.stride as usize);
            for x in 0..w as usize {
                let pi = row_start + x * 4;
                // ARGB8888 in little-endian on the wire is B, G, R, A bytes.
                let b = unsafe { *self.ptr.as_ptr().add(pi) };
                let g = unsafe { *self.ptr.as_ptr().add(pi + 1) };
                let r = unsafe { *self.ptr.as_ptr().add(pi + 2) };
                row_buf[x * 3] = r;
                row_buf[x * 3 + 1] = g;
                row_buf[x * 3 + 2] = b;
            }
            file.write_all(&row_buf)?;
        }
        log::info!("cursor: dumped {path} ({w}x{h}, stride={})", self.stride);
        Ok(())
    }
}

impl Drop for CursorPlane {
    fn drop(&mut self) {
        // Best-effort: hide cursor on all known CRTCs before releasing resources.
        let crtcs: Vec<crtc::Handle> = self.known_crtcs().collect();
        for crtc in crtcs {
            if self.visible.get(&crtc).copied().unwrap_or(false)
                && let Err(e) = self.hide(crtc)
            {
                log::debug!("cursor: hide on drop for {crtc:?} failed: {e}");
            }
        }
        if let Some(fb) = self.fb.take() {
            let _ = self.device.destroy_framebuffer(fb);
        }
        if let Some(dumb) = self.dumb.take() {
            let _ = self.device.destroy_dumb_buffer(dumb);
        }
    }
}

/// Thin wrapper so `DumbBuffer` can be passed to `add_planar_framebuffer`.
struct CursorDumbFb<'a> {
    dumb: &'a DumbBuffer,
    stride: u32,
    width: u32,
    height: u32,
}

impl DrmPlanarBuffer for CursorDumbFb<'_> {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn format(&self) -> DrmFourcc {
        DrmFourcc::Argb8888
    }
    fn modifier(&self) -> Option<drm::buffer::DrmModifier> {
        None
    }
    fn pitches(&self) -> [u32; 4] {
        [self.stride, 0, 0, 0]
    }
    fn handles(&self) -> [Option<DrmBufferHandle>; 4] {
        [Some(self.dumb.handle()), None, None, None]
    }
    fn offsets(&self) -> [u32; 4] {
        [0, 0, 0, 0]
    }
}

/// Discover cursor plane handles and their property handles for each
/// requested CRTC. Uses `DRM_CLIENT_CAP_UNIVERSAL_PLANES` plane list
/// (already opted-in by `Device::enable_atomic_capabilities`).
///
/// A cursor plane matches a CRTC when `resources.filter_crtcs` returns
/// that CRTC for the plane's `possible_crtcs` filter.
fn discover_cursor_planes(
    device: &Device,
    crtcs: &[crtc::Handle],
    _fb: framebuffer::Handle,
) -> io::Result<HashMap<crtc::Handle, PerCrtcAtomicState>> {
    let resources = device.resource_handles()?;
    let mut result: HashMap<crtc::Handle, PerCrtcAtomicState> = HashMap::new();

    for ph in device.plane_handles()? {
        // Determine plane type by matching the `type` property value.
        let props = device.get_properties(ph)?;
        let prop_map_raw = props.as_hashmap(device)?;
        let Some(type_info) = prop_map_raw.get("type") else {
            continue;
        };
        let type_val = props
            .iter()
            .find(|(h, _)| **h == type_info.handle())
            .map(|(_, v)| *v)
            .unwrap_or(0);
        if type_val != PlaneType::Cursor as u64 {
            continue;
        }

        let plane_info = device.get_plane(ph)?;
        let drivable: std::collections::HashSet<crtc::Handle> = resources
            .filter_crtcs(plane_info.possible_crtcs())
            .into_iter()
            .collect();

        for &want_crtc in crtcs {
            if result.contains_key(&want_crtc) {
                continue; // already assigned a cursor plane to this CRTC
            }
            if !drivable.contains(&want_crtc) {
                continue; // this plane can't drive this CRTC
            }

            match PropMap::for_object(device, ph) {
                Ok(prop_map) => {
                    let state = (|| -> io::Result<PerCrtcAtomicState> {
                        Ok(PerCrtcAtomicState {
                            plane: ph,
                            prop_fb_id: prop_map.id("FB_ID")?,
                            prop_crtc_id: prop_map.id("CRTC_ID")?,
                            prop_crtc_x: prop_map.id("CRTC_X")?,
                            prop_crtc_y: prop_map.id("CRTC_Y")?,
                            prop_crtc_w: prop_map.id("CRTC_W")?,
                            prop_crtc_h: prop_map.id("CRTC_H")?,
                            prop_src_x: prop_map.id("SRC_X")?,
                            prop_src_y: prop_map.id("SRC_Y")?,
                            prop_src_w: prop_map.id("SRC_W")?,
                            prop_src_h: prop_map.id("SRC_H")?,
                        })
                    })();
                    match state {
                        Ok(s) => {
                            result.insert(want_crtc, s);
                        }
                        Err(e) => {
                            log::warn!(
                                "cursor: property lookup failed for cursor plane {ph:?} \
                                 on CRTC {want_crtc:?}: {e}; using legacy ioctl fallback"
                            );
                        }
                    }
                }
                Err(e) => {
                    log::warn!(
                        "cursor: get_properties failed for plane {ph:?}: {e}; \
                         using legacy ioctl fallback for CRTC {want_crtc:?}"
                    );
                }
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase B regression: `is_visible_on` tracks per-CRTC binding
    /// independently.
    #[test]
    fn visibility_is_per_crtc() {
        let mut visible: HashMap<crtc::Handle, bool> = HashMap::new();
        let crtc_a: crtc::Handle = ::drm::control::from_u32(11).unwrap();
        let crtc_b: crtc::Handle = ::drm::control::from_u32(12).unwrap();

        visible.insert(crtc_a, true);
        assert!(visible.get(&crtc_a).copied().unwrap_or(false));
        assert!(!visible.get(&crtc_b).copied().unwrap_or(false));

        visible.insert(crtc_b, true);
        assert!(visible.get(&crtc_a).copied().unwrap_or(false));
        assert!(visible.get(&crtc_b).copied().unwrap_or(false));

        visible.insert(crtc_a, false);
        assert!(!visible.get(&crtc_a).copied().unwrap_or(false));
        assert!(visible.get(&crtc_b).copied().unwrap_or(false));
    }

    /// Phase B regression test for the unavailable-plane path. The
    /// v2 `PlatformBackend::for_tests()` fixture has no real DRM
    /// device, so `cursor_plane` is `None`. The hooks must surface
    /// that cleanly via `Err(io::Error::other(...))` rather than
    /// panicking — every Phase D' recovery path relies on this so
    /// VT-leave / shutdown / drain_all hooks can fire blindly.
    #[test]
    fn unavailable_plane_returns_err_not_panic() {
        use crate::kms::v2::platform::PlatformBackend;

        let mut p = PlatformBackend::for_tests();
        assert!(!p.cursor_plane_available());
        assert!(
            p.cursor_plane_upload_image(1, 16, 16, &[0u8; 16 * 16 * 4])
                .is_err()
        );
        assert!(p.cursor_plane_show_on_crtc(0, 0, 0, 0, 0).is_err());
        assert!(p.cursor_plane_rebind_visible_crtcs(0, 0, 0, 0).is_err());
        assert!(p.cursor_plane_move(0, 0).is_err());
        assert!(p.cursor_plane_hide_on_crtc(0).is_err());
        assert!(p.cursor_plane_hide_all().is_err());
        assert!(p.cursor_plane_uploaded_version().is_none());
    }
}
