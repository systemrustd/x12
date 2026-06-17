//! Per-kind newtypes wrapping host XIDs (or, in future backends,
//! native resource handles). All are `NonZeroU32` so that `0`
//! (X11's reserved value used as the None sentinel) is statically
//! unrepresentable in the success type and `Option<KindHandle>`
//! costs one word.

use std::num::NonZeroU32;

macro_rules! handle {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
        pub struct $name(NonZeroU32);

        impl $name {
            pub fn from_raw(raw: u32) -> Option<Self> {
                NonZeroU32::new(raw).map($name)
            }

            pub fn from_raw_panicking(raw: u32) -> Self {
                Self::from_raw(raw).unwrap_or_else(|| panic!("{} from zero raw", stringify!($name)))
            }

            pub fn as_raw(self) -> u32 {
                self.0.get()
            }

            #[cfg(test)]
            pub fn from_raw_for_test(raw: u32) -> Self {
                Self::from_raw_panicking(raw)
            }
        }
    };
}

handle!(
    WindowHandle,
    "Backend handle for an X11 InputOutput / InputOnly window."
);
handle!(PixmapHandle, "Backend handle for a pixmap.");
handle!(PictureHandle, "Backend handle for a RENDER picture.");
handle!(GlyphSetHandle, "Backend handle for a RENDER glyphset.");
handle!(FontHandle, "Backend handle for an opened font.");
handle!(CursorHandle, "Backend handle for a cursor.");
handle!(ColormapHandle, "Backend handle for a colormap.");
handle!(VisualHandle, "Backend handle for a visual.");

#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
pub enum AnyHandle {
    Window(WindowHandle),
    Pixmap(PixmapHandle),
}

#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum HandleKind {
    Window,
    Pixmap,
    Picture,
    GlyphSet,
    Font,
    Cursor,
    Colormap,
    Visual,
}

impl AnyHandle {
    pub fn kind(self) -> HandleKind {
        match self {
            AnyHandle::Window(_) => HandleKind::Window,
            AnyHandle::Pixmap(_) => HandleKind::Pixmap,
        }
    }

    pub fn as_raw(self) -> u32 {
        match self {
            AnyHandle::Window(h) => h.as_raw(),
            AnyHandle::Pixmap(h) => h.as_raw(),
        }
    }
}

impl From<WindowHandle> for AnyHandle {
    fn from(h: WindowHandle) -> Self {
        AnyHandle::Window(h)
    }
}

impl From<PixmapHandle> for AnyHandle {
    fn from(h: PixmapHandle) -> Self {
        AnyHandle::Pixmap(h)
    }
}
