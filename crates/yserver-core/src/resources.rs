#![allow(dead_code)]

use std::collections::HashMap;

use yserver_protocol::x11::{
    AtomId, ChangeWindowAttributesRequest, ClientId, ClipRectangles, ConfigureWindowRequest,
    CreateGcRequest, CreatePixmapRequest, CreateWindowRequest, FontMetrics, GcChange,
    ReparentWindowRequest, ResourceId, SetClipRectanglesRequest,
};

use crate::{
    backend::{
        ArcMode, CapStyle, ClipState, DrawState, FillRule, FillState, FillStyle, FontHandle,
        GcFunction, JoinStyle, LineStyle, SubwindowMode,
    },
    properties::PropertyValue,
};

pub const SERVER_OWNER: ClientId = ClientId(0);

#[derive(Debug, Default)]
pub struct ClientRemovedResources {
    pub closed_fonts: Vec<u32>,
    pub freed_pixmaps: Vec<u32>,
    pub freed_pictures: Vec<(u32, Option<u32>)>,
    pub freed_glyphsets: Vec<u32>,
    pub freed_cursors: Vec<u32>,
}

pub const ROOT_WINDOW: ResourceId = ResourceId(0x100);
pub const ROOT_COLORMAP: ResourceId = ResourceId(0x101);
pub const ROOT_VISUAL: ResourceId = ResourceId(0x102);
pub const ARGB_VISUAL: ResourceId = ResourceId(0x103);
pub const ARGB_COLORMAP: ResourceId = ResourceId(0x104);

/// X11 visual class codes (subset we care about). The setup reply
/// advertises these per-visual; clients pass a visual ID into
/// `CreateWindow`, `CreateColormap`, and RENDER `CreatePicture`.
pub const VISUAL_CLASS_TRUE_COLOR: u8 = 4;

/// A visual exposed to clients via the setup reply. We currently
/// expose a fixed pair (root TrueColor at 24-bit, ARGB TrueColor at
/// 32-bit). The `host_visual_xid` field is filled in once we've
/// probed the host server's setup; it stays `None` until then so
/// that early CreateWindow forwarding can be detected and skipped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Visual {
    pub id: ResourceId,
    pub class: u8,
    pub depth: u8,
    pub bits_per_rgb: u8,
    pub colormap_entries: u16,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub host_visual_xid: Option<crate::backend::VisualHandle>,
}

/// A colormap. We currently expose one per visual (root colormap +
/// ARGB colormap). The `host_colormap_xid` is allocated on the host
/// once during `HostX11` init and pushed in via
/// [`ResourceTable::set_colormap_host_xid`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Colormap {
    pub id: ResourceId,
    pub visual: ResourceId,
    pub host_colormap_xid: Option<crate::backend::ColormapHandle>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostDrawableTarget {
    Window {
        nested: ResourceId,
        host_xid: crate::backend::WindowHandle,
        depth: u8,
    },
    Pixmap {
        nested: ResourceId,
        host_xid: crate::backend::PixmapHandle,
        width: u16,
        height: u16,
        depth: u8,
    },
}

impl HostDrawableTarget {
    pub fn host_xid(self) -> u32 {
        match self {
            Self::Window { host_xid, .. } => host_xid.as_raw(),
            Self::Pixmap { host_xid, .. } => host_xid.as_raw(),
        }
    }

    pub fn host_handle(self) -> crate::backend::AnyHandle {
        match self {
            Self::Window { host_xid, .. } => crate::backend::AnyHandle::Window(host_xid),
            Self::Pixmap { host_xid, .. } => crate::backend::AnyHandle::Pixmap(host_xid),
        }
    }

    pub fn depth(self) -> u8 {
        match self {
            Self::Window { depth, .. } | Self::Pixmap { depth, .. } => depth,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExposedRect {
    pub window: ResourceId,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReparentResult {
    pub window: ResourceId,
    pub old_parent: ResourceId,
    pub new_parent: ResourceId,
    pub x: i16,
    pub y: i16,
    pub override_redirect: bool,
    pub host_xid: Option<crate::backend::WindowHandle>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReparentWindowError {
    BadWindow,
    BadMatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureKind {
    /// Backed by a real drawable (window or pixmap) — valid as a
    /// Composite/Trapezoids/Triangles/FillRectangles/CompositeGlyphs
    /// destination.
    Drawable,
    /// Backed by a 1x1 SolidFill / LinearGradient / RadialGradient /
    /// ConicalGradient. Has no underlying drawable, so cannot be
    /// used as a destination — RENDER opcodes that try must raise
    /// `BadDrawable`.
    Sourceless,
}

#[derive(Debug)]
pub struct PictureState {
    pub client: ClientId,
    pub host_picture_xid: crate::backend::PictureHandle,
    pub host_owned_pixmap: Option<crate::backend::PixmapHandle>,
    pub kind: PictureKind,
}

#[derive(Debug)]
pub struct GlyphSetState {
    pub client: ClientId,
    pub host_glyphset_xid: crate::backend::GlyphSetHandle,
}

#[derive(Debug)]
pub struct ResourceTable {
    windows: HashMap<u32, Window>,
    pixmaps: HashMap<u32, Pixmap>,
    gcs: HashMap<u32, Gc>,
    fonts: HashMap<u32, Font>,
    cursors: HashMap<u32, Cursor>,
    pub pictures: HashMap<u32, PictureState>,
    pub glyphsets: HashMap<u32, GlyphSetState>,
    host_glyphset_refcounts: HashMap<u32, usize>,
    visuals: HashMap<u32, Visual>,
    colormaps: HashMap<u32, Colormap>,
}

impl ResourceTable {
    pub fn new() -> Self {
        let mut windows = HashMap::new();
        windows.insert(
            ROOT_WINDOW.0,
            Window {
                id: ROOT_WINDOW,
                parent: ROOT_WINDOW,
                children: Vec::new(),
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                border_width: 0,
                depth: 24,
                visual: ROOT_VISUAL,
                class: WindowClass::InputOutput,
                map_state: MapState::Viewable,
                background_pixel: 0x00ff_ffff,
                background_pixmap: None,
                background_pixmap_host_xid: None,
                border_pixmap_host_xid: None,
                override_redirect: false,
                cursor: None,
                owner: SERVER_OWNER,
                properties: HashMap::new(),
                host_xid: None,
                composite_named_pixmaps: Vec::new(),
            },
        );

        // Seed the visual + colormap tables with the same pair the setup
        // reply advertises. `host_visual_xid` / `host_colormap_xid` stay
        // `None` until `HostX11` init pushes the probed values in via
        // [`set_visual_host_xid`] / [`set_colormap_host_xid`].
        let mut visuals = HashMap::new();
        visuals.insert(
            ROOT_VISUAL.0,
            Visual {
                id: ROOT_VISUAL,
                class: VISUAL_CLASS_TRUE_COLOR,
                depth: 24,
                bits_per_rgb: 8,
                colormap_entries: 256,
                red_mask: 0x00ff_0000,
                green_mask: 0x0000_ff00,
                blue_mask: 0x0000_00ff,
                alpha_mask: 0,
                host_visual_xid: None,
            },
        );
        visuals.insert(
            ARGB_VISUAL.0,
            Visual {
                id: ARGB_VISUAL,
                class: VISUAL_CLASS_TRUE_COLOR,
                depth: 32,
                bits_per_rgb: 8,
                colormap_entries: 256,
                red_mask: 0x00ff_0000,
                green_mask: 0x0000_ff00,
                blue_mask: 0x0000_00ff,
                alpha_mask: 0xff00_0000,
                host_visual_xid: None,
            },
        );

        let mut colormaps = HashMap::new();
        colormaps.insert(
            ROOT_COLORMAP.0,
            Colormap {
                id: ROOT_COLORMAP,
                visual: ROOT_VISUAL,
                host_colormap_xid: None,
            },
        );
        colormaps.insert(
            ARGB_COLORMAP.0,
            Colormap {
                id: ARGB_COLORMAP,
                visual: ARGB_VISUAL,
                host_colormap_xid: None,
            },
        );

        Self {
            windows,
            pixmaps: HashMap::new(),
            gcs: HashMap::new(),
            fonts: HashMap::new(),
            cursors: HashMap::new(),
            pictures: HashMap::new(),
            glyphsets: HashMap::new(),
            host_glyphset_refcounts: HashMap::new(),
            visuals,
            colormaps,
        }
    }

    pub fn visual(&self, id: ResourceId) -> Option<&Visual> {
        self.visuals.get(&id.0)
    }

    /// Returns `true` if `id` corresponds to any allocated resource
    /// (window, pixmap, gc, font, cursor, colormap, picture, or
    /// glyphset). Used by `CreateXxx` opcodes to detect a
    /// `BadIDChoice` violation when a client tries to reuse an ID.
    pub fn xid_in_use(&self, id: ResourceId) -> bool {
        let id = id.0;
        self.windows.contains_key(&id)
            || self.pixmaps.contains_key(&id)
            || self.gcs.contains_key(&id)
            || self.fonts.contains_key(&id)
            || self.cursors.contains_key(&id)
            || self.colormaps.contains_key(&id)
            || self.pictures.contains_key(&id)
            || self.glyphsets.contains_key(&id)
    }

    pub fn visuals_iter(&self) -> impl Iterator<Item = &Visual> {
        self.visuals.values()
    }

    pub fn is_known_visual(&self, id: ResourceId) -> bool {
        self.visuals.contains_key(&id.0)
    }

    pub fn set_visual_host_xid(&mut self, id: ResourceId, host_xid: u32) -> bool {
        match self.visuals.get_mut(&id.0) {
            Some(v) => {
                v.host_visual_xid = crate::backend::VisualHandle::from_raw(host_xid);
                true
            }
            None => false,
        }
    }

    pub fn create_colormap(&mut self, id: ResourceId, visual: ResourceId) {
        self.colormaps.insert(
            id.0,
            Colormap {
                id,
                visual,
                host_colormap_xid: None,
            },
        );
    }

    pub fn colormap(&self, id: ResourceId) -> Option<&Colormap> {
        self.colormaps.get(&id.0)
    }

    pub fn set_colormap_host_xid(&mut self, id: ResourceId, host_xid: u32) -> bool {
        match self.colormaps.get_mut(&id.0) {
            Some(c) => {
                c.host_colormap_xid = crate::backend::ColormapHandle::from_raw(host_xid);
                true
            }
            None => false,
        }
    }

    /// First colormap entry whose `visual` matches; we currently keep
    /// one colormap per visual so this is unambiguous.
    pub fn colormap_for_visual(&self, visual: ResourceId) -> Option<&Colormap> {
        self.colormaps.values().find(|c| c.visual == visual)
    }

    pub fn create_window(&mut self, owner: ClientId, request: CreateWindowRequest) {
        // CopyFromParent on visual / depth must inherit from the parent's
        // visual and depth, not from the root. ARGB-parent + CopyFromParent
        // child must produce an ARGB child; otherwise the child's host
        // CreateWindow would forward depth=24 against an ARGB parent and
        // produce a host BadMatch.
        let parent = self.windows.get(&request.parent.0);
        let resolved_depth = if request.depth == 0 {
            parent.map_or(24, |p| p.depth)
        } else {
            request.depth
        };
        let resolved_visual = if request.visual.0 == 0 {
            parent.map_or(ROOT_VISUAL, |p| p.visual)
        } else {
            request.visual
        };
        let window = Window {
            id: request.window,
            parent: request.parent,
            children: Vec::new(),
            x: request.x,
            y: request.y,
            width: request.width,
            height: request.height,
            border_width: request.border_width,
            depth: resolved_depth,
            visual: resolved_visual,
            class: WindowClass::from_protocol(request.class),
            map_state: MapState::Unmapped,
            background_pixel: request.background_pixel.unwrap_or(0x00ff_ffff),
            background_pixmap: None,
            background_pixmap_host_xid: None,
            border_pixmap_host_xid: None,
            override_redirect: request.override_redirect.unwrap_or(false),
            cursor: None,
            owner,
            properties: HashMap::new(),
            host_xid: None,
            composite_named_pixmaps: Vec::new(),
        };

        self.windows
            .entry(request.parent.0)
            .or_insert_with(|| Window::placeholder(request.parent))
            .children
            .push(request.window);
        self.windows.insert(request.window.0, window);
    }

    pub fn destroy_window(&mut self, id: ResourceId) -> Vec<ResourceId> {
        let mut destroyed = Vec::new();
        self.destroy_window_inner(id, &mut destroyed);
        destroyed
    }

    /// Walk the about-to-be-destroyed window subtree and collect every
    /// retained bg-pixmap host XID. Caller frees them on the host.
    pub fn collect_bg_pixmap_host_xids(&self, root: ResourceId) -> Vec<u32> {
        let mut out = Vec::new();
        self.collect_bg_pixmap_host_xids_inner(root, &mut out);
        out
    }

    fn collect_bg_pixmap_host_xids_inner(&self, id: ResourceId, out: &mut Vec<u32>) {
        let Some(window) = self.windows.get(&id.0) else {
            return;
        };
        if let Some(xid) = window.background_pixmap_host_xid {
            out.push(xid.as_raw());
        }
        for child in &window.children {
            self.collect_bg_pixmap_host_xids_inner(*child, out);
        }
    }

    fn destroy_window_inner(&mut self, id: ResourceId, destroyed: &mut Vec<ResourceId>) {
        // X11 spec: "If the argument window is a root window, then this
        // request has no effect." Without this guard a misbehaved client
        // (or xts5 Xlib4/XDestroyWindow assertion 5, which calls
        // `XDestroyWindow(root)` and verifies the root and a sibling
        // window are still valid) would silently delete the root entry
        // from the windows table — leaving every subsequent client to
        // receive `BadWindow` from `GetGeometry` / `QueryTree` /
        // `GetWindowAttributes` on `ROOT_WINDOW`.
        if id == ROOT_WINDOW {
            return;
        }
        let Some(window) = self.windows.remove(&id.0) else {
            return;
        };
        if let Some(parent) = self.windows.get_mut(&window.parent.0) {
            parent.children.retain(|child| *child != id);
        }
        destroyed.push(id);
        for child in window.children {
            self.destroy_window_inner(child, destroyed);
        }
    }

    /// Apply attribute changes. Returns the previous bg-pixmap host XID if it
    /// was replaced — the caller should free it on the host since the X server
    /// no longer needs it.
    pub fn change_window_attributes(
        &mut self,
        request: ChangeWindowAttributesRequest,
    ) -> Option<crate::backend::PixmapHandle> {
        let mut previous_bg_host_xid: Option<crate::backend::PixmapHandle> = None;
        let new_bg_host_xid: Option<Option<crate::backend::PixmapHandle>> =
            if let Some(bg_pixmap) = request.background_pixmap {
                if bg_pixmap.0 == 0 {
                    Some(None)
                } else {
                    let host = self.pixmaps.get(&bg_pixmap.0).and_then(|p| p.host_xid);
                    Some(host)
                }
            } else {
                None
            };

        if let Some(window) = self.windows.get_mut(&request.window.0) {
            if let Some(bg_pixmap) = request.background_pixmap {
                let new_resource_id = if bg_pixmap.0 == 0 {
                    None
                } else {
                    Some(bg_pixmap)
                };
                if window.background_pixmap_host_xid != new_bg_host_xid.flatten() {
                    previous_bg_host_xid = window.background_pixmap_host_xid;
                }
                window.background_pixmap = new_resource_id;
                if let Some(host) = new_bg_host_xid {
                    window.background_pixmap_host_xid = host;
                }
            }
            if let Some(background_pixel) = request.background_pixel {
                window.background_pixel = background_pixel;
            }
            if let Some(cursor) = request.cursor {
                window.cursor = Some(cursor);
            }
        }

        previous_bg_host_xid
    }

    pub fn configure_window(&mut self, request: ConfigureWindowRequest) -> Option<&Window> {
        {
            let window = self.windows.get_mut(&request.window.0)?;
            if let Some(x) = request.x {
                window.x = x;
            }
            if let Some(y) = request.y {
                window.y = y;
            }
            if let Some(width) = request.width {
                window.width = width;
            }
            if let Some(height) = request.height {
                window.height = height;
            }
            if let Some(border_width) = request.border_width {
                window.border_width = border_width;
            }
        }
        self.restack_window(request.window, request.sibling, request.stack_mode);
        self.windows.get(&request.window.0)
    }

    fn restack_window(
        &mut self,
        window_id: ResourceId,
        sibling: Option<ResourceId>,
        stack_mode: Option<u8>,
    ) {
        let Some(stack_mode) = stack_mode else {
            return;
        };
        let Some(parent_id) = self.windows.get(&window_id.0).map(|window| window.parent) else {
            return;
        };
        let Some(action) = self.resolve_restack_action(window_id, parent_id, sibling, stack_mode)
        else {
            return;
        };
        let Some(parent) = self.windows.get_mut(&parent_id.0) else {
            return;
        };
        let Some(index) = parent.children.iter().position(|child| *child == window_id) else {
            return;
        };
        match action {
            RestackAction::NoOp => {}
            RestackAction::Top => {
                let window = parent.children.remove(index);
                parent.children.push(window);
            }
            RestackAction::Bottom => {
                let window = parent.children.remove(index);
                parent.children.insert(0, window);
            }
            RestackAction::AboveSibling(sibling_id) => {
                let window = parent.children.remove(index);
                let sibling_index = parent
                    .children
                    .iter()
                    .position(|child| *child == sibling_id);
                let insert_at = sibling_index.map_or(parent.children.len(), |i| i + 1);
                parent.children.insert(insert_at, window);
            }
            RestackAction::BelowSibling(sibling_id) => {
                let window = parent.children.remove(index);
                let sibling_index = parent
                    .children
                    .iter()
                    .position(|child| *child == sibling_id);
                let insert_at = sibling_index.unwrap_or(0);
                parent.children.insert(insert_at, window);
            }
        }
    }

    /// Resolve a `ConfigureWindow` stack-mode + optional sibling to the
    /// concrete restack action per the X11 protocol. `None` means the
    /// request is malformed (window not in parent's child list, sibling
    /// not actually a sibling, unknown stack mode) and should be skipped.
    ///
    /// X11 stack-mode codes: 0=Above, 1=Below, 2=TopIf, 3=BottomIf,
    /// 4=Opposite. TopIf/BottomIf/Opposite are *conditional* on the
    /// current occlusion state; Above/Below are unconditional.
    fn resolve_restack_action(
        &self,
        window_id: ResourceId,
        parent_id: ResourceId,
        sibling: Option<ResourceId>,
        stack_mode: u8,
    ) -> Option<RestackAction> {
        let parent = self.windows.get(&parent_id.0)?;
        let window_index = parent.children.iter().position(|c| *c == window_id)?;
        if let Some(sibling_id) = sibling
            && !parent.children.contains(&sibling_id)
        {
            return None;
        }

        Some(match stack_mode {
            0 => match sibling {
                Some(sib) => RestackAction::AboveSibling(sib),
                None => RestackAction::Top,
            },
            1 => match sibling {
                Some(sib) => RestackAction::BelowSibling(sib),
                None => RestackAction::Bottom,
            },
            2 => {
                if self.any_sibling_occludes_window(parent_id, window_index, sibling) {
                    RestackAction::Top
                } else {
                    RestackAction::NoOp
                }
            }
            3 => {
                if self.window_occludes_any_sibling(parent_id, window_index, sibling) {
                    RestackAction::Bottom
                } else {
                    RestackAction::NoOp
                }
            }
            4 => {
                if self.any_sibling_occludes_window(parent_id, window_index, sibling) {
                    RestackAction::Top
                } else if self.window_occludes_any_sibling(parent_id, window_index, sibling) {
                    RestackAction::Bottom
                } else {
                    RestackAction::NoOp
                }
            }
            _ => return None,
        })
    }

    /// True iff some sibling currently stacked above `window` (at
    /// `window_index` in the parent's child list) overlaps it. Both
    /// windows must be mapped per X11's occlusion definition. With
    /// `sibling = Some(_)`, only that sibling is considered.
    fn any_sibling_occludes_window(
        &self,
        parent_id: ResourceId,
        window_index: usize,
        sibling: Option<ResourceId>,
    ) -> bool {
        let Some(parent) = self.windows.get(&parent_id.0) else {
            return false;
        };
        let Some(window_id) = parent.children.get(window_index) else {
            return false;
        };
        let Some(window) = self.windows.get(&window_id.0) else {
            return false;
        };
        if window.map_state == MapState::Unmapped {
            return false;
        }
        for (i, other_id) in parent.children.iter().enumerate() {
            if i <= window_index {
                continue;
            }
            if let Some(sib) = sibling
                && *other_id != sib
            {
                continue;
            }
            let Some(other) = self.windows.get(&other_id.0) else {
                continue;
            };
            if other.map_state == MapState::Unmapped {
                continue;
            }
            if window_rects_overlap(window, other) {
                return true;
            }
        }
        false
    }

    /// True iff `window` (at `window_index` in the parent's child list)
    /// currently occludes some sibling stacked below it. With `sibling =
    /// Some(_)`, only that sibling is considered.
    fn window_occludes_any_sibling(
        &self,
        parent_id: ResourceId,
        window_index: usize,
        sibling: Option<ResourceId>,
    ) -> bool {
        let Some(parent) = self.windows.get(&parent_id.0) else {
            return false;
        };
        let Some(window_id) = parent.children.get(window_index) else {
            return false;
        };
        let Some(window) = self.windows.get(&window_id.0) else {
            return false;
        };
        if window.map_state == MapState::Unmapped {
            return false;
        }
        for (i, other_id) in parent.children.iter().enumerate() {
            if i >= window_index {
                continue;
            }
            if let Some(sib) = sibling
                && *other_id != sib
            {
                continue;
            }
            let Some(other) = self.windows.get(&other_id.0) else {
                continue;
            };
            if other.map_state == MapState::Unmapped {
                continue;
            }
            if window_rects_overlap(window, other) {
                return true;
            }
        }
        false
    }

    #[must_use]
    pub fn map_window(&mut self, id: ResourceId) -> bool {
        // A window is Viewable only if it is mapped AND all ancestors
        // up to the root are also mapped (Viewable). If any ancestor
        // is not Viewable, the window becomes Unviewable instead.
        let parent_id = self.windows.get(&id.0).map(|w| w.parent);
        let parent_viewable = match parent_id {
            Some(pid) if pid.0 == id.0 => true, // root: mapping its parent (itself) is N/A
            Some(pid) => self
                .windows
                .get(&pid.0)
                .is_some_and(|p| p.map_state == MapState::Viewable),
            None => false,
        };
        if let Some(window) = self.windows.get_mut(&id.0) {
            let was_unmapped = window.map_state == MapState::Unmapped;
            window.map_state = if parent_viewable {
                MapState::Viewable
            } else {
                MapState::Unviewable
            };
            was_unmapped
        } else {
            false
        }
    }

    #[must_use]
    pub fn unmap_window(&mut self, id: ResourceId) -> bool {
        if id == ROOT_WINDOW {
            return false;
        }
        let Some(window) = self.windows.get_mut(&id.0) else {
            return false;
        };
        let was_mapped = window.map_state != MapState::Unmapped;
        window.map_state = MapState::Unmapped;
        was_mapped
    }

    pub fn window(&self, id: ResourceId) -> Option<&Window> {
        self.windows.get(&id.0)
    }

    pub fn window_mut(&mut self, id: ResourceId) -> Option<&mut Window> {
        self.windows.get_mut(&id.0)
    }

    pub fn children(&self, parent: ResourceId) -> &[ResourceId] {
        self.windows
            .get(&parent.0)
            .map_or(&[], |window| window.children.as_slice())
    }

    /// Phase-2 naive CirculateWindow: rotate the back child to the front
    /// (`direction = 0`, RaiseLowest) or the front child to the back
    /// (`direction = 1`, LowerHighest). Returns the moved child if any.
    /// Real obscuring detection is a Phase 4+ compositor concern.
    pub fn circulate_window(&mut self, container: ResourceId, direction: u8) -> Option<ResourceId> {
        let kids = &mut self.windows.get_mut(&container.0)?.children;
        if kids.len() < 2 {
            return None;
        }
        match direction {
            0 => {
                let last = kids.pop().expect("len>=2");
                kids.insert(0, last);
                Some(last)
            }
            1 => {
                let first = kids.remove(0);
                kids.push(first);
                Some(first)
            }
            _ => None,
        }
    }

    pub fn mapped_children_bottom_to_top(&self, parent: ResourceId) -> Option<Vec<ResourceId>> {
        let parent = self.windows.get(&parent.0)?;
        Some(
            parent
                .children
                .iter()
                .copied()
                .filter(|child| {
                    self.windows
                        .get(&child.0)
                        .is_some_and(|w| w.map_state != MapState::Unmapped)
                })
                .collect(),
        )
    }

    #[must_use]
    pub fn is_descendant_of(&self, candidate: ResourceId, ancestor: ResourceId) -> bool {
        let mut current = candidate;
        let mut seen = 0usize;
        while current != ROOT_WINDOW && seen <= self.windows.len() {
            let Some(window) = self.windows.get(&current.0) else {
                return false;
            };
            if window.parent == ancestor {
                return true;
            }
            if window.parent == current {
                return false;
            }
            current = window.parent;
            seen += 1;
        }
        false
    }

    pub fn window_owner(&self, id: ResourceId) -> Option<ClientId> {
        self.windows.get(&id.0).map(|w| w.owner)
    }

    pub fn parent_of(&self, id: ResourceId) -> Option<ResourceId> {
        self.windows.get(&id.0).map(|w| w.parent)
    }

    /// Walk mapped descendants of `top_level` and return the intersection
    /// rectangles (in each descendant's local coordinates) that overlap an
    /// expose region given in top-level coordinates. Used to synthesize
    /// `Expose` events for sub-windows when only the top-level subwindow has a
    /// host counterpart — without this, dragging a window across another
    /// leaves child sub-windows (titlebars, content panes) unrepainted because
    /// the host server never knows they exist.
    #[must_use]
    pub fn descendants_in_exposed_area(
        &self,
        top_level: ResourceId,
        ex: i16,
        ey: i16,
        ew: u16,
        eh: u16,
    ) -> Vec<ExposedRect> {
        let mut out = Vec::new();
        let er = i32::from(ex) + i32::from(ew);
        let eb = i32::from(ey) + i32::from(eh);
        self.descendants_in_exposed_area_inner(
            top_level,
            0,
            0,
            i32::from(ex),
            i32::from(ey),
            er,
            eb,
            &mut out,
        );
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn descendants_in_exposed_area_inner(
        &self,
        parent_id: ResourceId,
        parent_x: i32,
        parent_y: i32,
        ex: i32,
        ey: i32,
        er: i32,
        eb: i32,
        out: &mut Vec<ExposedRect>,
    ) {
        let Some(parent) = self.windows.get(&parent_id.0) else {
            return;
        };
        for child_id in &parent.children {
            let Some(child) = self.windows.get(&child_id.0) else {
                continue;
            };
            if child.map_state == MapState::Unmapped {
                continue;
            }
            let cx = parent_x + i32::from(child.x);
            let cy = parent_y + i32::from(child.y);
            let cr = cx + i32::from(child.width);
            let cb = cy + i32::from(child.height);
            let ix = ex.max(cx);
            let iy = ey.max(cy);
            let ir = er.min(cr);
            let ib = eb.min(cb);
            if ir <= ix || ib <= iy {
                continue;
            }
            let local_x = i16::try_from(ix - cx).unwrap_or(0);
            let local_y = i16::try_from(iy - cy).unwrap_or(0);
            let local_w = u16::try_from(ir - ix).unwrap_or(0);
            let local_h = u16::try_from(ib - iy).unwrap_or(0);
            if local_w > 0 && local_h > 0 {
                out.push(ExposedRect {
                    window: *child_id,
                    x: local_x,
                    y: local_y,
                    width: local_w,
                    height: local_h,
                });
            }
            self.descendants_in_exposed_area_inner(*child_id, cx, cy, ex, ey, er, eb, out);
        }
    }

    pub fn pointer_target_at(
        &self,
        top_level: ResourceId,
        x: i16,
        y: i16,
    ) -> Option<(ResourceId, i16, i16)> {
        let top = self.windows.get(&top_level.0)?;
        if top.map_state == MapState::Unmapped {
            return None;
        }
        let mut best = (top_level, x, y);
        self.pointer_target_at_inner(top_level, x, y, &mut best);
        Some(best)
    }

    fn pointer_target_at_inner(
        &self,
        parent: ResourceId,
        parent_x: i16,
        parent_y: i16,
        best: &mut (ResourceId, i16, i16),
    ) {
        let Some(parent_window) = self.windows.get(&parent.0) else {
            return;
        };
        for child_id in parent_window.children.iter().rev() {
            let Some(child) = self.windows.get(&child_id.0) else {
                continue;
            };
            if child.map_state == MapState::Unmapped {
                continue;
            }
            let child_x = parent_x.wrapping_sub(child.x);
            let child_y = parent_y.wrapping_sub(child.y);
            if child_x < 0
                || child_y < 0
                || child_x >= i16::try_from(child.width).unwrap_or(i16::MAX)
                || child_y >= i16::try_from(child.height).unwrap_or(i16::MAX)
            {
                continue;
            }
            *best = (*child_id, child_x, child_y);
            self.pointer_target_at_inner(*child_id, child_x, child_y, best);
            return;
        }
    }

    pub fn reparent_window(
        &mut self,
        request: ReparentWindowRequest,
    ) -> Result<ReparentResult, ReparentWindowError> {
        if request.window == ROOT_WINDOW
            || request.window == request.parent
            || self.is_descendant_of(request.parent, request.window)
        {
            return Err(ReparentWindowError::BadMatch);
        }
        let Some(window) = self.windows.get(&request.window.0) else {
            return Err(ReparentWindowError::BadWindow);
        };
        if !self.windows.contains_key(&request.parent.0) {
            return Err(ReparentWindowError::BadWindow);
        }

        let old_parent = window.parent;
        let override_redirect = window.override_redirect;
        let host_xid = window.host_xid;

        if let Some(parent) = self.windows.get_mut(&old_parent.0) {
            parent.children.retain(|child| *child != request.window);
        }
        if let Some(parent) = self.windows.get_mut(&request.parent.0) {
            parent.children.push(request.window);
        }
        let window = self
            .windows
            .get_mut(&request.window.0)
            .expect("window validated above");
        window.parent = request.parent;
        window.x = request.x;
        window.y = request.y;
        // Phase 3.6 Step 4a forwards XReparentWindow to the host, so
        // the host subwindow stays alive and continues to be the
        // rendering target. (Pre-Step-4a code destroyed the host
        // subwindow when a top-level moved away from root and cleared
        // host_xid here; that's no longer correct.)

        Ok(ReparentResult {
            window: request.window,
            old_parent,
            new_parent: request.parent,
            x: request.x,
            y: request.y,
            override_redirect,
            host_xid,
        })
    }

    #[must_use]
    pub fn window_property(&self, w: ResourceId, atom: AtomId) -> Option<&PropertyValue> {
        self.windows.get(&w.0)?.properties.get(&atom)
    }

    pub fn set_window_property(&mut self, w: ResourceId, atom: AtomId, value: PropertyValue) {
        if let Some(window) = self.windows.get_mut(&w.0) {
            window.properties.insert(atom, value);
        }
    }

    pub fn delete_window_property(&mut self, w: ResourceId, atom: AtomId) -> Option<PropertyValue> {
        self.windows.get_mut(&w.0)?.properties.remove(&atom)
    }

    pub fn create_pixmap(&mut self, owner: ClientId, request: CreatePixmapRequest) {
        self.pixmaps.insert(
            request.pixmap.0,
            Pixmap {
                id: request.pixmap,
                drawable: request.drawable,
                width: request.width,
                height: request.height,
                depth: request.depth,
                owner,
                host_xid: None,
            },
        );
    }

    pub fn free_pixmap(&mut self, id: ResourceId) -> Option<Pixmap> {
        self.pixmaps.remove(&id.0)
    }

    pub fn pixmap(&self, id: ResourceId) -> Option<&Pixmap> {
        self.pixmaps.get(&id.0)
    }

    #[must_use]
    pub fn set_pixmap_host_xid(
        &mut self,
        id: ResourceId,
        host_handle: crate::backend::PixmapHandle,
    ) -> bool {
        if let Some(pixmap) = self.pixmaps.get_mut(&id.0) {
            pixmap.host_xid = Some(host_handle);
            true
        } else {
            false
        }
    }

    pub fn window_background_pixmap_host_xid(&self, window_id: ResourceId) -> Option<u32> {
        // Use the snapshotted host XID rather than re-resolving the pixmap, so
        // it remains valid after the client frees the original pixmap (X11
        // semantics: the server retains the bg pixmap independent of refs).
        self.windows
            .get(&window_id.0)?
            .background_pixmap_host_xid
            .map(|h| h.as_raw())
    }

    /// Returns true if any window currently uses `host_xid` as its background.
    /// Used by FreePixmap to skip releasing host pixmaps still owned by a window.
    pub fn host_xid_referenced_by_window_bg(&self, host_xid: crate::backend::PixmapHandle) -> bool {
        self.windows
            .values()
            .any(|w| w.background_pixmap_host_xid == Some(host_xid))
    }

    #[must_use]
    pub fn host_drawable_target(&self, id: ResourceId) -> Option<HostDrawableTarget> {
        // Phase 3.6 Step 6: every InputOutput window has its own host_xid
        // (Step 2 invariant) and the host tree mirrors the local tree
        // (Step 4 reparent + configure forwarding), so drawing on a
        // sub-window targets that sub-window's host xid directly with no
        // coordinate translation. Windows without their own host_xid
        // (InputOnly, transient pre-init state) yield None — the caller
        // drops the draw silently, same as before (drawing on InputOnly
        // is undefined / spec-error territory).
        if let Some(window) = self.windows.get(&id.0) {
            let host_xid = window.host_xid?;
            return Some(HostDrawableTarget::Window {
                nested: id,
                host_xid,
                depth: window.depth,
            });
        }

        let pixmap = self.pixmaps.get(&id.0)?;
        Some(HostDrawableTarget::Pixmap {
            nested: id,
            host_xid: pixmap.host_xid?,
            width: pixmap.width,
            height: pixmap.height,
            depth: pixmap.depth,
        })
    }

    pub fn create_gc(&mut self, owner: ClientId, request: CreateGcRequest) {
        let clip_pixmap = match request.clip_mask {
            Some(Some(pixmap)) => Some(pixmap),
            _ => None,
        };
        let mut gc = Gc::with_defaults(request.gc, request.drawable, owner);
        gc.clip_pixmap = clip_pixmap;
        Self::apply_gc_change(
            &mut gc,
            GcChangeView {
                function: request.function,
                plane_mask: request.plane_mask,
                foreground: request.foreground,
                background: request.background,
                line_width: request.line_width,
                line_style: request.line_style,
                cap_style: request.cap_style,
                join_style: request.join_style,
                fill_style: request.fill_style,
                fill_rule: request.fill_rule,
                tile: request.tile,
                stipple: request.stipple,
                tile_x_origin: request.tile_x_origin,
                tile_y_origin: request.tile_y_origin,
                font: request.font,
                subwindow_mode: request.subwindow_mode,
                graphics_exposures: request.graphics_exposures,
                clip_x_origin: request.clip_x_origin,
                clip_y_origin: request.clip_y_origin,
                // CreateGC's clip-mask is consumed by the explicit
                // `clip_pixmap` assignment above; passing it again here
                // would re-clear `clip_rectangles` (irrelevant on a
                // fresh GC) but otherwise harmless. Pass `None` so the
                // helper is purely additive.
                clip_mask: None,
                dash_offset: request.dash_offset,
                dashes: request.dashes,
                arc_mode: request.arc_mode,
            },
        );
        self.gcs.insert(request.gc.0, gc);
    }

    pub fn change_gc(&mut self, request: GcChange) {
        let gc = self
            .gcs
            .entry(request.gc.0)
            .or_insert_with(|| Gc::with_defaults(request.gc, ResourceId(0), SERVER_OWNER));
        Self::apply_gc_change(
            gc,
            GcChangeView {
                function: request.function,
                plane_mask: request.plane_mask,
                foreground: request.foreground,
                background: request.background,
                line_width: request.line_width,
                line_style: request.line_style,
                cap_style: request.cap_style,
                join_style: request.join_style,
                fill_style: request.fill_style,
                fill_rule: request.fill_rule,
                tile: request.tile,
                stipple: request.stipple,
                tile_x_origin: request.tile_x_origin,
                tile_y_origin: request.tile_y_origin,
                font: request.font,
                subwindow_mode: request.subwindow_mode,
                graphics_exposures: request.graphics_exposures,
                clip_x_origin: request.clip_x_origin,
                clip_y_origin: request.clip_y_origin,
                clip_mask: request.clip_mask,
                dash_offset: request.dash_offset,
                dashes: request.dashes,
                arc_mode: request.arc_mode,
            },
        );
    }

    /// Apply the `Some`-valued attributes of a CreateGC / ChangeGC
    /// request onto an existing `Gc`. Shared between the two request
    /// paths so all 23 attribute slots are handled the same way.
    fn apply_gc_change(gc: &mut Gc, change: GcChangeView) {
        if let Some(function) = change.function {
            gc.function = GcFunction::from_protocol(function);
        }
        if let Some(plane_mask) = change.plane_mask {
            gc.plane_mask = plane_mask;
        }
        if let Some(foreground) = change.foreground {
            gc.foreground = foreground;
        }
        if let Some(background) = change.background {
            gc.background = background;
        }
        if let Some(line_width) = change.line_width {
            gc.line_width = line_width;
        }
        if let Some(line_style) = change.line_style {
            gc.line_style = LineStyle::from_protocol(line_style);
        }
        if let Some(cap_style) = change.cap_style {
            gc.cap_style = CapStyle::from_protocol(cap_style);
        }
        if let Some(join_style) = change.join_style {
            gc.join_style = JoinStyle::from_protocol(join_style);
        }
        if let Some(fs) = change.fill_style {
            gc.fill_style = FillStyle::from_protocol(fs);
        }
        if let Some(fill_rule) = change.fill_rule {
            gc.fill_rule = FillRule::from_protocol(fill_rule);
        }
        if let Some(tile) = change.tile {
            gc.tile = Some(tile);
        }
        if let Some(stipple) = change.stipple {
            gc.stipple = Some(stipple);
        }
        if let Some(x) = change.tile_x_origin {
            gc.tile_x_origin = x;
        }
        if let Some(y) = change.tile_y_origin {
            gc.tile_y_origin = y;
        }
        if let Some(font) = change.font {
            gc.font = Some(font);
        }
        if let Some(submode) = change.subwindow_mode {
            gc.subwindow_mode = SubwindowMode::from_protocol(submode);
        }
        if let Some(graphics_exposures) = change.graphics_exposures {
            gc.graphics_exposures = graphics_exposures;
        }
        if let Some(x) = change.clip_x_origin {
            gc.clip_x_origin = x;
        }
        if let Some(y) = change.clip_y_origin {
            gc.clip_y_origin = y;
        }
        // CPClipMask: Some(None) = clear, Some(Some(p)) = pixmap. Setting
        // a clip-mask supersedes any prior `SetClipRectangles` per spec.
        if let Some(mask) = change.clip_mask {
            gc.clip_rectangles = None;
            gc.clip_pixmap = mask;
        }
        if let Some(offset) = change.dash_offset {
            gc.dash_offset = offset as i16;
        }
        // CPDashList in CreateGC/ChangeGC is a single byte: store it as
        // the on/off pattern `[n, n]` and reset dash_offset per the X11
        // protocol semantics. The full SetDashes opcode (58) remains
        // unimplemented.
        if let Some(n) = change.dashes
            && n != 0
        {
            gc.dashes = vec![n, n];
            gc.dash_offset = 0;
        }
        if let Some(arc_mode) = change.arc_mode {
            gc.arc_mode = ArcMode::from_protocol(arc_mode);
        }
    }

    pub fn set_clip_rectangles(&mut self, request: SetClipRectanglesRequest) {
        let gc = self
            .gcs
            .entry(request.gc.0)
            .or_insert_with(|| Gc::with_defaults(request.gc, ResourceId(0), SERVER_OWNER));
        // SetClipRectangles supersedes any prior clip-mask pixmap.
        gc.clip_pixmap = None;
        gc.clip_rectangles = Some(request.clip);
    }

    pub fn copy_gc(&mut self, src: ResourceId, dst: ResourceId, value_mask: u32) {
        // Snapshot the source GC under the immutable borrow so we can
        // then take a mutable borrow of dst. Cheap because the only
        // owned field copied here is `dashes`.
        let Some(src_gc) = self.gcs.get(&src.0).cloned() else {
            return;
        };
        let Some(dst_gc) = self.gcs.get_mut(&dst.0) else {
            return;
        };
        if value_mask & 0x0000_0001 != 0 {
            dst_gc.function = src_gc.function;
        }
        if value_mask & 0x0000_0002 != 0 {
            dst_gc.plane_mask = src_gc.plane_mask;
        }
        if value_mask & 0x0000_0004 != 0 {
            dst_gc.foreground = src_gc.foreground;
        }
        if value_mask & 0x0000_0008 != 0 {
            dst_gc.background = src_gc.background;
        }
        if value_mask & 0x0000_0010 != 0 {
            dst_gc.line_width = src_gc.line_width;
        }
        if value_mask & 0x0000_0020 != 0 {
            dst_gc.line_style = src_gc.line_style;
        }
        if value_mask & 0x0000_0040 != 0 {
            dst_gc.cap_style = src_gc.cap_style;
        }
        if value_mask & 0x0000_0080 != 0 {
            dst_gc.join_style = src_gc.join_style;
        }
        if value_mask & 0x0000_0100 != 0 {
            dst_gc.fill_style = src_gc.fill_style;
        }
        if value_mask & 0x0000_0200 != 0 {
            dst_gc.fill_rule = src_gc.fill_rule;
        }
        if value_mask & 0x0000_0400 != 0 {
            dst_gc.tile = src_gc.tile;
        }
        if value_mask & 0x0000_0800 != 0 {
            dst_gc.stipple = src_gc.stipple;
        }
        if value_mask & 0x0000_1000 != 0 {
            dst_gc.tile_x_origin = src_gc.tile_x_origin;
        }
        if value_mask & 0x0000_2000 != 0 {
            dst_gc.tile_y_origin = src_gc.tile_y_origin;
        }
        if value_mask & 0x0000_4000 != 0 {
            dst_gc.font = src_gc.font;
        }
        if value_mask & 0x0000_8000 != 0 {
            dst_gc.subwindow_mode = src_gc.subwindow_mode;
        }
        if value_mask & 0x0001_0000 != 0 {
            dst_gc.graphics_exposures = src_gc.graphics_exposures;
        }
        if value_mask & 0x0002_0000 != 0 {
            dst_gc.clip_x_origin = src_gc.clip_x_origin;
        }
        if value_mask & 0x0004_0000 != 0 {
            dst_gc.clip_y_origin = src_gc.clip_y_origin;
        }
        if value_mask & 0x0008_0000 != 0 {
            // GCClipMask — copy both the rectangle-list and pixmap
            // clip-mask members. Either may be `None`; whichever the
            // source has set wins per X11 semantics (a clip-mask and a
            // rectangle-list cannot coexist).
            dst_gc.clip_rectangles = src_gc.clip_rectangles.clone();
            dst_gc.clip_pixmap = src_gc.clip_pixmap;
        }
        if value_mask & 0x0010_0000 != 0 {
            dst_gc.dash_offset = src_gc.dash_offset;
        }
        if value_mask & 0x0020_0000 != 0 {
            dst_gc.dashes = src_gc.dashes.clone();
        }
        if value_mask & 0x0040_0000 != 0 {
            dst_gc.arc_mode = src_gc.arc_mode;
        }
    }

    pub fn free_gc(&mut self, id: ResourceId) {
        self.gcs.remove(&id.0);
    }

    pub fn gc(&self, id: ResourceId) -> Option<&Gc> {
        self.gcs.get(&id.0)
    }

    pub fn gc_foreground(&self, id: ResourceId) -> u32 {
        self.gc(id).map_or(0, |gc| gc.foreground)
    }

    pub fn gc_background(&self, id: ResourceId) -> u32 {
        self.gc(id).map_or(0x00ff_ffff, |gc| gc.background)
    }

    pub fn gc_clip_rectangles(&self, id: ResourceId) -> Option<ClipRectangles> {
        self.gc(id).and_then(|gc| gc.clip_rectangles.clone())
    }

    /// Return the GC's effective clip-state, resolved against the host:
    /// either a list of rectangles, a host-pixmap clip-mask (with
    /// origin), or `None` (no clipping). Returns `None` for an unknown
    /// GC, or when the GC names a `clip_pixmap` whose host-side backing
    /// is missing — both equivalent to "draw unclipped" rather than
    /// erroring out the request.
    pub fn gc_clip_state(&self, id: ResourceId) -> GcClipState {
        let Some(gc) = self.gc(id) else {
            return GcClipState::None;
        };
        if let Some(rects) = gc.clip_rectangles.clone() {
            return GcClipState::Rectangles(rects);
        }
        if let Some(pixmap_id) = gc.clip_pixmap
            && let Some(pixmap) = self.pixmaps.get(&pixmap_id.0)
            && let Some(host_pixmap) = pixmap.host_xid
        {
            return GcClipState::Pixmap {
                host_pixmap,
                clip_x_origin: gc.clip_x_origin,
                clip_y_origin: gc.clip_y_origin,
            };
        }
        GcClipState::None
    }

    /// Resolve the GC's effective fill state (Solid / Tiled / Stippled /
    /// OpaqueStippled) plus the host pixmap xid for the tile/stipple if any.
    /// Returns `None` for the default Solid case (no setup needed) or when
    /// the GC is unknown. e16 paints popup backgrounds via Tiled fill — the
    /// PolyFillRectangle / PolyFillArc / FillPoly handlers must call
    /// `apply_gc_fill_state` before forwarding the draw and reset to Solid
    /// after, otherwise the host's shared GC silently fills with the GC's
    /// foreground (typically 0 = black).
    pub fn gc_fill_state(&self, id: ResourceId) -> GcFillState {
        let Some(gc) = self.gc(id) else {
            return GcFillState::Solid;
        };
        match gc.fill_style {
            FillStyle::Tiled => {
                let host_pixmap = gc
                    .tile
                    .and_then(|p| self.pixmaps.get(&p.0))
                    .and_then(|p| p.host_xid);
                match host_pixmap {
                    Some(host_pixmap) => GcFillState::Tiled {
                        host_pixmap,
                        tile_x_origin: gc.tile_x_origin,
                        tile_y_origin: gc.tile_y_origin,
                    },
                    // Tile pixmap missing or no host backing — fall back to
                    // Solid so the draw doesn't blow up; the host will fill
                    // with foreground colour, same as before this fix.
                    None => GcFillState::Solid,
                }
            }
            // Stippled / OpaqueStippled: not yet plumbed end-to-end on the
            // host shared GC; degrade to Solid so the draw doesn't blow up.
            // `resolve_draw_state` exposes the full FillState the host can
            // honour once the surface plumbing is wired up.
            _ => GcFillState::Solid,
        }
    }

    /// Resolve the GC's full `DrawState` snapshot for use by drawing
    /// call sites. Returns `None` only when the GC id is unknown — this
    /// is the BadGC case in the X11 protocol. Missing pixmap backing
    /// for a tile / stipple / clip-mask degrades the relevant component
    /// to its safe default (Solid fill, unclipped) rather than failing
    /// the whole request, mirroring `gc_fill_state` / `gc_clip_state`
    /// pre-Phase-6.2 behavior.
    pub fn resolve_draw_state(&self, gc_id: ResourceId) -> Option<DrawState> {
        let gc = self.gcs.get(&gc_id.0)?;

        // Clip resolution: rectangles take priority over pixmap, both
        // shifted by (clip_x_origin, clip_y_origin). Missing pixmap
        // backing degrades to "no clip".
        let clip = if let Some(rects) = gc.clip_rectangles.clone() {
            ClipState::Rectangles {
                origin: (gc.clip_x_origin, gc.clip_y_origin),
                rects,
            }
        } else if let Some(clip_pixmap_id) = gc.clip_pixmap {
            match self.pixmaps.get(&clip_pixmap_id.0).and_then(|p| p.host_xid) {
                Some(pixmap) => ClipState::Pixmap {
                    origin: (gc.clip_x_origin, gc.clip_y_origin),
                    pixmap,
                },
                None => ClipState::None,
            }
        } else {
            ClipState::None
        };

        // Fill resolution: degrade to Solid if the named tile/stipple
        // pixmap is missing host backing. The host's shared GC then
        // fills with foreground (existing pre-Phase-6.2 fallback).
        let fill = match gc.fill_style {
            FillStyle::Solid => FillState::Solid,
            FillStyle::Tiled => gc
                .tile
                .and_then(|t| self.pixmaps.get(&t.0))
                .and_then(|p| p.host_xid)
                .map(|pixmap| FillState::Tiled {
                    pixmap,
                    origin: (gc.tile_x_origin, gc.tile_y_origin),
                })
                .unwrap_or(FillState::Solid),
            FillStyle::Stippled => gc
                .stipple
                .and_then(|s| self.pixmaps.get(&s.0))
                .and_then(|p| p.host_xid)
                .map(|pixmap| FillState::Stippled {
                    pixmap,
                    origin: (gc.tile_x_origin, gc.tile_y_origin),
                })
                .unwrap_or(FillState::Solid),
            FillStyle::OpaqueStippled => gc
                .stipple
                .and_then(|s| self.pixmaps.get(&s.0))
                .and_then(|p| p.host_xid)
                .map(|pixmap| FillState::OpaqueStippled {
                    pixmap,
                    origin: (gc.tile_x_origin, gc.tile_y_origin),
                })
                .unwrap_or(FillState::Solid),
        };

        let font: Option<FontHandle> = gc
            .font
            .and_then(|f| self.fonts.get(&f.0))
            .map(|f| f.host_xid);

        Some(DrawState {
            foreground: gc.foreground,
            background: gc.background,
            line_width: gc.line_width,
            line_style: gc.line_style,
            cap_style: gc.cap_style,
            join_style: gc.join_style,
            fill_style: gc.fill_style,
            fill_rule: gc.fill_rule,
            function: gc.function,
            plane_mask: gc.plane_mask,
            font,
            clip,
            fill,
            subwindow_mode: gc.subwindow_mode,
            graphics_exposures: gc.graphics_exposures,
            dashes: gc.dashes.clone(),
            dash_offset: gc.dash_offset,
            arc_mode: gc.arc_mode,
        })
    }

    pub fn create_picture(&mut self, id: ResourceId, state: PictureState) {
        self.pictures.insert(id.0, state);
    }

    pub fn free_picture(&mut self, id: ResourceId) -> Option<PictureState> {
        self.pictures.remove(&id.0)
    }

    pub fn picture(&self, id: ResourceId) -> Option<&PictureState> {
        self.pictures.get(&id.0)
    }

    pub fn create_glyphset(&mut self, id: ResourceId, state: GlyphSetState) {
        if let Some(old) = self.glyphsets.remove(&id.0) {
            let _ = self.release_host_glyphset_ref(old.host_glyphset_xid.as_raw());
        }
        *self
            .host_glyphset_refcounts
            .entry(state.host_glyphset_xid.as_raw())
            .or_insert(0) += 1;
        self.glyphsets.insert(id.0, state);
    }

    pub fn free_glyphset(&mut self, id: ResourceId) -> Option<GlyphSetState> {
        let state = self.glyphsets.remove(&id.0)?;
        if self.release_host_glyphset_ref(state.host_glyphset_xid.as_raw()) {
            Some(state)
        } else {
            None
        }
    }

    pub fn glyphset(&self, id: ResourceId) -> Option<&GlyphSetState> {
        self.glyphsets.get(&id.0)
    }

    pub fn reference_glyphset(
        &mut self,
        client: ClientId,
        new_id: ResourceId,
        existing_id: ResourceId,
    ) -> bool {
        let Some(existing) = self.glyphsets.get(&existing_id.0) else {
            return false;
        };
        self.create_glyphset(
            new_id,
            GlyphSetState {
                client,
                host_glyphset_xid: existing.host_glyphset_xid,
            },
        );
        true
    }

    fn release_host_glyphset_ref(&mut self, host_xid: u32) -> bool {
        let Some(count) = self.host_glyphset_refcounts.get_mut(&host_xid) else {
            return true;
        };
        if *count > 1 {
            *count -= 1;
            false
        } else {
            self.host_glyphset_refcounts.remove(&host_xid);
            true
        }
    }

    pub fn install_font(
        &mut self,
        owner: ClientId,
        id: ResourceId,
        name: String,
        host_xid: crate::backend::FontHandle,
        metrics: FontMetrics,
    ) {
        self.fonts.insert(
            id.0,
            Font {
                id,
                name,
                host_xid,
                metrics,
                owner,
            },
        );
    }

    pub fn close_font(&mut self, id: ResourceId) -> Option<Font> {
        self.fonts.remove(&id.0)
    }

    pub fn font(&self, id: ResourceId) -> Option<&Font> {
        self.fonts.get(&id.0)
    }

    /// Resolve a FONTABLE id (either a Font or a GC carrying a font) to a `&Font`.
    pub fn fontable(&self, id: ResourceId) -> Option<&Font> {
        if let Some(font) = self.fonts.get(&id.0) {
            return Some(font);
        }
        let gc_font = self.gcs.get(&id.0).and_then(|gc| gc.font)?;
        self.fonts.get(&gc_font.0)
    }

    pub fn create_glyph_cursor(&mut self, owner: ClientId, id: ResourceId) {
        self.cursors.insert(
            id.0,
            Cursor {
                id,
                owner,
                host_xid: None,
            },
        );
    }

    pub fn create_cursor(&mut self, owner: ClientId, id: ResourceId) {
        self.cursors.insert(
            id.0,
            Cursor {
                id,
                owner,
                host_xid: None,
            },
        );
    }

    pub fn set_cursor_host_xid(&mut self, id: ResourceId, handle: crate::backend::CursorHandle) {
        if let Some(c) = self.cursors.get_mut(&id.0) {
            c.host_xid = Some(handle);
        }
    }

    pub fn cursor_host_xid(&self, id: ResourceId) -> Option<u32> {
        self.cursors.get(&id.0)?.host_xid.map(|h| h.as_raw())
    }

    /// Remove a cursor from the table and return the host XID (if any)
    /// so the caller can free it on the host. Caller's responsibility to
    /// dispatch `backend.free_cursor` — keeping the resource layer
    /// backend-agnostic.
    pub fn free_cursor(&mut self, id: ResourceId) -> Option<u32> {
        let host_xid = self.cursors.get(&id.0).and_then(|c| c.host_xid);
        self.cursors.remove(&id.0);
        host_xid.map(|h| h.as_raw())
    }

    /// Top-level windows owned by `client`: windows whose parent is *not*
    /// owned by the same client. Reachable descendants (regardless of
    /// owner) get destroyed transitively when each root is destroyed.
    pub fn collect_owned_window_roots(&self, client: ClientId, out: &mut Vec<ResourceId>) {
        for (raw_id, w) in &self.windows {
            if w.owner != client {
                continue;
            }
            let parent_owner = self.windows.get(&w.parent.0).map(|p| p.owner);
            if parent_owner != Some(client) {
                out.push(ResourceId(*raw_id));
            }
        }
    }

    /// Remove every non-window resource owned by `client`. Returns the
    /// `host_xid` of every removed font and every removed host-backed pixmap so
    /// the caller can issue host-side `CloseFont` / `FreePixmap` after dropping
    /// the `ServerState` lock.
    pub fn remove_non_window_resources_owned_by(
        &mut self,
        client: ClientId,
    ) -> ClientRemovedResources {
        let mut freed_pixmaps = Vec::new();
        self.pixmaps.retain(|_, p| {
            if p.owner == client {
                if let Some(xid) = p.host_xid {
                    freed_pixmaps.push(xid.as_raw());
                }
                false
            } else {
                true
            }
        });
        self.gcs.retain(|_, g| g.owner != client);
        let mut freed_cursors = Vec::new();
        self.cursors.retain(|_, c| {
            if c.owner == client {
                if let Some(xid) = c.host_xid {
                    freed_cursors.push(xid.as_raw());
                }
                false
            } else {
                true
            }
        });
        let mut closed_fonts = Vec::new();
        self.fonts.retain(|_, f| {
            if f.owner == client {
                closed_fonts.push(f.host_xid.as_raw());
                false
            } else {
                true
            }
        });
        let mut freed_pictures: Vec<(u32, Option<u32>)> = Vec::new();
        self.pictures.retain(|_, p| {
            if p.client == client {
                freed_pictures.push((
                    p.host_picture_xid.as_raw(),
                    p.host_owned_pixmap.map(|h| h.as_raw()),
                ));
                false
            } else {
                true
            }
        });
        let mut freed_glyphsets = Vec::new();
        let removed_glyphsets = self
            .glyphsets
            .extract_if(|_, g| g.client == client)
            .map(|(_, g)| g.host_glyphset_xid.as_raw())
            .collect::<Vec<_>>();
        for host_xid in removed_glyphsets {
            if self.release_host_glyphset_ref(host_xid) {
                freed_glyphsets.push(host_xid);
            }
        }
        ClientRemovedResources {
            closed_fonts,
            freed_pixmaps,
            freed_pictures,
            freed_glyphsets,
            freed_cursors,
        }
    }

    #[must_use]
    pub fn any_resource_exists(&self, id: ResourceId) -> bool {
        self.windows.contains_key(&id.0)
            || self.pixmaps.contains_key(&id.0)
            || self.gcs.contains_key(&id.0)
            || self.fonts.contains_key(&id.0)
            || self.cursors.contains_key(&id.0)
    }

    /// Returns the screen-absolute (x, y) of the top-left corner of `id`.
    /// Walks up the parent chain accumulating x/y offsets. Returns (0, 0) for ROOT_WINDOW.
    #[must_use]
    pub fn window_absolute_position(&self, id: ResourceId) -> (i32, i32) {
        if id == ROOT_WINDOW {
            return (0, 0);
        }
        let mut ax: i32 = 0;
        let mut ay: i32 = 0;
        let mut current = id;
        let mut depth = 0usize;
        while current != ROOT_WINDOW && depth < 256 {
            let Some(w) = self.windows.get(&current.0) else {
                break;
            };
            ax += i32::from(w.x);
            ay += i32::from(w.y);
            if w.parent == current {
                break;
            }
            current = w.parent;
            depth += 1;
        }
        (ax, ay)
    }

    /// Returns the child of `parent` (in the nested window tree) that contains
    /// the screen-absolute point (abs_x, abs_y), or None if no mapped child contains it.
    /// Children are checked in reverse order (top of stacking = last in list).
    #[must_use]
    pub fn child_containing_point(
        &self,
        parent: ResourceId,
        abs_x: i32,
        abs_y: i32,
    ) -> Option<ResourceId> {
        let (px, py) = self.window_absolute_position(parent);
        let local_x = abs_x - px;
        let local_y = abs_y - py;
        let w = self.windows.get(&parent.0)?;
        for &child_id in w.children.iter().rev() {
            let Some(child) = self.windows.get(&child_id.0) else {
                continue;
            };
            if child.map_state == MapState::Unmapped {
                continue;
            }
            let cx = i32::from(child.x);
            let cy = i32::from(child.y);
            let cw = i32::from(child.width);
            let ch = i32::from(child.height);
            if local_x >= cx && local_x < cx + cw && local_y >= cy && local_y < cy + ch {
                return Some(child_id);
            }
        }
        None
    }
}

/// Effective clip-state of a GC at the moment a draw op runs. Either
/// the GC has a `SetClipRectangles` list, a `ChangeGC(clip_mask=Pixmap)`
/// host-mask, or no clip at all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GcClipState {
    None,
    Rectangles(ClipRectangles),
    Pixmap {
        host_pixmap: crate::backend::PixmapHandle,
        clip_x_origin: i16,
        clip_y_origin: i16,
    },
}

/// Effective fill-style of a GC. Solid = use foreground; Tiled = tile a
/// pixmap onto the destination. Stippled / OpaqueStippled would belong
/// here too but no observed client uses them in the popup/menu paths
/// currently exercised — they fall through to Solid for now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcFillState {
    Solid,
    Tiled {
        host_pixmap: crate::backend::PixmapHandle,
        tile_x_origin: i16,
        tile_y_origin: i16,
    },
}

/// One client-issued `Composite::NameWindowPixmap` alias on a window. The
/// COMPOSITE spec allows a client to call `NameWindowPixmap` repeatedly;
/// each call returns a distinct `Pixmap` resource pointing at the
/// window's redirected backing store. All aliases on a window are
/// invalidated together by a resize and freed on destroy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NamedCompositePixmap {
    pub client_pixmap: ResourceId,
    pub host_pixmap: crate::backend::PixmapHandle,
    pub width: u16,
    pub height: u16,
}

/// The concrete restack a `ConfigureWindow` request resolves to after
/// applying X11's stack-mode + occlusion semantics. Built by
/// [`ResourceTable::resolve_restack_action`] from the immutable child
/// list, then applied by [`ResourceTable::restack_window`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RestackAction {
    NoOp,
    Top,
    Bottom,
    AboveSibling(ResourceId),
    BelowSibling(ResourceId),
}

/// True iff the bounding rectangles of `a` and `b` (including border)
/// have a non-empty intersection. Used by occlusion checks for
/// `TopIf` / `BottomIf` / `Opposite` stack modes.
fn window_rects_overlap(a: &Window, b: &Window) -> bool {
    let (ax0, ay0, ax1, ay1) = window_bounding_box(a);
    let (bx0, by0, bx1, by1) = window_bounding_box(b);
    ax0 < bx1 && bx0 < ax1 && ay0 < by1 && by0 < ay1
}

/// X11 bounding box of a window: the outer rectangle including border.
/// Returns (left, top, right, bottom) in parent coords.
fn window_bounding_box(w: &Window) -> (i32, i32, i32, i32) {
    let bw = i32::from(w.border_width);
    let x0 = i32::from(w.x);
    let y0 = i32::from(w.y);
    let x1 = x0 + i32::from(w.width) + 2 * bw;
    let y1 = y0 + i32::from(w.height) + 2 * bw;
    (x0, y0, x1, y1)
}

#[derive(Clone, Debug)]
pub struct Window {
    pub id: ResourceId,
    pub parent: ResourceId,
    pub children: Vec<ResourceId>,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border_width: u16,
    pub depth: u8,
    pub visual: ResourceId,
    pub class: WindowClass,
    pub map_state: MapState,
    pub background_pixel: u32,
    pub background_pixmap: Option<ResourceId>,
    /// Host XID of the bg pixmap, snapshotted at attrs-change time so
    /// it survives FreePixmap (X11 servers retain bg pixmaps independent
    /// of client refs).
    pub background_pixmap_host_xid: Option<crate::backend::PixmapHandle>,
    /// Host XID of the border pixmap (if any). Parallel to
    /// `background_pixmap_host_xid`, retained for the same reason.
    pub border_pixmap_host_xid: Option<crate::backend::PixmapHandle>,
    pub override_redirect: bool,
    pub cursor: Option<ResourceId>,
    pub owner: ClientId,
    pub properties: HashMap<AtomId, PropertyValue>,
    pub host_xid: Option<crate::backend::WindowHandle>,
    /// Per-window list of `Composite::NameWindowPixmap` aliases. All are
    /// invalidated together on resize per the COMPOSITE spec.
    pub composite_named_pixmaps: Vec<NamedCompositePixmap>,
}

impl Window {
    fn placeholder(id: ResourceId) -> Self {
        Self {
            id,
            parent: ROOT_WINDOW,
            children: Vec::new(),
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            border_width: 0,
            depth: 24,
            visual: ROOT_VISUAL,
            class: WindowClass::InputOutput,
            map_state: MapState::Unmapped,
            background_pixel: 0x00ff_ffff,
            background_pixmap: None,
            background_pixmap_host_xid: None,
            border_pixmap_host_xid: None,
            override_redirect: false,
            cursor: None,
            owner: SERVER_OWNER,
            properties: HashMap::new(),
            host_xid: None,
            composite_named_pixmaps: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowClass {
    CopyFromParent,
    InputOutput,
    InputOnly,
    Other(u16),
}

impl WindowClass {
    fn from_protocol(value: u16) -> Self {
        match value {
            0 => Self::CopyFromParent,
            1 => Self::InputOutput,
            2 => Self::InputOnly,
            value => Self::Other(value),
        }
    }

    pub fn protocol_value(self) -> u16 {
        match self {
            Self::CopyFromParent => 0,
            Self::InputOutput => 1,
            Self::InputOnly => 2,
            Self::Other(value) => value,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MapState {
    Unmapped,
    Unviewable,
    Viewable,
}

impl MapState {
    pub fn protocol_value(self) -> u8 {
        match self {
            Self::Unmapped => 0,
            Self::Unviewable => 1,
            Self::Viewable => 2,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Pixmap {
    pub id: ResourceId,
    pub drawable: ResourceId,
    pub width: u16,
    pub height: u16,
    pub depth: u8,
    pub owner: ClientId,
    pub host_xid: Option<crate::backend::PixmapHandle>,
}

#[derive(Clone, Debug)]
pub struct Gc {
    pub id: ResourceId,
    pub drawable: ResourceId,
    pub foreground: u32,
    pub background: u32,
    pub line_width: u16,
    pub font: Option<ResourceId>,
    pub clip_rectangles: Option<ClipRectangles>,
    /// Pixmap-based clip-mask, set via `ChangeGC` with `CPClipMask`. When
    /// `Some`, draws through the GC are clipped to the 1-bits of the
    /// referenced depth-1 pixmap shifted by `(clip_x_origin,
    /// clip_y_origin)`. wmaker uses this for window-decoration symbols
    /// (close-button "X", miniaturize dot).
    pub clip_pixmap: Option<ResourceId>,
    pub clip_x_origin: i16,
    pub clip_y_origin: i16,
    /// X11 GC `fill-style`. e16 paints popup backgrounds via Tiled fill,
    /// so PolyFillRectangle on the destination pixmap tiles the theme
    /// pixmap onto it. Without honoring this, the destination stays the
    /// default solid foreground (typically 0 = black).
    pub fill_style: FillStyle,
    pub tile: Option<ResourceId>,
    pub stipple: Option<ResourceId>,
    pub tile_x_origin: i16,
    pub tile_y_origin: i16,
    // Phase 6.2 additive scope: stored per-GC so they can be forwarded
    // to the host's shared GC at draw time. Pre-Phase-6.2 ynest silently
    // ignored these and drew with host-GC defaults; honoring them is a
    // behavioral improvement (e.g. drag-rectangle Xor now works).
    pub line_style: LineStyle,
    pub cap_style: CapStyle,
    pub join_style: JoinStyle,
    pub fill_rule: FillRule,
    pub function: GcFunction,
    pub plane_mask: u32,
    pub subwindow_mode: SubwindowMode,
    pub graphics_exposures: bool,
    pub dashes: Vec<u8>,
    pub dash_offset: i16,
    pub arc_mode: ArcMode,
    pub owner: ClientId,
}

/// Internal projection of CreateGC / ChangeGC's value-list onto a
/// single struct. Both request paths build one of these and feed it to
/// `apply_gc_change`, so all 23 attribute slots are handled identically.
#[derive(Clone, Copy, Debug, Default)]
struct GcChangeView {
    function: Option<u8>,
    plane_mask: Option<u32>,
    foreground: Option<u32>,
    background: Option<u32>,
    line_width: Option<u16>,
    line_style: Option<u8>,
    cap_style: Option<u8>,
    join_style: Option<u8>,
    fill_style: Option<u8>,
    fill_rule: Option<u8>,
    tile: Option<ResourceId>,
    stipple: Option<ResourceId>,
    tile_x_origin: Option<i16>,
    tile_y_origin: Option<i16>,
    font: Option<ResourceId>,
    subwindow_mode: Option<u8>,
    graphics_exposures: Option<bool>,
    clip_x_origin: Option<i16>,
    clip_y_origin: Option<i16>,
    clip_mask: Option<Option<ResourceId>>,
    dash_offset: Option<u16>,
    dashes: Option<u8>,
    arc_mode: Option<u8>,
}

impl Gc {
    /// Construct a GC with all-default attributes for the given id /
    /// drawable / owner. Used by the `change_gc` and
    /// `set_clip_rectangles` paths when the GC has not been seen before.
    fn with_defaults(id: ResourceId, drawable: ResourceId, owner: ClientId) -> Self {
        Self {
            id,
            drawable,
            foreground: 0,
            background: 0x00ff_ffff,
            line_width: 0,
            font: None,
            clip_rectangles: None,
            clip_pixmap: None,
            clip_x_origin: 0,
            clip_y_origin: 0,
            fill_style: FillStyle::Solid,
            tile: None,
            stipple: None,
            tile_x_origin: 0,
            tile_y_origin: 0,
            line_style: LineStyle::Solid,
            cap_style: CapStyle::Butt,
            join_style: JoinStyle::Miter,
            fill_rule: FillRule::EvenOdd,
            function: GcFunction::Copy,
            plane_mask: u32::MAX,
            subwindow_mode: SubwindowMode::ClipByChildren,
            graphics_exposures: true,
            dashes: vec![4, 4],
            dash_offset: 0,
            arc_mode: ArcMode::PieSlice,
            owner,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Font {
    pub id: ResourceId,
    pub name: String,
    pub host_xid: crate::backend::FontHandle,
    pub metrics: FontMetrics,
    pub owner: ClientId,
}

#[derive(Clone, Debug)]
pub struct Cursor {
    pub id: ResourceId,
    pub owner: ClientId,
    pub host_xid: Option<crate::backend::CursorHandle>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use yserver_protocol::x11::{ClientId, CreateWindowRequest};

    fn make_window(table: &mut ResourceTable, id: u32) {
        table.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(id),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                background_pixel: None,
                event_mask: None,
                override_redirect: None,
            },
        );
    }

    fn make_top_level_with_host_xid(table: &mut ResourceTable, id: u32, host_xid: u32) {
        make_window(table, id);
        table.windows.get_mut(&id).unwrap().host_xid =
            Some(crate::backend::WindowHandle::from_raw_for_test(host_xid));
    }

    fn make_child(table: &mut ResourceTable, id: u32, parent: u32, x: i16, y: i16) {
        table.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 24,
                window: ResourceId(id),
                parent: ResourceId(parent),
                x,
                y,
                width: 50,
                height: 50,
                border_width: 0,
                class: 1,
                visual: ROOT_VISUAL,
                background_pixel: None,
                event_mask: None,
                override_redirect: None,
            },
        );
    }

    #[derive(Debug, Clone, Copy)]
    enum InitialState {
        Viewable,
        Unviewable,
        Unmapped,
    }

    fn arb_initial() -> impl Strategy<Value = InitialState> {
        prop_oneof![
            Just(InitialState::Viewable),
            Just(InitialState::Unviewable),
            Just(InitialState::Unmapped),
        ]
    }

    #[test]
    fn reference_glyphset_alias_frees_host_only_after_last_alias() {
        let mut table = ResourceTable::new();
        table.create_glyphset(
            ResourceId(0x200),
            GlyphSetState {
                client: ClientId(1),
                host_glyphset_xid: crate::backend::GlyphSetHandle::from_raw_for_test(0xabc),
            },
        );

        assert!(table.reference_glyphset(ClientId(1), ResourceId(0x201), ResourceId(0x200)));
        assert_eq!(
            table
                .glyphset(ResourceId(0x201))
                .map(|g| g.host_glyphset_xid.as_raw()),
            Some(0xabc)
        );

        assert!(table.free_glyphset(ResourceId(0x200)).is_none());
        assert_eq!(
            table
                .free_glyphset(ResourceId(0x201))
                .map(|g| g.host_glyphset_xid.as_raw()),
            Some(0xabc)
        );
    }

    #[test]
    fn remove_client_frees_shared_glyphset_once() {
        let mut table = ResourceTable::new();
        table.create_glyphset(
            ResourceId(0x200),
            GlyphSetState {
                client: ClientId(1),
                host_glyphset_xid: crate::backend::GlyphSetHandle::from_raw_for_test(0xabc),
            },
        );
        assert!(table.reference_glyphset(ClientId(1), ResourceId(0x201), ResourceId(0x200)));

        let removed = table.remove_non_window_resources_owned_by(ClientId(1));

        assert_eq!(removed.freed_glyphsets, vec![0xabc]);
        assert!(table.glyphset(ResourceId(0x200)).is_none());
        assert!(table.glyphset(ResourceId(0x201)).is_none());
    }

    #[test]
    fn unmap_window_returns_true_on_transition_from_viewable() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x100002);
        let _ = table.map_window(ResourceId(0x100002));
        assert_eq!(
            table.window(ResourceId(0x100002)).unwrap().map_state,
            MapState::Viewable
        );
        let was_mapped = table.unmap_window(ResourceId(0x100002));
        assert!(was_mapped);
        assert_eq!(
            table.window(ResourceId(0x100002)).unwrap().map_state,
            MapState::Unmapped
        );
    }

    #[test]
    fn unmap_window_returns_true_on_transition_from_unviewable() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x100002);
        // Force Unviewable directly — no public setter, but the field is pub.
        table.windows.get_mut(&0x100002).unwrap().map_state = MapState::Unviewable;
        let was_mapped = table.unmap_window(ResourceId(0x100002));
        assert!(was_mapped);
        assert_eq!(
            table.window(ResourceId(0x100002)).unwrap().map_state,
            MapState::Unmapped
        );
    }

    #[test]
    fn unmap_window_returns_false_when_already_unmapped() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x100002);
        // create_window leaves new windows Unmapped.
        assert_eq!(
            table.window(ResourceId(0x100002)).unwrap().map_state,
            MapState::Unmapped
        );
        let first = table.unmap_window(ResourceId(0x100002));
        assert!(!first);
        let second = table.unmap_window(ResourceId(0x100002));
        assert!(!second);
    }

    #[test]
    fn unmap_window_returns_false_for_unknown_window() {
        let mut table = ResourceTable::new();
        let was_mapped = table.unmap_window(ResourceId(0x9999_9999));
        assert!(!was_mapped);
    }

    #[test]
    fn unmap_window_no_ops_on_root() {
        let mut table = ResourceTable::new();
        assert_eq!(
            table.window(ROOT_WINDOW).unwrap().map_state,
            MapState::Viewable
        );
        let was_mapped = table.unmap_window(ROOT_WINDOW);
        assert!(!was_mapped);
        assert_eq!(
            table.window(ROOT_WINDOW).unwrap().map_state,
            MapState::Viewable
        );
    }

    #[test]
    fn host_drawable_target_top_level_window_with_host_xid() {
        let mut table = ResourceTable::new();
        make_top_level_with_host_xid(&mut table, 0x0010_0002, 0xAA);
        let target = table.host_drawable_target(ResourceId(0x0010_0002));
        assert_eq!(
            target,
            Some(HostDrawableTarget::Window {
                nested: ResourceId(0x0010_0002),
                host_xid: crate::backend::WindowHandle::from_raw_for_test(0xAA),
                depth: 24,
            })
        );
    }

    #[test]
    fn host_drawable_target_child_window_targets_own_host_xid() {
        // Phase 3.6 Step 6: every InputOutput window has its own host_xid;
        // a child without host_xid set yields None (drop) rather than
        // walking up to the parent.
        let mut table = ResourceTable::new();
        make_top_level_with_host_xid(&mut table, 0x0010_0002, 0xBB);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
        if let Some(child) = table.window_mut(ResourceId(0x0010_0003)) {
            child.host_xid = Some(crate::backend::WindowHandle::from_raw_for_test(0xCC));
        }
        let target = table.host_drawable_target(ResourceId(0x0010_0003));
        assert_eq!(
            target,
            Some(HostDrawableTarget::Window {
                nested: ResourceId(0x0010_0003),
                host_xid: crate::backend::WindowHandle::from_raw_for_test(0xCC),
                depth: 24,
            })
        );
    }

    #[test]
    fn host_drawable_target_window_without_host_xid_returns_none() {
        let mut table = ResourceTable::new();
        make_top_level_with_host_xid(&mut table, 0x0010_0002, 0xBB);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
        // child has no host_xid; drawing on it drops silently.
        assert_eq!(table.host_drawable_target(ResourceId(0x0010_0003)), None);
    }

    #[test]
    fn host_drawable_target_pixmap_with_host_xid() {
        let mut table = ResourceTable::new();
        let request = CreatePixmapRequest {
            pixmap: ResourceId(0x0020_0002),
            drawable: ROOT_WINDOW,
            width: 256,
            height: 256,
            depth: 32,
        };
        table.create_pixmap(ClientId(1), request);
        assert!(table.set_pixmap_host_xid(
            ResourceId(0x0020_0002),
            crate::backend::PixmapHandle::from_raw_for_test(0xDEAD_BEEF),
        ));
        let target = table.host_drawable_target(ResourceId(0x0020_0002));
        assert_eq!(
            target,
            Some(HostDrawableTarget::Pixmap {
                nested: ResourceId(0x0020_0002),
                host_xid: crate::backend::PixmapHandle::from_raw_for_test(0xDEAD_BEEF),
                width: 256,
                height: 256,
                depth: 32,
            })
        );
    }

    #[test]
    fn host_drawable_target_pixmap_without_host_xid_returns_none() {
        let mut table = ResourceTable::new();
        let request = CreatePixmapRequest {
            pixmap: ResourceId(0x0020_0002),
            drawable: ROOT_WINDOW,
            width: 128,
            height: 128,
            depth: 24,
        };
        table.create_pixmap(ClientId(1), request);
        // host_xid is None by default
        let target = table.host_drawable_target(ResourceId(0x0020_0002));
        assert_eq!(target, None);
    }

    #[test]
    fn host_drawable_target_unknown_drawable_returns_none() {
        let table = ResourceTable::new();
        let target = table.host_drawable_target(ResourceId(0x9999_9999));
        assert_eq!(target, None);
    }

    #[test]
    fn set_pixmap_host_xid_unknown_id_returns_false() {
        let mut table = ResourceTable::new();
        let result = table.set_pixmap_host_xid(
            ResourceId(0xDEAD),
            crate::backend::PixmapHandle::from_raw_for_test(0x1234),
        );
        assert!(!result);
    }

    #[test]
    fn set_pixmap_host_xid_sets_value_and_returns_true() {
        let mut table = ResourceTable::new();
        let request = CreatePixmapRequest {
            pixmap: ResourceId(0x0020_0002),
            drawable: ROOT_WINDOW,
            width: 128,
            height: 128,
            depth: 24,
        };
        table.create_pixmap(ClientId(1), request);
        let result = table.set_pixmap_host_xid(
            ResourceId(0x0020_0002),
            crate::backend::PixmapHandle::from_raw_for_test(0x5678),
        );
        assert!(result);
        assert_eq!(
            table
                .pixmap(ResourceId(0x0020_0002))
                .unwrap()
                .host_xid
                .map(|h| h.as_raw()),
            Some(0x5678)
        );
    }

    #[test]
    fn host_drawable_target_window_depth_matches_window_depth() {
        let mut table = ResourceTable::new();
        make_top_level_with_host_xid(&mut table, 0x0010_0002, 0x1234);
        // Set depth to 32
        table.windows.get_mut(&0x0010_0002).unwrap().depth = 32;
        let target = table.host_drawable_target(ResourceId(0x0010_0002));
        if let Some(HostDrawableTarget::Window { depth, .. }) = target {
            assert_eq!(depth, 32);
        } else {
            panic!("Expected Window variant with depth 32");
        }
    }

    #[test]
    fn is_descendant_of_handles_child_grandchild_and_unrelated() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 0, 0);
        make_child(&mut table, 0x0010_0004, 0x0010_0003, 0, 0);
        make_window(&mut table, 0x0010_0005);

        assert!(table.is_descendant_of(ResourceId(0x0010_0003), ResourceId(0x0010_0002)));
        assert!(table.is_descendant_of(ResourceId(0x0010_0004), ResourceId(0x0010_0002)));
        assert!(!table.is_descendant_of(ResourceId(0x0010_0005), ResourceId(0x0010_0002)));
        assert!(!table.is_descendant_of(ResourceId(0xdead_beef), ResourceId(0x0010_0002)));
    }

    #[test]
    fn mapped_children_bottom_to_top_filters_unmapped_and_preserves_order() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 0, 0);
        make_child(&mut table, 0x0010_0004, 0x0010_0002, 0, 0);
        make_child(&mut table, 0x0010_0005, 0x0010_0002, 0, 0);
        let _ = table.map_window(ResourceId(0x0010_0003));
        let _ = table.map_window(ResourceId(0x0010_0005));

        assert_eq!(
            table.mapped_children_bottom_to_top(ResourceId(0x0010_0002)),
            Some(vec![ResourceId(0x0010_0003), ResourceId(0x0010_0005)])
        );
        assert_eq!(
            table.mapped_children_bottom_to_top(ResourceId(0xdead_beef)),
            None
        );
    }

    #[test]
    fn reparent_window_moves_child_and_updates_position() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002);
        make_window(&mut table, 0x0010_0003);
        make_child(&mut table, 0x0010_0004, 0x0010_0002, 1, 2);

        let result = table
            .reparent_window(ReparentWindowRequest {
                window: ResourceId(0x0010_0004),
                parent: ResourceId(0x0010_0003),
                x: 10,
                y: 20,
            })
            .unwrap();

        assert_eq!(result.old_parent, ResourceId(0x0010_0002));
        assert_eq!(result.new_parent, ResourceId(0x0010_0003));
        assert!(
            !table
                .children(ResourceId(0x0010_0002))
                .contains(&ResourceId(0x0010_0004))
        );
        assert_eq!(
            table.children(ResourceId(0x0010_0003)),
            &[ResourceId(0x0010_0004)]
        );
        let window = table.window(ResourceId(0x0010_0004)).unwrap();
        assert_eq!(window.parent, ResourceId(0x0010_0003));
        assert_eq!((window.x, window.y), (10, 20));
    }

    #[test]
    fn reparent_window_rejects_invalid_relationships() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 0, 0);

        assert_eq!(
            table.reparent_window(ReparentWindowRequest {
                window: ROOT_WINDOW,
                parent: ResourceId(0x0010_0002),
                x: 0,
                y: 0,
            }),
            Err(ReparentWindowError::BadMatch)
        );
        assert_eq!(
            table.reparent_window(ReparentWindowRequest {
                window: ResourceId(0x0010_0002),
                parent: ResourceId(0x0010_0002),
                x: 0,
                y: 0,
            }),
            Err(ReparentWindowError::BadMatch)
        );
        assert_eq!(
            table.reparent_window(ReparentWindowRequest {
                window: ResourceId(0x0010_0002),
                parent: ResourceId(0x0010_0003),
                x: 0,
                y: 0,
            }),
            Err(ReparentWindowError::BadMatch)
        );
    }

    #[test]
    fn reparent_window_rejects_unknown_windows() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002);

        assert_eq!(
            table.reparent_window(ReparentWindowRequest {
                window: ResourceId(0xdead_beef),
                parent: ResourceId(0x0010_0002),
                x: 0,
                y: 0,
            }),
            Err(ReparentWindowError::BadWindow)
        );
        assert_eq!(
            table.reparent_window(ReparentWindowRequest {
                window: ResourceId(0x0010_0002),
                parent: ResourceId(0xdead_beef),
                x: 0,
                y: 0,
            }),
            Err(ReparentWindowError::BadWindow)
        );
    }

    #[test]
    fn pointer_target_at_returns_deepest_mapped_child_and_relative_coords() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
        make_child(&mut table, 0x0010_0004, 0x0010_0003, 5, 6);
        let _ = table.map_window(ResourceId(0x0010_0002));
        let _ = table.map_window(ResourceId(0x0010_0003));
        let _ = table.map_window(ResourceId(0x0010_0004));

        assert_eq!(
            table.pointer_target_at(ResourceId(0x0010_0002), 20, 30),
            Some((ResourceId(0x0010_0004), 5, 4))
        );
    }

    #[test]
    fn pointer_target_at_falls_back_to_top_level_outside_children() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
        let _ = table.map_window(ResourceId(0x0010_0002));
        let _ = table.map_window(ResourceId(0x0010_0003));

        assert_eq!(
            table.pointer_target_at(ResourceId(0x0010_0002), 2, 3),
            Some((ResourceId(0x0010_0002), 2, 3))
        );
    }

    proptest! {
        #[test]
        fn unmap_window_state_machine(
            initial in arb_initial(),
            n in 1usize..=5,
        ) {
            let mut table = ResourceTable::new();
            make_window(&mut table, 0x100002);
            let target = ResourceId(0x100002);
            let initial_map_state = match initial {
                InitialState::Viewable => MapState::Viewable,
                InitialState::Unviewable => MapState::Unviewable,
                InitialState::Unmapped => MapState::Unmapped,
            };
            table.windows.get_mut(&target.0).unwrap().map_state = initial_map_state;

            let mut results = Vec::with_capacity(n);
            for _ in 0..n {
                results.push(table.unmap_window(target));
            }

            let expected_first = !matches!(initial, InitialState::Unmapped);
            prop_assert_eq!(results[0], expected_first);
            for r in results.iter().skip(1) {
                prop_assert!(!*r, "subsequent calls must return false");
            }
            prop_assert_eq!(
                table.window(target).unwrap().map_state,
                MapState::Unmapped
            );
        }

    }

    #[test]
    fn circulate_window_raises_lowest_to_top() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x400, ROOT_WINDOW.0, 0, 0);
        let moved = t.circulate_window(ROOT_WINDOW, 0).unwrap();
        assert_eq!(moved, ResourceId(0x400));
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x400), ResourceId(0x200), ResourceId(0x300)]
        );
    }

    #[test]
    fn circulate_window_lowers_highest_to_bottom() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        let moved = t.circulate_window(ROOT_WINDOW, 1).unwrap();
        assert_eq!(moved, ResourceId(0x200));
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x300), ResourceId(0x200)]
        );
    }

    #[test]
    fn circulate_window_noop_with_lt_two_children() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        assert!(t.circulate_window(ROOT_WINDOW, 0).is_none());
    }

    #[test]
    fn configure_window_stack_mode_above_raises_child() {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x400, ROOT_WINDOW.0, 0, 0);

        let configured = t.configure_window(ConfigureWindowRequest {
            window: ResourceId(0x200),
            value_mask: 1 << 6,
            x: None,
            y: None,
            width: None,
            height: None,
            border_width: None,
            sibling: None,
            stack_mode: Some(0),
        });

        assert!(configured.is_some());
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
        );
    }

    /// Build a request that only sets sibling + stack_mode. Geometry
    /// fields are `None` so existing position/size is preserved.
    fn restack_request(
        window: u32,
        sibling: Option<u32>,
        stack_mode: u8,
    ) -> ConfigureWindowRequest {
        ConfigureWindowRequest {
            window: ResourceId(window),
            value_mask: 0,
            x: None,
            y: None,
            width: None,
            height: None,
            border_width: None,
            sibling: sibling.map(ResourceId),
            stack_mode: Some(stack_mode),
        }
    }

    /// Children A=0x200, B=0x300, C=0x400 under root, all 50×50, mapped.
    /// `a_x` / `b_x` / `c_x` set the x position of each — y is always 0.
    fn three_mapped_children(a_x: i16, b_x: i16, c_x: i16) -> ResourceTable {
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, a_x, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, b_x, 0);
        make_child(&mut t, 0x400, ROOT_WINDOW.0, c_x, 0);
        let _ = t.map_window(ROOT_WINDOW);
        let _ = t.map_window(ResourceId(0x200));
        let _ = t.map_window(ResourceId(0x300));
        let _ = t.map_window(ResourceId(0x400));
        t
    }

    #[test]
    fn stack_mode_above_with_sibling_places_just_above() {
        // A B C ; place A above B → B A C
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x200, Some(0x300), 0))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x300), ResourceId(0x200), ResourceId(0x400)]
        );
    }

    #[test]
    fn stack_mode_below_with_sibling_places_just_below() {
        // A B C ; place C below B → A C B
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x400, Some(0x300), 1))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x400), ResourceId(0x300)]
        );
    }

    #[test]
    fn stack_mode_below_no_sibling_lowers_to_bottom() {
        // A B C ; lower C → C A B
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x400, None, 1))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x400), ResourceId(0x200), ResourceId(0x300)]
        );
    }

    #[test]
    fn top_if_with_overlapping_higher_sibling_raises_to_top() {
        // A B C all overlapping at (0,0); TopIf on A with sibling=C → C is
        // above A and overlaps → A goes to top.
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x200, Some(0x400), 2))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
        );
    }

    #[test]
    fn top_if_with_lower_sibling_is_noop() {
        // A B C ; TopIf on C with sibling=A → A is below C, cannot occlude
        // → no-op.
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x400, Some(0x200), 2))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
        );
    }

    #[test]
    fn top_if_with_higher_sibling_no_overlap_is_noop() {
        // A at x=0, B at x=200, C at x=400 — no geometric overlap.
        let mut t = three_mapped_children(0, 200, 400);
        assert!(
            t.configure_window(restack_request(0x200, Some(0x400), 2))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
        );
    }

    #[test]
    fn top_if_no_sibling_raises_when_any_higher_overlaps() {
        // A at x=0, B at x=200 (no overlap with A), C at x=20 (overlaps A
        // and is above A) → TopIf on A with no sibling → top.
        let mut t = three_mapped_children(0, 200, 20);
        assert!(
            t.configure_window(restack_request(0x200, None, 2))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
        );
    }

    #[test]
    fn bottom_if_with_lower_overlapping_sibling_lowers_to_bottom() {
        // A B C all overlapping; BottomIf on C with sibling=A → C is above
        // A and overlaps → C lowers to bottom.
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x400, Some(0x200), 3))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x400), ResourceId(0x200), ResourceId(0x300)]
        );
    }

    #[test]
    fn bottom_if_with_higher_sibling_is_noop() {
        // A B C ; BottomIf on A with sibling=C → A cannot occlude C
        // (A is below C) → no-op.
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x200, Some(0x400), 3))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
        );
    }

    #[test]
    fn bottom_if_no_sibling_no_overlap_is_noop() {
        let mut t = three_mapped_children(0, 200, 400);
        assert!(
            t.configure_window(restack_request(0x400, None, 3))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
        );
    }

    #[test]
    fn opposite_with_higher_overlapping_sibling_goes_to_top() {
        // A B C ; Opposite on A with sibling=C → C above + overlaps → top.
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x200, Some(0x400), 4))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
        );
    }

    #[test]
    fn opposite_with_lower_overlapping_sibling_goes_to_bottom() {
        // A B C ; Opposite on C with sibling=A → A is below + overlaps
        // (window-occludes-sibling holds) → bottom.
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x400, Some(0x200), 4))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x400), ResourceId(0x200), ResourceId(0x300)]
        );
    }

    #[test]
    fn opposite_no_overlap_is_noop() {
        let mut t = three_mapped_children(0, 200, 400);
        assert!(
            t.configure_window(restack_request(0x300, None, 4))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
        );
    }

    #[test]
    fn occlusion_ignores_unmapped_siblings() {
        // A B C all overlapping; unmap C; TopIf on A with no sibling.
        // C is unmapped so cannot occlude; B is above A and overlaps →
        // A still raises to top. Order after restack: [B, C, A].
        let mut t = three_mapped_children(0, 0, 0);
        let _ = t.unmap_window(ResourceId(0x400));
        assert!(
            t.configure_window(restack_request(0x200, None, 2))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x300), ResourceId(0x400), ResourceId(0x200)]
        );

        // Same shape but with B unmapped too: no mapped occluder remains
        // → no-op.
        let mut t = three_mapped_children(0, 0, 0);
        let _ = t.unmap_window(ResourceId(0x300));
        let _ = t.unmap_window(ResourceId(0x400));
        assert!(
            t.configure_window(restack_request(0x200, None, 2))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
        );
    }

    #[test]
    fn pointer_hit_test_follows_top_if_restack() {
        // A B at (0,0) overlapping; B is on top so pointer at (10,10) hits
        // B. After TopIf on A → A on top → pointer hits A.
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        let _ = t.map_window(ROOT_WINDOW);
        let _ = t.map_window(ResourceId(0x200));
        let _ = t.map_window(ResourceId(0x300));

        assert_eq!(
            t.pointer_target_at(ROOT_WINDOW, 10, 10).map(|h| h.0),
            Some(ResourceId(0x300))
        );

        assert!(
            t.configure_window(restack_request(0x200, None, 2))
                .is_some()
        );

        assert_eq!(
            t.pointer_target_at(ROOT_WINDOW, 10, 10).map(|h| h.0),
            Some(ResourceId(0x200))
        );
    }

    #[test]
    fn pointer_hit_test_follows_bottom_if_restack() {
        // A B at (0,0) overlapping; B is on top → pointer hits B.
        // BottomIf on B (no sibling) → B occludes A → bottom → pointer hits A.
        let mut t = ResourceTable::new();
        make_child(&mut t, 0x200, ROOT_WINDOW.0, 0, 0);
        make_child(&mut t, 0x300, ROOT_WINDOW.0, 0, 0);
        let _ = t.map_window(ROOT_WINDOW);
        let _ = t.map_window(ResourceId(0x200));
        let _ = t.map_window(ResourceId(0x300));

        assert!(
            t.configure_window(restack_request(0x300, None, 3))
                .is_some()
        );

        assert_eq!(
            t.pointer_target_at(ROOT_WINDOW, 10, 10).map(|h| h.0),
            Some(ResourceId(0x200))
        );
    }

    #[test]
    fn restack_with_sibling_not_in_parent_is_noop() {
        // 0x999 is not a child of root → request must be a no-op (in a
        // real server it would raise BadMatch; the resolver's contract is
        // to leave the local state untouched).
        let mut t = three_mapped_children(0, 0, 0);
        assert!(
            t.configure_window(restack_request(0x200, Some(0x999), 0))
                .is_some()
        );
        assert_eq!(
            t.children(ROOT_WINDOW),
            &[ResourceId(0x200), ResourceId(0x300), ResourceId(0x400)]
        );
    }

    #[test]
    fn visual_table_seeded_with_root_and_argb() {
        let t = ResourceTable::new();
        let root = t.visual(ROOT_VISUAL).expect("root visual seeded");
        assert_eq!(root.depth, 24);
        assert_eq!(root.alpha_mask, 0);
        assert_eq!(root.host_visual_xid, None);
        let argb = t.visual(ARGB_VISUAL).expect("argb visual seeded");
        assert_eq!(argb.depth, 32);
        assert_eq!(argb.alpha_mask, 0xff00_0000);
        assert_eq!(argb.host_visual_xid, None);
    }

    #[test]
    fn colormap_for_visual_returns_matching_entry() {
        let t = ResourceTable::new();
        assert_eq!(
            t.colormap_for_visual(ROOT_VISUAL).map(|c| c.id),
            Some(ROOT_COLORMAP)
        );
        assert_eq!(
            t.colormap_for_visual(ARGB_VISUAL).map(|c| c.id),
            Some(ARGB_COLORMAP)
        );
    }

    #[test]
    fn set_visual_host_xid_persists() {
        let mut t = ResourceTable::new();
        assert!(t.set_visual_host_xid(ARGB_VISUAL, 0x4711));
        assert_eq!(
            t.visual(ARGB_VISUAL)
                .and_then(|v| v.host_visual_xid)
                .map(|h| h.as_raw()),
            Some(0x4711)
        );
        assert!(!t.set_visual_host_xid(ResourceId(0xdead), 0x42));
    }

    #[test]
    fn set_colormap_host_xid_persists() {
        let mut t = ResourceTable::new();
        assert!(t.set_colormap_host_xid(ARGB_COLORMAP, 0x9999));
        assert_eq!(
            t.colormap(ARGB_COLORMAP)
                .and_then(|c| c.host_colormap_xid)
                .map(|h| h.as_raw()),
            Some(0x9999)
        );
    }

    #[test]
    fn is_known_visual_distinguishes_table_entries() {
        let t = ResourceTable::new();
        assert!(t.is_known_visual(ROOT_VISUAL));
        assert!(t.is_known_visual(ARGB_VISUAL));
        // 0 is the wire encoding for CopyFromParent — not in the table; the
        // CreateWindow handler validates separately and never queries this.
        assert!(!t.is_known_visual(ResourceId(0)));
        assert!(!t.is_known_visual(ResourceId(0xdead_beef)));
    }

    #[test]
    fn copy_from_parent_visual_inherits_argb_parent() {
        let mut t = ResourceTable::new();
        // Create an ARGB top-level then a CopyFromParent child of it.
        t.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 32,
                window: ResourceId(0x200),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 50,
                height: 50,
                border_width: 0,
                class: 1,
                visual: ARGB_VISUAL,
                background_pixel: None,
                event_mask: None,
                override_redirect: None,
            },
        );
        t.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 0,
                window: ResourceId(0x300),
                parent: ResourceId(0x200),
                x: 0,
                y: 0,
                width: 25,
                height: 25,
                border_width: 0,
                class: 1,
                visual: ResourceId(0), // CopyFromParent
                background_pixel: None,
                event_mask: None,
                override_redirect: None,
            },
        );
        let child = t.window(ResourceId(0x300)).expect("child created");
        assert_eq!(child.visual, ARGB_VISUAL);
        assert_eq!(child.depth, 32);
    }

    #[test]
    fn newly_created_window_has_no_border_pixmap_host_xid() {
        let mut t = ResourceTable::new();
        t.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 0,
                window: ResourceId(0x200),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 50,
                height: 50,
                border_width: 1,
                class: 1,
                visual: ResourceId(0),
                background_pixel: None,
                event_mask: None,
                override_redirect: None,
            },
        );
        assert_eq!(
            t.window(ResourceId(0x200)).unwrap().border_pixmap_host_xid,
            None
        );
    }

    #[test]
    fn copy_from_parent_visual_inherits_root_visual_for_root_child() {
        let mut t = ResourceTable::new();
        t.create_window(
            ClientId(1),
            CreateWindowRequest {
                depth: 0,
                window: ResourceId(0x200),
                parent: ROOT_WINDOW,
                x: 0,
                y: 0,
                width: 50,
                height: 50,
                border_width: 0,
                class: 1,
                visual: ResourceId(0),
                background_pixel: None,
                event_mask: None,
                override_redirect: None,
            },
        );
        let w = t.window(ResourceId(0x200)).expect("created");
        assert_eq!(w.visual, ROOT_VISUAL);
        assert_eq!(w.depth, 24);
    }

    fn empty_create_gc_request(gc: ResourceId, drawable: ResourceId) -> CreateGcRequest {
        CreateGcRequest {
            gc,
            drawable,
            function: None,
            plane_mask: None,
            foreground: None,
            background: None,
            line_width: None,
            line_style: None,
            cap_style: None,
            join_style: None,
            fill_style: None,
            fill_rule: None,
            tile: None,
            stipple: None,
            tile_x_origin: None,
            tile_y_origin: None,
            font: None,
            subwindow_mode: None,
            graphics_exposures: None,
            clip_x_origin: None,
            clip_y_origin: None,
            clip_mask: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        }
    }

    fn empty_change_gc(gc: ResourceId) -> GcChange {
        GcChange {
            gc,
            function: None,
            plane_mask: None,
            foreground: None,
            background: None,
            line_width: None,
            line_style: None,
            cap_style: None,
            join_style: None,
            fill_style: None,
            fill_rule: None,
            tile: None,
            stipple: None,
            tile_x_origin: None,
            tile_y_origin: None,
            font: None,
            subwindow_mode: None,
            graphics_exposures: None,
            clip_mask: None,
            clip_x_origin: None,
            clip_y_origin: None,
            dash_offset: None,
            dashes: None,
            arc_mode: None,
        }
    }

    fn install_pixmap_with_host_xid(table: &mut ResourceTable, id: u32, host_xid: u32) {
        table.create_pixmap(
            ClientId(1),
            CreatePixmapRequest {
                depth: 24,
                pixmap: ResourceId(id),
                drawable: ROOT_WINDOW,
                width: 16,
                height: 16,
            },
        );
        assert!(table.set_pixmap_host_xid(
            ResourceId(id),
            crate::backend::PixmapHandle::from_raw_for_test(host_xid),
        ));
    }

    #[test]
    fn change_gc_clip_mask_none_clears_clip_rectangles() {
        let mut t = ResourceTable::new();
        t.create_gc(
            ClientId(1),
            empty_create_gc_request(ResourceId(0x500), ROOT_WINDOW),
        );
        t.set_clip_rectangles(SetClipRectanglesRequest {
            gc: ResourceId(0x500),
            clip: ClipRectangles {
                ordering: 0,
                x_origin: 0,
                y_origin: 0,
                rectangles: vec![0, 0, 0, 0, 10, 0, 10, 0],
            },
        });
        assert!(t.gc_clip_rectangles(ResourceId(0x500)).is_some());

        let mut clear = empty_change_gc(ResourceId(0x500));
        clear.clip_mask = Some(None);
        t.change_gc(clear);

        assert!(t.gc_clip_rectangles(ResourceId(0x500)).is_none());
    }

    fn install_dummy_gc(table: &mut ResourceTable, id: u32) {
        table.create_gc(
            ClientId(1),
            empty_create_gc_request(ResourceId(id), ROOT_WINDOW),
        );
    }

    fn install_font_with_host_xid(table: &mut ResourceTable, id: u32, host_xid: u32) {
        table.install_font(
            ClientId(1),
            ResourceId(id),
            "fixed".to_string(),
            crate::backend::FontHandle::from_raw_for_test(host_xid),
            FontMetrics::default(),
        );
    }

    // ---- Phase 6.2 Step 3: copy_gc unit tests for the new fields. ----

    #[test]
    fn copy_gc_function_and_plane_mask() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_dummy_gc(&mut t, 0x501);
        // Mutate src.
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.function = Some(GcFunction::Xor.protocol_value());
        chg.plane_mask = Some(0x00ff_00ff);
        t.change_gc(chg);
        // Copy GCFunction (1<<0) + GCPlaneMask (1<<1).
        t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0000_0003);
        let dst = t.gc(ResourceId(0x501)).unwrap();
        assert_eq!(dst.function, GcFunction::Xor);
        assert_eq!(dst.plane_mask, 0x00ff_00ff);
    }

    #[test]
    fn copy_gc_line_join_cap_styles() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_dummy_gc(&mut t, 0x501);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.line_style = Some(LineStyle::OnOffDash.protocol_value());
        chg.cap_style = Some(CapStyle::Round.protocol_value());
        chg.join_style = Some(JoinStyle::Bevel.protocol_value());
        t.change_gc(chg);
        // line_style (1<<5) | cap_style (1<<6) | join_style (1<<7) = 0xE0
        t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0000_00E0);
        let dst = t.gc(ResourceId(0x501)).unwrap();
        assert_eq!(dst.line_style, LineStyle::OnOffDash);
        assert_eq!(dst.cap_style, CapStyle::Round);
        assert_eq!(dst.join_style, JoinStyle::Bevel);
    }

    #[test]
    fn copy_gc_fill_rule_and_subwindow_mode() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_dummy_gc(&mut t, 0x501);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.fill_rule = Some(FillRule::Winding.protocol_value());
        chg.subwindow_mode = Some(SubwindowMode::IncludeInferiors.protocol_value());
        t.change_gc(chg);
        // fill_rule (1<<9) | subwindow_mode (1<<15) = 0x8200
        t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0000_8200);
        let dst = t.gc(ResourceId(0x501)).unwrap();
        assert_eq!(dst.fill_rule, FillRule::Winding);
        assert_eq!(dst.subwindow_mode, SubwindowMode::IncludeInferiors);
    }

    #[test]
    fn copy_gc_graphics_exposures_and_dash_offset() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_dummy_gc(&mut t, 0x501);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.graphics_exposures = Some(false);
        chg.dash_offset = Some(7);
        t.change_gc(chg);
        // graphics_exposures (1<<16) | dash_offset (1<<20) = 0x0011_0000
        t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0011_0000);
        let dst = t.gc(ResourceId(0x501)).unwrap();
        assert!(!dst.graphics_exposures);
        assert_eq!(dst.dash_offset, 7);
    }

    #[test]
    fn copy_gc_dashes_and_arc_mode() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_dummy_gc(&mut t, 0x501);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.dashes = Some(9);
        chg.arc_mode = Some(ArcMode::Chord.protocol_value());
        t.change_gc(chg);
        // dashes (1<<21) | arc_mode (1<<22) = 0x00600000
        t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0x0060_0000);
        let dst = t.gc(ResourceId(0x501)).unwrap();
        assert_eq!(dst.dashes, vec![9, 9]);
        assert_eq!(dst.arc_mode, ArcMode::Chord);
    }

    #[test]
    fn copy_gc_zero_mask_copies_nothing() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_dummy_gc(&mut t, 0x501);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.function = Some(GcFunction::Xor.protocol_value());
        t.change_gc(chg);
        t.copy_gc(ResourceId(0x500), ResourceId(0x501), 0);
        let dst = t.gc(ResourceId(0x501)).unwrap();
        assert_eq!(dst.function, GcFunction::Copy);
    }

    // ---- Phase 6.2 Step 3: resolve_draw_state unit tests. ----

    #[test]
    fn resolve_draw_state_default_gc() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        let state = t.resolve_draw_state(ResourceId(0x500)).expect("known gc");
        // A fresh GC should match the DrawState::default() for the
        // attribute-only fields.
        let d = DrawState::default();
        assert_eq!(state.foreground, d.foreground);
        assert_eq!(state.background, d.background);
        assert_eq!(state.line_width, d.line_width);
        assert_eq!(state.line_style, d.line_style);
        assert_eq!(state.cap_style, d.cap_style);
        assert_eq!(state.join_style, d.join_style);
        assert_eq!(state.fill_style, d.fill_style);
        assert_eq!(state.fill_rule, d.fill_rule);
        assert_eq!(state.function, d.function);
        assert_eq!(state.plane_mask, d.plane_mask);
        assert_eq!(state.font, None);
        assert_eq!(state.clip, ClipState::None);
        assert_eq!(state.fill, FillState::Solid);
        assert_eq!(state.subwindow_mode, d.subwindow_mode);
        assert!(state.graphics_exposures);
        assert_eq!(state.dashes, d.dashes);
        assert_eq!(state.dash_offset, d.dash_offset);
        assert_eq!(state.arc_mode, d.arc_mode);
    }

    #[test]
    fn resolve_draw_state_tiled_fill_resolves_pixmap_handle() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_pixmap_with_host_xid(&mut t, 0x600, 0x12345);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.fill_style = Some(FillStyle::Tiled.protocol_value());
        chg.tile = Some(ResourceId(0x600));
        chg.tile_x_origin = Some(3);
        chg.tile_y_origin = Some(5);
        t.change_gc(chg);
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        match state.fill {
            FillState::Tiled { pixmap, origin } => {
                assert_eq!(pixmap.as_raw(), 0x12345);
                assert_eq!(origin, (3, 5));
            }
            other => panic!("expected Tiled, got {other:?}"),
        }
    }

    #[test]
    fn resolve_draw_state_stippled_fill_resolves_pixmap_handle() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_pixmap_with_host_xid(&mut t, 0x600, 0xabcde);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.fill_style = Some(FillStyle::Stippled.protocol_value());
        chg.stipple = Some(ResourceId(0x600));
        t.change_gc(chg);
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        match state.fill {
            FillState::Stippled { pixmap, origin } => {
                assert_eq!(pixmap.as_raw(), 0xabcde);
                assert_eq!(origin, (0, 0));
            }
            other => panic!("expected Stippled, got {other:?}"),
        }
    }

    #[test]
    fn resolve_draw_state_clip_rectangles_with_origin() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        let rect_bytes = vec![0u8, 0, 0, 0, 10, 0, 10, 0];
        t.set_clip_rectangles(SetClipRectanglesRequest {
            gc: ResourceId(0x500),
            clip: ClipRectangles {
                ordering: 0,
                x_origin: 4,
                y_origin: 7,
                rectangles: rect_bytes.clone(),
            },
        });
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.clip_x_origin = Some(11);
        chg.clip_y_origin = Some(13);
        t.change_gc(chg);
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        match state.clip {
            ClipState::Rectangles { origin, rects } => {
                // origin comes from the GC's clip-x/y-origin (set above),
                // not from the SetClipRectangles x/y_origin (which lives
                // inside the rectangles payload itself).
                assert_eq!(origin, (11, 13));
                assert_eq!(rects.x_origin, 4);
                assert_eq!(rects.y_origin, 7);
                assert_eq!(rects.rectangles, rect_bytes);
            }
            other => panic!("expected Rectangles, got {other:?}"),
        }
    }

    #[test]
    fn resolve_draw_state_pixmap_clip_with_origin() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_pixmap_with_host_xid(&mut t, 0x600, 0xdead);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.clip_mask = Some(Some(ResourceId(0x600)));
        chg.clip_x_origin = Some(2);
        chg.clip_y_origin = Some(3);
        t.change_gc(chg);
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        match state.clip {
            ClipState::Pixmap { origin, pixmap } => {
                assert_eq!(origin, (2, 3));
                assert_eq!(pixmap.as_raw(), 0xdead);
            }
            other => panic!("expected Pixmap, got {other:?}"),
        }
    }

    #[test]
    fn resolve_draw_state_unknown_gc_returns_none() {
        let t = ResourceTable::new();
        assert!(t.resolve_draw_state(ResourceId(0x999)).is_none());
    }

    #[test]
    fn resolve_draw_state_tiled_with_freed_tile_pixmap_degrades_to_solid() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_pixmap_with_host_xid(&mut t, 0x600, 0xbeef);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.fill_style = Some(FillStyle::Tiled.protocol_value());
        chg.tile = Some(ResourceId(0x600));
        t.change_gc(chg);
        // Now free the tile pixmap; the GC still names it but the host
        // backing is gone — the resolver must fall back to Solid.
        let _ = t.free_pixmap(ResourceId(0x600));
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        assert_eq!(state.fill, FillState::Solid);
    }

    #[test]
    fn resolve_draw_state_clip_pixmap_freed_degrades_to_unclipped() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_pixmap_with_host_xid(&mut t, 0x600, 0xcafe);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.clip_mask = Some(Some(ResourceId(0x600)));
        t.change_gc(chg);
        let _ = t.free_pixmap(ResourceId(0x600));
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        assert_eq!(state.clip, ClipState::None);
    }

    #[test]
    fn resolve_draw_state_font_resolves_handle() {
        let mut t = ResourceTable::new();
        install_dummy_gc(&mut t, 0x500);
        install_font_with_host_xid(&mut t, 0x700, 0x4242);
        let mut chg = empty_change_gc(ResourceId(0x500));
        chg.font = Some(ResourceId(0x700));
        t.change_gc(chg);
        let state = t.resolve_draw_state(ResourceId(0x500)).unwrap();
        let f = state.font.expect("font handle");
        assert_eq!(f.as_raw(), 0x4242);
    }
}
