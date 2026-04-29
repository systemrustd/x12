#![allow(dead_code)]

use std::collections::HashMap;

use yserver_protocol::x11::{
    AtomId, ChangeWindowAttributesRequest, ClientId, ClipRectangles, ConfigureWindowRequest,
    CreateGcRequest, CreatePixmapRequest, CreateWindowRequest, FontMetrics, GcChange,
    ReparentWindowRequest, ResourceId, SetClipRectanglesRequest,
};

use crate::properties::PropertyValue;

pub const SERVER_OWNER: ClientId = ClientId(0);

#[derive(Debug, Default)]
pub struct ClientRemovedResources {
    pub closed_fonts: Vec<u32>,
    pub freed_pixmaps: Vec<u32>,
    pub freed_pictures: Vec<(u32, Option<u32>)>,
    pub freed_glyphsets: Vec<u32>,
}

pub const ROOT_WINDOW: ResourceId = ResourceId(0x100);
pub const ROOT_COLORMAP: ResourceId = ResourceId(0x101);
pub const ROOT_VISUAL: ResourceId = ResourceId(0x102);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TopLevelTarget {
    pub top_level: ResourceId,
    pub host_xid: u32,
    pub x_offset: i16,
    pub y_offset: i16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostDrawableTarget {
    Window {
        nested: ResourceId,
        top_level: ResourceId,
        host_xid: u32,
        x_offset: i16,
        y_offset: i16,
        depth: u8,
    },
    Pixmap {
        nested: ResourceId,
        host_xid: u32,
        width: u16,
        height: u16,
        depth: u8,
    },
}

impl HostDrawableTarget {
    pub fn host_xid(self) -> u32 {
        match self {
            Self::Window { host_xid, .. } | Self::Pixmap { host_xid, .. } => host_xid,
        }
    }

    pub fn x_offset(self) -> i16 {
        match self {
            Self::Window { x_offset, .. } => x_offset,
            Self::Pixmap { .. } => 0,
        }
    }

    pub fn y_offset(self) -> i16 {
        match self {
            Self::Window { y_offset, .. } => y_offset,
            Self::Pixmap { .. } => 0,
        }
    }

    pub fn depth(self) -> u8 {
        match self {
            Self::Window { depth, .. } | Self::Pixmap { depth, .. } => depth,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReparentResult {
    pub window: ResourceId,
    pub old_parent: ResourceId,
    pub new_parent: ResourceId,
    pub x: i16,
    pub y: i16,
    pub override_redirect: bool,
    pub host_xid: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReparentWindowError {
    BadWindow,
    BadMatch,
}

#[derive(Debug)]
pub struct PictureState {
    pub client: ClientId,
    pub host_picture_xid: u32,
    pub host_owned_pixmap: Option<u32>,
    pub x_offset: i16,
    pub y_offset: i16,
}

#[derive(Debug)]
pub struct GlyphSetState {
    pub client: ClientId,
    pub host_glyphset_xid: u32,
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
                override_redirect: false,
                cursor: None,
                owner: SERVER_OWNER,
                properties: HashMap::new(),
                host_xid: None,
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
        }
    }

    pub fn create_window(&mut self, owner: ClientId, request: CreateWindowRequest) {
        let window = Window {
            id: request.window,
            parent: request.parent,
            children: Vec::new(),
            x: request.x,
            y: request.y,
            width: request.width,
            height: request.height,
            border_width: request.border_width,
            depth: if request.depth == 0 {
                self.windows.get(&request.parent.0).map_or(24, |p| p.depth)
            } else {
                request.depth
            },
            visual: if request.visual.0 == 0 {
                ROOT_VISUAL
            } else {
                request.visual
            },
            class: WindowClass::from_protocol(request.class),
            map_state: MapState::Unmapped,
            background_pixel: request.background_pixel.unwrap_or(0x00ff_ffff),
            background_pixmap: None,
            override_redirect: request.override_redirect.unwrap_or(false),
            cursor: None,
            owner,
            properties: HashMap::new(),
            host_xid: None,
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

    fn destroy_window_inner(&mut self, id: ResourceId, destroyed: &mut Vec<ResourceId>) {
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

    pub fn change_window_attributes(&mut self, request: ChangeWindowAttributesRequest) {
        if let Some(window) = self.windows.get_mut(&request.window.0) {
            if let Some(bg_pixmap) = request.background_pixmap {
                window.background_pixmap = if bg_pixmap.0 == 0 {
                    None
                } else {
                    Some(bg_pixmap)
                };
            }
            if let Some(background_pixel) = request.background_pixel {
                window.background_pixel = background_pixel;
            }
            if let Some(cursor) = request.cursor {
                window.cursor = Some(cursor);
            }
        }
    }

    pub fn configure_window(&mut self, request: ConfigureWindowRequest) -> Option<&Window> {
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
        Some(window)
    }

    #[must_use]
    pub fn map_window(&mut self, id: ResourceId) -> bool {
        if let Some(window) = self.windows.get_mut(&id.0) {
            let was_unmapped = window.map_state == MapState::Unmapped;
            window.map_state = MapState::Viewable;
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

    #[must_use]
    pub fn top_level_host_target(&self, id: ResourceId) -> Option<TopLevelTarget> {
        let mut current = self.windows.get(&id.0)?;
        if current.id == ROOT_WINDOW {
            return None;
        }
        let mut x_offset: i16 = 0;
        let mut y_offset: i16 = 0;
        while current.parent != ROOT_WINDOW {
            x_offset = x_offset.wrapping_add(current.x);
            y_offset = y_offset.wrapping_add(current.y);
            let next = self.windows.get(&current.parent.0)?;
            if next.id == ROOT_WINDOW {
                // Parent chain points at root through a missing or self-loop entry.
                return None;
            }
            current = next;
        }
        // current is now the top-level (parent == ROOT_WINDOW).
        let host_xid = current.host_xid?;
        Some(TopLevelTarget {
            top_level: current.id,
            host_xid,
            x_offset,
            y_offset,
        })
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
        // Moving a former top-level into the tree: its host subwindow is no longer
        // the rendering target (top_level_host_target will follow the new top-level).
        if old_parent == ROOT_WINDOW && request.parent != ROOT_WINDOW {
            window.host_xid = None;
        }

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
    pub fn set_pixmap_host_xid(&mut self, id: ResourceId, host_xid: u32) -> bool {
        if let Some(pixmap) = self.pixmaps.get_mut(&id.0) {
            pixmap.host_xid = Some(host_xid);
            true
        } else {
            false
        }
    }

    pub fn window_background_pixmap_host_xid(&self, window_id: ResourceId) -> Option<u32> {
        let bg_pixmap_id = self.windows.get(&window_id.0)?.background_pixmap?;
        self.pixmaps.get(&bg_pixmap_id.0)?.host_xid
    }

    #[must_use]
    pub fn host_drawable_target(&self, id: ResourceId) -> Option<HostDrawableTarget> {
        if let Some(window) = self.windows.get(&id.0) {
            let target = self.top_level_host_target(id)?;
            return Some(HostDrawableTarget::Window {
                nested: id,
                top_level: target.top_level,
                host_xid: target.host_xid,
                x_offset: target.x_offset,
                y_offset: target.y_offset,
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
        self.gcs.insert(
            request.gc.0,
            Gc {
                id: request.gc,
                drawable: request.drawable,
                foreground: request.foreground.unwrap_or(0),
                background: request.background.unwrap_or(0x00ff_ffff),
                line_width: request.line_width.unwrap_or(0),
                font: request.font,
                clip_rectangles: None,
                owner,
            },
        );
    }

    pub fn change_gc(&mut self, request: GcChange) {
        let gc = self.gcs.entry(request.gc.0).or_insert(Gc {
            id: request.gc,
            drawable: ResourceId(0),
            foreground: 0,
            background: 0x00ff_ffff,
            line_width: 0,
            font: None,
            clip_rectangles: None,
            owner: SERVER_OWNER,
        });
        if let Some(foreground) = request.foreground {
            gc.foreground = foreground;
        }
        if let Some(background) = request.background {
            gc.background = background;
        }
        if let Some(line_width) = request.line_width {
            gc.line_width = line_width;
        }
        if let Some(font) = request.font {
            gc.font = Some(font);
        }
    }

    pub fn set_clip_rectangles(&mut self, request: SetClipRectanglesRequest) {
        let gc = self.gcs.entry(request.gc.0).or_insert(Gc {
            id: request.gc,
            drawable: ResourceId(0),
            foreground: 0,
            background: 0x00ff_ffff,
            line_width: 0,
            font: None,
            clip_rectangles: None,
            owner: SERVER_OWNER,
        });
        gc.clip_rectangles = Some(request.clip);
    }

    pub fn copy_gc(&mut self, src: ResourceId, dst: ResourceId, value_mask: u32) {
        let src_data = self.gcs.get(&src.0).map(|g| {
            (
                g.foreground,
                g.background,
                g.line_width,
                g.font,
                g.clip_rectangles.clone(),
            )
        });
        let Some((fg, bg, lw, font, clip)) = src_data else {
            return;
        };
        let Some(dst_gc) = self.gcs.get_mut(&dst.0) else {
            return;
        };
        if value_mask & (1 << 2) != 0 {
            dst_gc.foreground = fg;
        }
        if value_mask & (1 << 3) != 0 {
            dst_gc.background = bg;
        }
        if value_mask & (1 << 4) != 0 {
            dst_gc.line_width = lw;
        }
        if value_mask & (1 << 14) != 0 {
            dst_gc.font = font;
        }
        if value_mask & (1 << 19) != 0 {
            // GCClipMask — copy internal clip-rectangle list
            dst_gc.clip_rectangles = clip;
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
        self.glyphsets.insert(id.0, state);
    }

    pub fn free_glyphset(&mut self, id: ResourceId) -> Option<GlyphSetState> {
        self.glyphsets.remove(&id.0)
    }

    pub fn glyphset(&self, id: ResourceId) -> Option<&GlyphSetState> {
        self.glyphsets.get(&id.0)
    }

    pub fn install_font(
        &mut self,
        owner: ClientId,
        id: ResourceId,
        name: String,
        host_xid: u32,
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

    pub fn set_cursor_host_xid(&mut self, id: ResourceId, xid: u32) {
        if let Some(c) = self.cursors.get_mut(&id.0) {
            c.host_xid = Some(xid);
        }
    }

    pub fn cursor_host_xid(&self, id: ResourceId) -> Option<u32> {
        self.cursors.get(&id.0)?.host_xid
    }

    pub fn free_cursor(&mut self, id: ResourceId) {
        self.cursors.remove(&id.0);
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
                    freed_pixmaps.push(xid);
                }
                false
            } else {
                true
            }
        });
        self.gcs.retain(|_, g| g.owner != client);
        self.cursors.retain(|_, c| c.owner != client);
        let mut closed_fonts = Vec::new();
        self.fonts.retain(|_, f| {
            if f.owner == client {
                closed_fonts.push(f.host_xid);
                false
            } else {
                true
            }
        });
        let mut freed_pictures: Vec<(u32, Option<u32>)> = Vec::new();
        self.pictures.retain(|_, p| {
            if p.client == client {
                freed_pictures.push((p.host_picture_xid, p.host_owned_pixmap));
                false
            } else {
                true
            }
        });
        let mut freed_glyphsets = Vec::new();
        self.glyphsets.retain(|_, g| {
            if g.client == client {
                freed_glyphsets.push(g.host_glyphset_xid);
                false
            } else {
                true
            }
        });
        ClientRemovedResources {
            closed_fonts,
            freed_pixmaps,
            freed_pictures,
            freed_glyphsets,
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
    pub override_redirect: bool,
    pub cursor: Option<ResourceId>,
    pub owner: ClientId,
    pub properties: HashMap<AtomId, PropertyValue>,
    pub host_xid: Option<u32>,
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
            override_redirect: false,
            cursor: None,
            owner: SERVER_OWNER,
            properties: HashMap::new(),
            host_xid: None,
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
    pub host_xid: Option<u32>,
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
    pub owner: ClientId,
}

#[derive(Clone, Debug)]
pub struct Font {
    pub id: ResourceId,
    pub name: String,
    pub host_xid: u32,
    pub metrics: FontMetrics,
    pub owner: ClientId,
}

#[derive(Clone, Debug)]
pub struct Cursor {
    pub id: ResourceId,
    pub owner: ClientId,
    pub host_xid: Option<u32>,
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
        table.windows.get_mut(&id).unwrap().host_xid = Some(host_xid);
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
    fn top_level_host_target_for_top_level_returns_self() {
        let mut table = ResourceTable::new();
        make_top_level_with_host_xid(&mut table, 0x0010_0002, 0xAA);
        let target = table.top_level_host_target(ResourceId(0x0010_0002));
        assert_eq!(
            target,
            Some(TopLevelTarget {
                top_level: ResourceId(0x0010_0002),
                host_xid: 0xAA,
                x_offset: 0,
                y_offset: 0,
            })
        );
    }

    #[test]
    fn top_level_host_target_for_child_accumulates_offset() {
        let mut table = ResourceTable::new();
        make_top_level_with_host_xid(&mut table, 0x0010_0002, 0xAA);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
        let target = table.top_level_host_target(ResourceId(0x0010_0003));
        assert_eq!(
            target,
            Some(TopLevelTarget {
                top_level: ResourceId(0x0010_0002),
                host_xid: 0xAA,
                x_offset: 10,
                y_offset: 20,
            })
        );
    }

    #[test]
    fn top_level_host_target_for_grandchild_sums_offsets() {
        let mut table = ResourceTable::new();
        make_top_level_with_host_xid(&mut table, 0x0010_0002, 0xAA);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
        make_child(&mut table, 0x0010_0004, 0x0010_0003, 5, 5);
        let target = table.top_level_host_target(ResourceId(0x0010_0004));
        assert_eq!(
            target,
            Some(TopLevelTarget {
                top_level: ResourceId(0x0010_0002),
                host_xid: 0xAA,
                x_offset: 15,
                y_offset: 25,
            })
        );
    }

    #[test]
    fn top_level_host_target_returns_none_for_root() {
        let table = ResourceTable::new();
        assert_eq!(table.top_level_host_target(ROOT_WINDOW), None);
    }

    #[test]
    fn top_level_host_target_returns_none_when_top_level_has_no_host_xid() {
        let mut table = ResourceTable::new();
        make_window(&mut table, 0x0010_0002); // no host_xid set
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
        assert_eq!(table.top_level_host_target(ResourceId(0x0010_0002)), None);
        assert_eq!(table.top_level_host_target(ResourceId(0x0010_0003)), None);
    }

    #[test]
    fn top_level_host_target_returns_none_for_orphaned_window() {
        let mut table = ResourceTable::new();
        // Build a child whose parent is a non-existent window (chain breaks).
        make_child(&mut table, 0x0010_0003, 0x9999_9999, 10, 20);
        assert_eq!(table.top_level_host_target(ResourceId(0x0010_0003)), None);
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
                top_level: ResourceId(0x0010_0002),
                host_xid: 0xAA,
                x_offset: 0,
                y_offset: 0,
                depth: 24,
            })
        );
    }

    #[test]
    fn host_drawable_target_child_window_with_accumulated_offsets() {
        let mut table = ResourceTable::new();
        make_top_level_with_host_xid(&mut table, 0x0010_0002, 0xBB);
        make_child(&mut table, 0x0010_0003, 0x0010_0002, 10, 20);
        let target = table.host_drawable_target(ResourceId(0x0010_0003));
        assert_eq!(
            target,
            Some(HostDrawableTarget::Window {
                nested: ResourceId(0x0010_0003),
                top_level: ResourceId(0x0010_0002),
                host_xid: 0xBB,
                x_offset: 10,
                y_offset: 20,
                depth: 24,
            })
        );
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
        assert!(table.set_pixmap_host_xid(ResourceId(0x0020_0002), 0xDEAD_BEEF));
        let target = table.host_drawable_target(ResourceId(0x0020_0002));
        assert_eq!(
            target,
            Some(HostDrawableTarget::Pixmap {
                nested: ResourceId(0x0020_0002),
                host_xid: 0xDEAD_BEEF,
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
        let result = table.set_pixmap_host_xid(ResourceId(0xDEAD), 0x1234);
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
        let result = table.set_pixmap_host_xid(ResourceId(0x0020_0002), 0x5678);
        assert!(result);
        assert_eq!(
            table.pixmap(ResourceId(0x0020_0002)).unwrap().host_xid,
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

        #[test]
        fn top_level_host_target_offset_proptest(
            n in 1usize..=8,
            offsets in proptest::collection::vec((any::<i16>(), any::<i16>()), 1..=8),
        ) {
            let depth = n.min(offsets.len());
            let mut table = ResourceTable::new();
            let top_level_id: u32 = 0x0010_0000;
            let host_xid: u32 = 0xCAFE;
            make_top_level_with_host_xid(&mut table, top_level_id, host_xid);

            let mut parent = top_level_id;
            let mut expected_x: i16 = 0;
            let mut expected_y: i16 = 0;
            let mut leaf = top_level_id;
            for (i, (x, y)) in offsets.iter().take(depth).enumerate() {
                let id: u32 = 0x0010_0001 + u32::try_from(i).unwrap();
                make_child(&mut table, id, parent, *x, *y);
                // The child contributes its own (x, y) to the offset only on the
                // way *up* — the helper walks from leaf to top-level and skips the
                // top-level's own (x, y), matching the spec.
                expected_x = expected_x.wrapping_add(*x);
                expected_y = expected_y.wrapping_add(*y);
                leaf = id;
                parent = id;
            }
            // The helper accumulates only ancestor offsets up to (but not
            // including) the top-level. So the leaf's own (x, y) is included
            // only if there is at least one intermediate ancestor between it
            // and the top-level — i.e., depth >= 2. For depth == 1, the leaf
            // is a direct child of the top-level and its (x, y) is the
            // accumulated offset.
            let target = table.top_level_host_target(ResourceId(leaf)).unwrap();
            prop_assert_eq!(target.top_level, ResourceId(top_level_id));
            prop_assert_eq!(target.host_xid, host_xid);
            prop_assert_eq!(target.x_offset, expected_x);
            prop_assert_eq!(target.y_offset, expected_y);
        }
    }
}
