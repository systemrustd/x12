//! `Backend` trait — the surface `process_request` and the core loop
//! call on the host backend. Exists primarily as a seam for testing
//! (`RecordingBackend` lives next door, gated `#[cfg(test)]`) and so
//! that the KMS backend can sit alongside the host-X11 backend without
//! touching every call site.
//!
//! Method signatures mirror the existing `HostX11Backend::*` methods
//! 1:1 — no `Param` structs, no parameter renaming. Several methods
//! still take raw `u32` host xids rather than handle newtypes — call
//! sites pass `u32` from the `ResourceTable`'s `host_xid` field, and
//! rewrapping/unwrapping at every call boundary is noise.

use std::{any::Any, io};

use yserver_protocol::x11::{ClipRectangles, FontMetrics, xfixes};

use crate::{
    backend::{
        AnyHandle, ClipState, CursorHandle, DrawState, FillState, FontHandle, GlyphSetHandle,
        OriginContext, PictureHandle, PixmapHandle, WindowHandle,
    },
    core_loop::HostInputEvent,
    host_x11::{HostEvent, HostSubwindowConfig, HostSubwindowVisual, HostXidMap, PointerPosition},
    server::ServerState,
};

use yserver_protocol::x11::ResourceId;

/// Categorises the raw fds a backend wants the core's mio poller to
/// watch on its behalf (returned by `Backend::poll_fds`).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BackendFdKind {
    /// libinput's epoll fd. Readiness is dispatched to the libinput
    /// thread (KMS) — but the fd inventory still flows through the
    /// trait so the core's poller can register it uniformly.
    Libinput,
    /// DRM device fd; readiness drives `on_page_flip_ready`.
    Drm,
    /// Host X11 connection fd (ynest only); readiness drives
    /// `Backend::drain_host_socket` on the core thread.
    HostX11,
}

/// Outcome of a single `Backend::drain_host_socket` pass. Re-exported
/// from `host_x11` so non-host-X11 backends can spell the type
/// without depending on the host module.
pub use crate::host_x11::HostSocketStatus;

/// The dynamic backend surface. `Send` is required so that
/// `Arc<Mutex<dyn Backend>>` is `Send + Sync` (`Mutex<T>` is Sync iff
/// `T: Send`). `Sync` on the trait itself is not required because all
/// `Backend` access is mediated through a `Mutex`.
pub trait Backend: Send {
    // ──────────────────────────────────────────────────────────────
    // Lifecycle / state accessors
    // ──────────────────────────────────────────────────────────────

    fn window_id(&self) -> u32;
    fn root_visual_xid(&self) -> u32;
    fn argb_visual_xid(&self) -> Option<u32>;
    fn argb_colormap_xid(&self) -> Option<u32>;
    fn render_opcode(&self) -> Option<u8>;
    fn xkb_opcode(&self) -> Option<u8>;
    fn xkb_info(&self) -> Option<(u8, u8, u8)>;
    fn composite_opcode(&self) -> Option<u8>;
    fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32>;
    fn ping(&mut self, origin: Option<OriginContext>) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Single-threaded core hooks
    // ──────────────────────────────────────────────────────────────

    /// Dispatch a host input event the core received over the
    /// `Message::HostInput` channel. The backend produces zero or more
    /// X11 wire events via `state` fanout helpers. Filled in by E2
    /// (KMS) and F2 (host-X11); inert until then.
    fn on_host_input(&mut self, state: &mut ServerState, ev: HostInputEvent);

    /// DRM page-flip completion fd is readable. The backend should
    /// drain completion events and submit the next composite/flip.
    fn on_page_flip_ready(&mut self, state: &mut ServerState);

    /// Raw fds the core's poller should watch on this backend's behalf.
    /// The core registers each fd against the matching token derived
    /// from `BackendFdKind`.
    fn poll_fds(&self) -> Vec<(std::os::fd::RawFd, BackendFdKind)>;

    /// Downcast to `Any` for backend-specific operations (e.g. KMS composite).
    fn as_any(&self) -> &dyn Any;
    /// Mutable downcast to `Any` for backend-specific operations.
    fn as_any_mut(&mut self) -> &mut dyn Any;

    // ──────────────────────────────────────────────────────────────
    // Subwindow lifecycle
    // ──────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn create_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_parent: WindowHandle,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        border_width: u16,
        visual: HostSubwindowVisual,
        background_pixel: Option<u32>,
        background_pixmap: Option<u32>,
    ) -> io::Result<WindowHandle>;

    fn destroy_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32)
    -> io::Result<()>;

    fn map_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn unmap_subwindow(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn configure_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()>;

    fn reparent_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        host_parent: u32,
        x: i16,
        y: i16,
    ) -> io::Result<()>;

    fn change_subwindow_attributes(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        value_mask: u32,
        values: &[u32],
    ) -> io::Result<()>;

    fn update_host_event_mask(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        mask: u32,
        enabled: bool,
    ) -> io::Result<()>;

    /// Phase 6.3 Step 4: register a freshly-created host top-level so
    /// pointer / expose events on it route to `nested_id` through
    /// `pointer_event_fanout` / `expose_event_fanout`. Replaces the
    /// pre-Step-4 pump-handle `register_top_level`.
    fn register_top_level(
        &mut self,
        origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()>;

    /// Phase 6.3 Step 4: same as [`Backend::register_top_level`] but
    /// for sub-windows — selects `Exposure` only so pointer events
    /// bubble up to the top-level ancestor where dispatch lives.
    fn register_subwindow(
        &mut self,
        origin: Option<OriginContext>,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()>;

    /// Phase 6.3 Step 4: drop a `host_xid → ResourceId` mapping at
    /// DestroyWindow / Reparent-out so stale host events never
    /// misroute.
    fn unregister_host_window(&mut self, host_xid: u32);

    /// View of the `host_xid → ResourceId` map. F2: plain HashMap —
    /// the core thread is the only writer/reader, so no Arc/Mutex.
    fn xid_map(&self) -> &HostXidMap;

    /// F2: drain whatever the kernel has buffered on the host fd and
    /// classify into `pending_replies` / `pending_events`. KMS
    /// backend has no host fd; the default no-op suits.
    fn drain_host_socket(&mut self) -> io::Result<HostSocketStatus> {
        Ok(HostSocketStatus::WouldBlock)
    }

    /// F2: pop the next decoded host event so the core can fan out at
    /// the outer-loop boundary. Default `None` — only HostX11Backend
    /// produces these.
    fn pop_pending_host_event(&mut self) -> Option<HostEvent> {
        None
    }

    /// F2: did the host close the connection? `run_core` posts
    /// `Message::Shutdown` once this flips true.
    fn host_socket_eof(&self) -> bool {
        false
    }

    fn name_window_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_window: WindowHandle,
    ) -> io::Result<PixmapHandle>;

    // ──────────────────────────────────────────────────────────────
    // Resources (pixmap, font, cursor)
    // ──────────────────────────────────────────────────────────────

    fn create_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<PixmapHandle>;

    fn free_pixmap(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn open_font(
        &mut self,
        origin: Option<OriginContext>,
        name: &str,
    ) -> io::Result<(FontHandle, FontMetrics)>;

    fn close_font(&mut self, origin: Option<OriginContext>, host_xid: u32) -> io::Result<()>;

    fn create_cursor(
        &mut self,
        origin: Option<OriginContext>,
        source_pixmap: PixmapHandle,
        mask_pixmap: Option<PixmapHandle>,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<CursorHandle>;

    /// Free a host cursor. Counterpart to `create_cursor`. Default impl
    /// is a no-op so backends without a real X-server peer (KMS) need
    /// no boilerplate.
    fn free_cursor(&mut self, _origin: Option<OriginContext>, _host_xid: u32) -> io::Result<()> {
        Ok(())
    }

    fn define_cursor(
        &mut self,
        origin: Option<OriginContext>,
        host_window_xid: u32,
        cursor_host_xid: u32,
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Container background (root-mapped helpers)
    // ──────────────────────────────────────────────────────────────

    fn set_container_background_pixel(
        &mut self,
        origin: Option<OriginContext>,
        pixel: u32,
    ) -> io::Result<()>;

    fn set_container_background_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap_xid: u32,
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // GC state (sync points feeding the host's shared GC)
    // ──────────────────────────────────────────────────────────────

    fn clear_clip_rectangles(&mut self, origin: Option<OriginContext>) -> io::Result<()>;

    fn set_clip_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        clip: Option<ClipRectangles>,
    ) -> io::Result<()>;

    fn set_clip_pixmap(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
    ) -> io::Result<()>;

    fn set_gc_fill_solid(&mut self, origin: Option<OriginContext>) -> io::Result<()>;

    fn set_gc_fill_tiled(
        &mut self,
        origin: Option<OriginContext>,
        host_pixmap: u32,
        tile_x_origin: i16,
        tile_y_origin: i16,
    ) -> io::Result<()>;

    fn apply_clip_state(
        &mut self,
        origin: Option<OriginContext>,
        clip: &ClipState,
    ) -> io::Result<()>;

    fn apply_fill_state(
        &mut self,
        origin: Option<OriginContext>,
        fill: &FillState,
    ) -> io::Result<()>;

    fn apply_draw_state(
        &mut self,
        origin: Option<OriginContext>,
        state: &DrawState,
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Drawing primitives
    //
    // These match the existing `HostX11Backend::*` signatures: they
    // take raw `u32` host xids and a foreground colour (the Phase 6.2
    // additive scope adds `&DrawState` propagation through the
    // `apply_draw_state` sync point above; the methods themselves are
    // unchanged). Phase 6.3+ may collapse the surface as the trait
    // grows additional impls, but for now matching the existing shape
    // keeps Step 5 churn-free at every call site.
    // ──────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn copy_area(
        &mut self,
        origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn copy_plane(
        &mut self,
        origin: Option<OriginContext>,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        plane: u32,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn put_image(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn get_image(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>>;

    fn poly_line(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn poly_segment(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()>;

    fn poly_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()>;

    fn poly_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()>;

    fn poly_point(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn poly_fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()>;

    fn poly_fill_arc(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()>;

    fn fill_poly(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()>;

    fn fill_rectangle(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()>;

    fn poly_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn poly_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn image_text8(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()>;

    fn image_text16(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // RENDER
    // ──────────────────────────────────────────────────────────────

    fn render_create_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_drawable: AnyHandle,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_change_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_free_picture(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
    ) -> io::Result<()>;

    fn render_create_glyphset(
        &mut self,
        origin: Option<OriginContext>,
        ynest_format: u32,
    ) -> io::Result<Option<GlyphSetHandle>>;

    fn render_free_glyphset(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
    ) -> io::Result<()>;

    fn render_add_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
        body_tail: &[u8],
    ) -> io::Result<()>;

    fn render_free_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        host_gs: u32,
        glyph_ids: &[u8],
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_composite(
        &mut self,
        origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_mask: u32,
        host_dst: u32,
        src_x: i16,
        src_y: i16,
        mask_x: i16,
        mask_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_composite_glyphs(
        &mut self,
        origin: Option<OriginContext>,
        minor: u8,
        op: u8,
        host_src: u32,
        host_dst: u32,
        mask_fmt: u32,
        host_gs: u32,
        src_x: i16,
        src_y: i16,
        items: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    fn render_fill_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    #[allow(clippy::too_many_arguments)]
    fn render_trapezoids(
        &mut self,
        origin: Option<OriginContext>,
        op: u8,
        host_src: u32,
        host_dst: u32,
        host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()>;

    /// RENDER `Triangles` (minor 11), `TriStrip` (12), `TriFan` (13).
    /// `primitives` is the wire body after the fixed prefix:
    /// 24-byte `XTriangle`s for minor 11, packed `XPointFixed`s
    /// (8 bytes each) for 12 and 13.
    #[allow(clippy::too_many_arguments)]
    fn render_triangles_op(
        &mut self,
        _origin: Option<OriginContext>,
        _minor: u8,
        _op: u8,
        _host_src: u32,
        _host_dst: u32,
        _host_mask_format: u32,
        _src_x: i16,
        _src_y: i16,
        _primitives: &[u8],
        _x_off: i16,
        _y_off: i16,
    ) -> io::Result<()> {
        Ok(())
    }

    fn render_create_solid_fill(
        &mut self,
        origin: Option<OriginContext>,
        color: [u8; 8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_create_linear_gradient(
        &mut self,
        origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_create_radial_gradient(
        &mut self,
        origin: Option<OriginContext>,
        body: &[u8],
    ) -> io::Result<Option<PictureHandle>>;

    fn render_create_cursor(
        &mut self,
        origin: Option<OriginContext>,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>>;

    fn render_set_picture_clip_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_set_picture_filter(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_set_picture_transform(
        &mut self,
        origin: Option<OriginContext>,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()>;

    fn render_query_version(&mut self, origin: Option<OriginContext>) -> io::Result<(u32, u32)>;

    // ──────────────────────────────────────────────────────────────
    // Other extensions
    // ──────────────────────────────────────────────────────────────

    fn xkb_proxy(
        &mut self,
        origin: Option<OriginContext>,
        minor: u8,
        body: &[u8],
    ) -> io::Result<Option<Vec<u8>>>;

    fn xfixes_change_cursor_by_name(
        &mut self,
        origin: Option<OriginContext>,
        host_cursor_xid: u32,
        name_bytes: &[u8],
    ) -> io::Result<()>;

    fn set_shape_rectangles(
        &mut self,
        origin: Option<OriginContext>,
        host_xid: u32,
        kind: u8,
        rects: &[xfixes::RegionRect],
    ) -> io::Result<()>;

    // ──────────────────────────────────────────────────────────────
    // Misc
    // ──────────────────────────────────────────────────────────────

    fn warp_pointer(
        &mut self,
        origin: Option<OriginContext>,
        dst_host_xid: u32,
        dst_x: i16,
        dst_y: i16,
    ) -> io::Result<()>;

    fn query_pointer(&mut self, origin: Option<OriginContext>) -> io::Result<PointerPosition>;

    fn list_fonts_proxy(
        &mut self,
        origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<u8>>;

    fn list_fonts_with_info_proxy(
        &mut self,
        origin: Option<OriginContext>,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<Vec<u8>>>;

    fn get_atom_name(
        &mut self,
        origin: Option<OriginContext>,
        atom: u32,
    ) -> io::Result<Option<String>>;

    fn get_keyboard_mapping(
        &mut self,
        origin: Option<OriginContext>,
        first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)>;

    fn get_modifier_mapping(&mut self, origin: Option<OriginContext>) -> io::Result<(u8, Vec<u8>)>;
}

// Compile-time assertion that `Backend` is object-safe and that the
// `Arc<Mutex<dyn Backend>>` shape used by the hot-path call sites is
// `Send + Sync` (so worker threads can hold it).
const _: fn() = || {
    fn assert_obj_safe(_: &dyn Backend) {}
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<std::sync::Arc<std::sync::Mutex<dyn Backend>>>();
    let _ = assert_obj_safe;
};
