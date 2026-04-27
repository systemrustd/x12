#![allow(dead_code)]

use std::collections::HashMap;

use yserver_protocol::x11::{
    ChangeWindowAttributesRequest, ConfigureWindowRequest, CreateGcRequest, CreatePixmapRequest,
    CreateWindowRequest, FontMetrics, GcChange, ResourceId,
};

pub const ROOT_WINDOW: ResourceId = ResourceId(0x100);
pub const ROOT_COLORMAP: ResourceId = ResourceId(0x101);
pub const ROOT_VISUAL: ResourceId = ResourceId(0x102);

#[derive(Debug)]
pub struct ResourceTable {
    windows: HashMap<u32, Window>,
    pixmaps: HashMap<u32, Pixmap>,
    gcs: HashMap<u32, Gc>,
    fonts: HashMap<u32, Font>,
    cursors: HashMap<u32, Cursor>,
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
                event_mask: 0,
                background_pixel: 0x00ff_ffff,
                override_redirect: false,
                cursor: None,
            },
        );

        Self {
            windows,
            pixmaps: HashMap::new(),
            gcs: HashMap::new(),
            fonts: HashMap::new(),
            cursors: HashMap::new(),
        }
    }

    pub fn create_window(&mut self, request: CreateWindowRequest) {
        let window = Window {
            id: request.window,
            parent: request.parent,
            children: Vec::new(),
            x: request.x,
            y: request.y,
            width: request.width,
            height: request.height,
            border_width: request.border_width,
            depth: request.depth,
            visual: if request.visual.0 == 0 {
                ROOT_VISUAL
            } else {
                request.visual
            },
            class: WindowClass::from_protocol(request.class),
            map_state: MapState::Unmapped,
            event_mask: request.event_mask.unwrap_or(0),
            background_pixel: request.background_pixel.unwrap_or(0x00ff_ffff),
            override_redirect: request.override_redirect.unwrap_or(false),
            cursor: None,
        };

        self.windows
            .entry(request.parent.0)
            .or_insert_with(|| Window::placeholder(request.parent))
            .children
            .push(request.window);
        self.windows.insert(request.window.0, window);
    }

    pub fn destroy_window(&mut self, id: ResourceId) {
        let Some(window) = self.windows.remove(&id.0) else {
            return;
        };
        if let Some(parent) = self.windows.get_mut(&window.parent.0) {
            parent.children.retain(|child| *child != id);
        }
        for child in window.children {
            self.destroy_window(child);
        }
    }

    pub fn change_window_attributes(&mut self, request: ChangeWindowAttributesRequest) {
        if let Some(window) = self.windows.get_mut(&request.window.0) {
            if let Some(background_pixel) = request.background_pixel {
                window.background_pixel = background_pixel;
            }
            if let Some(event_mask) = request.event_mask {
                window.event_mask = event_mask;
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

    pub fn map_window(&mut self, id: ResourceId) {
        if let Some(window) = self.windows.get_mut(&id.0) {
            window.map_state = MapState::Viewable;
        }
    }

    pub fn unmap_window(&mut self, id: ResourceId) {
        if let Some(window) = self.windows.get_mut(&id.0) {
            window.map_state = MapState::Unmapped;
        }
    }

    pub fn window(&self, id: ResourceId) -> Option<&Window> {
        self.windows.get(&id.0)
    }

    pub fn children(&self, parent: ResourceId) -> &[ResourceId] {
        self.windows
            .get(&parent.0)
            .map_or(&[], |window| window.children.as_slice())
    }

    pub fn create_pixmap(&mut self, request: CreatePixmapRequest) {
        self.pixmaps.insert(
            request.pixmap.0,
            Pixmap {
                id: request.pixmap,
                drawable: request.drawable,
                width: request.width,
                height: request.height,
                depth: request.depth,
            },
        );
    }

    pub fn free_pixmap(&mut self, id: ResourceId) {
        self.pixmaps.remove(&id.0);
    }

    pub fn pixmap(&self, id: ResourceId) -> Option<&Pixmap> {
        self.pixmaps.get(&id.0)
    }

    pub fn create_gc(&mut self, request: CreateGcRequest) {
        self.gcs.insert(
            request.gc.0,
            Gc {
                id: request.gc,
                drawable: request.drawable,
                foreground: request.foreground.unwrap_or(0),
                background: request.background.unwrap_or(0x00ff_ffff),
                line_width: request.line_width.unwrap_or(0),
                font: request.font,
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

    pub fn install_font(
        &mut self,
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

    pub fn create_glyph_cursor(&mut self, id: ResourceId) {
        self.cursors.insert(id.0, Cursor { id });
    }

    pub fn free_cursor(&mut self, id: ResourceId) {
        self.cursors.remove(&id.0);
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
    pub event_mask: u32,
    pub background_pixel: u32,
    pub override_redirect: bool,
    pub cursor: Option<ResourceId>,
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
            event_mask: 0,
            background_pixel: 0x00ff_ffff,
            override_redirect: false,
            cursor: None,
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
}

#[derive(Clone, Debug)]
pub struct Gc {
    pub id: ResourceId,
    pub drawable: ResourceId,
    pub foreground: u32,
    pub background: u32,
    pub line_width: u16,
    pub font: Option<ResourceId>,
}

#[derive(Clone, Debug)]
pub struct Font {
    pub id: ResourceId,
    pub name: String,
    pub host_xid: u32,
    pub metrics: FontMetrics,
}

#[derive(Clone, Debug)]
pub struct Cursor {
    pub id: ResourceId,
}
