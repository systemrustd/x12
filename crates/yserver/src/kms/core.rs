//! `KmsCore` — protocol-bookkeeping state shared between
//! `KmsBackend` (v1) and `KmsBackendV2` (v2, lands in Stage 1b).
//!
//! Per the rendering-model-v2 spec (`docs/superpowers/specs/
//! 2026-05-15-rendering-model-v2.md` § "KmsCore scope — narrowly
//! drawn"), this module owns **only** protocol state that
//! describes what the X11 protocol says exists (XID maps, window
//! metadata stripped of storage, fonts as logical entities, etc.).
//! It does **not** own Vulkan images, GPU pipelines, scanout BOs,
//! or anything keyed by `vk::*` types — those stay in the
//! backend-specific structs.
//!
//! The litmus test: a hypothetical all-CPU yserver backend would
//! need every field here; it would not need anything in
//! `KmsBackend`'s LEAVE-column fields.
//!
//! Several types and helpers that were previously private to
//! `kms::backend` have moved here too, with widened visibility
//! (`pub(crate)`) so `KmsBackend`'s rendering code can still
//! reach them.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
};

use yserver_core::{
    backend::{ClipState, FillState, GcFunction, PixmapHandle},
    host_x11::{HostPointerEvent, HostXidMap},
};
use yserver_protocol::x11::{CharInfo as ProtocolCharInfo, FontMetrics, ResourceId, xfixes};

use crate::kms::cpu_types::{PictTransform, Rectangle16, Repeat};

// ───────────────────────────────────────────────────────────────
// `Send` newtype wrappers around `!Send` third-party types.
// The kms backend is driven from a single core thread, so manual
// `Send` impls are sound — but the wrapper makes that explicit.
// ───────────────────────────────────────────────────────────────

/// Newtype wrapper around `freetype::Face`.
/// `repr(transparent)` is required so `RefCell::as_ptr` can be safely cast
/// from `*mut FreetypeFace` to `*mut freetype::Face` in `render_text_string`.
/// SAFETY: All access is on the single-threaded core thread.
/// Single-threaded context makes this sound. `Face` contains raw pointers
/// and `Rc<Vec<u8>>` by default, both `!Send`.
#[repr(transparent)]
pub struct FreetypeFace(#[allow(dead_code)] pub freetype::Face);
// SAFETY: see doc comment above.
unsafe impl Send for FreetypeFace {}

/// Newtype wrapper around `xkb::Context`.
/// SAFETY: All access is on the single-threaded core thread.
/// The raw pointer in xkbcommon is not `Send`, but the C library is thread-safe.
pub struct XkbContext(pub xkbcommon::xkb::Context);
// SAFETY: see doc comment above.
unsafe impl Send for XkbContext {}

/// Newtype wrapper around `xkb::Keymap`.
/// SAFETY: All access is on the single-threaded core thread.
pub struct XkbKeymap(pub xkbcommon::xkb::Keymap);
// SAFETY: see doc comment above.
unsafe impl Send for XkbKeymap {}

/// Newtype wrapper around `xkb::State`.
/// SAFETY: All access is on the single-threaded core thread.
pub struct XkbState(pub xkbcommon::xkb::State);
// SAFETY: see doc comment above.
unsafe impl Send for XkbState {}

// ───────────────────────────────────────────────────────────────
// Font protocol state (FontLoader + FontState + helpers).
// Pure protocol-domain: resolves X11 font names to FreeType faces
// via fontconfig. No Vulkan / GPU types reach in here.
// ───────────────────────────────────────────────────────────────

pub(crate) struct FontState {
    #[allow(dead_code)]
    pub(crate) handle: u32,
    pub(crate) face: RefCell<FreetypeFace>,
    pub(crate) metrics: FontMetrics,
    pub(crate) char_info_cache: HashMap<char, ProtocolCharInfo>,
}

/// Resolves X11 font names (aliases like `fixed`, XLFDs like
/// `-adobe-helvetica-bold-r-*-*-12-*-...`, or family names) to a filesystem
/// path via fontconfig, then opens the file with FreeType.
///
/// `catalog` is the list of XLFDs we advertise via ListFonts /
/// ListFontsWithInfo. It is built once at init time by enumerating
/// fontconfig's installed-font set and synthesising one XLFD per
/// (face × pixel-size × charset) combination. Every entry resolves
/// back through `open_font`, so the LFWI metrics path can return real
/// FreeType metrics for any name we hand out.
pub(crate) struct FontLoader {
    pub(crate) library: freetype::Library,
    pub(crate) fc: fontconfig::Fontconfig,
    pub(crate) catalog: Vec<String>,
}

impl FontLoader {
    pub(crate) fn new() -> io::Result<Self> {
        let fc = fontconfig::Fontconfig::new()
            .ok_or_else(|| io::Error::other("fontconfig init failed"))?;
        let catalog = build_font_catalog(&fc);
        log::info!("font catalog: {} XLFDs from fontconfig", catalog.len());
        Ok(Self {
            library: freetype::Library::init()
                .map_err(|e| io::Error::other(format!("freetype init failed: {e:?}")))?,
            fc,
            catalog,
        })
    }

    pub(crate) fn is_xlfd_pattern(name: &str) -> bool {
        name.starts_with('-')
    }

    /// Pull (family, style, pixel_size) hints out of an XLFD pattern.
    /// XLFD field indices after splitting on '-' (leading '-' produces an
    /// empty 0th element):
    ///   1=foundry 2=family 3=weight 4=slant 5=setwidth 6=addstyle
    ///   7=pixelsize 8=pointsize 9=resx 10=resy 11=spacing 12=avgwidth
    /// "*" or empty fields are treated as wildcards.
    pub(crate) fn parse_xlfd(name: &str) -> (Option<String>, Option<String>, Option<u32>) {
        let parts: Vec<&str> = name.split('-').collect();
        let take = |i: usize| -> Option<String> {
            parts
                .get(i)
                .filter(|s| !s.is_empty() && **s != "*")
                .map(|s| (*s).to_string())
        };
        let family = take(2);
        let weight = take(3);
        let slant = take(4);
        let style = match (weight.as_deref(), slant.as_deref()) {
            (None, Some("i" | "o")) => Some("Italic".to_string()),
            (Some(w), Some("i" | "o")) => Some(format!("{w} Italic")),
            (Some(w), _) => Some(w.to_string()),
            (None, _) => None,
        };
        let px = parts
            .get(7)
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|&s| s > 0);
        (family, style, px)
    }

    pub(crate) fn open_font(
        &self,
        name: &str,
    ) -> io::Result<(freetype::Face, FontMetrics, HashMap<char, ProtocolCharInfo>)> {
        // Resolve the X11 font name to a file path via fontconfig. We can't
        // rely on the high-level `Fontconfig::find`: when the requested family
        // doesn't exist, fontconfig falls back to the *system default* (often
        // a proportional sans-serif), which makes xterm/wmaker render with
        // wrong metrics. Build the pattern ourselves and chain "monospace" as
        // a secondary family — fontconfig prefers the first listed family but
        // falls through the chain before reaching its system default.
        let (family, style, xlfd_px) = if Self::is_xlfd_pattern(name) {
            Self::parse_xlfd(name)
        } else {
            (Some(name.to_string()), None, None)
        };
        let query_family = family.as_deref().unwrap_or("monospace");

        let cfamily = std::ffi::CString::new(query_family)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "font name has nul"))?;

        let mut pat = fontconfig::Pattern::new(&self.fc);
        pat.add_string(fontconfig::FC_FAMILY, &cfamily);
        if query_family != "monospace" {
            pat.add_string(fontconfig::FC_FAMILY, c"monospace");
        }
        let cstyle_storage;
        if let Some(style) = style.as_deref() {
            cstyle_storage = std::ffi::CString::new(style)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "font style has nul"))?;
            pat.add_string(fontconfig::FC_STYLE, &cstyle_storage);
        }
        let matched = pat.font_match();
        let path = matched.filename().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("font not found: {name}"))
        })?;
        let face_index: isize = matched.face_index().unwrap_or(0) as isize;
        let face = self
            .library
            .new_face(path, face_index)
            .map_err(|e| io::Error::other(format!("freetype new_face({path}) failed: {e:?}")))?;

        // Honour XLFD PIXEL_SIZE if specified; otherwise default to 12pt @ 96dpi.
        if let Some(px) = xlfd_px {
            let _ = face.set_pixel_sizes(0, px);
        } else {
            let _ = face.set_char_size(12 << 6, 12 << 6, 96, 96);
        }
        let (metrics, char_cache) = compute_font_metrics(&face);
        Ok((face, metrics, char_cache))
    }
}

fn compute_char_info(face: &freetype::Face, ch: char) -> ProtocolCharInfo {
    let glyph_idx = ch as usize;
    let _ = face.load_char(glyph_idx, freetype::face::LoadFlag::RENDER);
    let glyph = face.glyph();
    let bitmap = glyph.bitmap();
    let metrics = glyph.metrics();

    let width = (metrics.horiAdvance >> 6) as i16;
    let left_side_bearing = (metrics.horiBearingX >> 6) as i16;
    let right_side_bearing = left_side_bearing + bitmap.width() as i16;
    let ascent = (metrics.horiBearingY >> 6) as i16;
    let descent = (bitmap.rows() as i16) - ascent;

    ProtocolCharInfo {
        left_side_bearing,
        right_side_bearing,
        character_width: width,
        ascent,
        descent,
        attributes: 0,
    }
}

fn compute_font_metrics(face: &freetype::Face) -> (FontMetrics, HashMap<char, ProtocolCharInfo>) {
    let mut char_info_cache = HashMap::new();
    let mut min_bounds = ProtocolCharInfo {
        left_side_bearing: i16::MAX,
        right_side_bearing: i16::MAX,
        character_width: i16::MAX,
        ascent: i16::MAX,
        descent: i16::MAX,
        attributes: 0,
    };
    let mut max_bounds = ProtocolCharInfo {
        left_side_bearing: i16::MIN,
        right_side_bearing: i16::MIN,
        character_width: i16::MIN,
        ascent: i16::MIN,
        descent: i16::MIN,
        attributes: 0,
    };

    for code in 0x20u32..=0x7E {
        let ch = char::from_u32(code).unwrap();
        let ci = compute_char_info(face, ch);
        min_bounds.left_side_bearing = min_bounds.left_side_bearing.min(ci.left_side_bearing);
        max_bounds.left_side_bearing = max_bounds.left_side_bearing.max(ci.left_side_bearing);
        min_bounds.right_side_bearing = min_bounds.right_side_bearing.min(ci.right_side_bearing);
        max_bounds.right_side_bearing = max_bounds.right_side_bearing.max(ci.right_side_bearing);
        min_bounds.character_width = min_bounds.character_width.min(ci.character_width);
        max_bounds.character_width = max_bounds.character_width.max(ci.character_width);
        min_bounds.ascent = min_bounds.ascent.min(ci.ascent);
        max_bounds.ascent = max_bounds.ascent.max(ci.ascent);
        min_bounds.descent = min_bounds.descent.min(ci.descent);
        max_bounds.descent = max_bounds.descent.max(ci.descent);
        char_info_cache.insert(ch, ci);
    }

    let font_ascent = max_bounds.ascent;
    let font_descent = max_bounds.descent;

    let metrics = FontMetrics {
        min_bounds,
        max_bounds,
        min_char_or_byte2: 0x20,
        max_char_or_byte2: 0x7E,
        default_char: 0x20,
        draw_direction: 0, // LeftToRight
        min_byte1: 0,
        max_byte1: 0,
        all_chars_exist: true,
        font_ascent,
        font_descent,
        properties: Vec::new(),
        char_infos: char_info_cache.values().cloned().collect(),
    };
    (metrics, char_info_cache)
}

/// Map a fontconfig integer `weight` (`FC_WEIGHT_*`) to the X11 XLFD
/// weight name. Buckets follow the canonical fontconfig→XLFD mapping
/// used by Xft and traditional X font path synthesis.
pub(crate) fn xlfd_weight(w: i32) -> &'static str {
    match w {
        ..=49 => "thin",
        50..=74 => "light",
        75..=89 => "book",
        90..=139 => "medium",
        140..=189 => "demibold",
        190..=209 => "bold",
        _ => "black",
    }
}

/// Map a fontconfig integer `slant` to the single-letter XLFD slant.
pub(crate) fn xlfd_slant(s: i32) -> &'static str {
    match s {
        100 => "i", // italic
        110 => "o", // oblique
        _ => "r",   // roman
    }
}

/// Map a fontconfig integer `spacing` to the single-letter XLFD spacing.
pub(crate) fn xlfd_spacing(s: i32) -> &'static str {
    match s {
        100 => "m", // monospaced
        110 => "c", // charcell
        _ => "p",   // proportional
    }
}

/// Make a fontconfig string safe to drop into a single XLFD field:
/// dashes are field separators in XLFDs, so any embedded `-` is
/// replaced with a space. XLFD is case-insensitive but conventionally
/// lowercase.
pub(crate) fn sanitize_xlfd_field(s: &str) -> String {
    s.replace('-', " ").to_lowercase()
}

/// Enumerate the installed font set via fontconfig and synthesise one
/// XLFD per (face × pixel-size × charset) combination. Every entry is
/// a real font that `FontLoader::open_font` can resolve back to a
/// `FreeType::Face` — there are no stub XLFDs.
///
/// Charsets are limited to `iso8859-1` and `iso10646-1` because those
/// two are the universal subset every scalable font supports through
/// FreeType's char-by-char lookup. Locale-specific charsets (jisx*,
/// gb2312*, ksc*) need real fonts on disk to satisfy properly; rather
/// than stub them, we let libXt warn "Missing charset X" and proceed
/// with the iso* coverage that's guaranteed real.
pub(crate) fn build_font_catalog(fc: &fontconfig::Fontconfig) -> Vec<String> {
    const PIXEL_SIZES: &[u32] = &[8, 10, 12, 14, 16, 18, 24];
    const CHARSETS: &[&str] = &["iso8859-1", "iso10646-1"];

    let pat = fontconfig::Pattern::new(fc);
    let mut objs = fontconfig::ObjectSet::new(fc);
    objs.add(fontconfig::FC_FAMILY);
    objs.add(fontconfig::FC_FOUNDRY);
    objs.add(fontconfig::FC_WEIGHT);
    objs.add(fontconfig::FC_SLANT);
    objs.add(fontconfig::FC_SPACING);
    let set = fontconfig::list_fonts(&pat, Some(&objs));

    // Aliases the loader handles directly without an XLFD parse pass.
    let mut entries: Vec<String> = vec!["fixed".into(), "cursor".into(), "nil2".into()];
    let mut seen: HashSet<(String, String, i32, i32, i32)> = HashSet::new();

    for font in set.iter() {
        let Some(family) = font.get_string(fontconfig::FC_FAMILY) else {
            continue;
        };
        let foundry = font.get_string(fontconfig::FC_FOUNDRY).unwrap_or("misc");
        let weight = font.get_int(fontconfig::FC_WEIGHT).unwrap_or(80);
        let slant = font.get_int(fontconfig::FC_SLANT).unwrap_or(0);
        let spacing = font.get_int(fontconfig::FC_SPACING).unwrap_or(0);

        let key = (
            family.to_lowercase(),
            foundry.to_lowercase(),
            weight,
            slant,
            spacing,
        );
        if !seen.insert(key) {
            continue;
        }

        let foundry_x = sanitize_xlfd_field(foundry);
        let family_x = sanitize_xlfd_field(family);
        let weight_x = xlfd_weight(weight);
        let slant_x = xlfd_slant(slant);
        let spacing_x = xlfd_spacing(spacing);

        for &px in PIXEL_SIZES {
            // Average width estimate: ~0.6 × pixel size, in 1/10 px.
            // Approximation only — clients filter by name pattern, not
            // by this field, and QueryFont returns the real widths.
            let avg_width = (px * 6).clamp(1, 999);
            for &charset in CHARSETS {
                entries.push(format!(
                    "-{foundry_x}-{family_x}-{weight_x}-{slant_x}-normal--{px}-{}-75-75-{spacing_x}-{avg_width}-{charset}",
                    px * 10,
                ));
            }
        }
    }

    entries
}

// ───────────────────────────────────────────────────────────────
// RENDER glyphset protocol state.
// Glyphset records keyed by host xid; the glyph bytes are CPU-side
// (uploaded later into v1's `glyph_atlas` or v2's `RenderEngine`).
// ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GlyphSetFormat {
    A8,
    A1,
    /// ARGB32 source glyphs (Cairo's default for modern GTK3 themes
    /// using subpixel / colour-emoji rendering). On `AddGlyphs` we
    /// extract the alpha channel into a densely-packed A8 buffer and
    /// store the glyph as if it had been uploaded in A8 format — the
    /// downstream atlas + text pipeline path is identical from there
    /// on. Subpixel coverage detail and emoji colour are lost; glyph
    /// shape is preserved, which is enough for grayscale text.
    Argb32,
    Other,
}

pub(crate) struct StoredGlyph {
    pub(crate) width: u16,
    pub(crate) height: u16,
    /// RENDER wire field: top-left of bitmap relative to glyph origin.
    /// This is the *negative* of FreeType's bitmap_left.
    /// Draw at pen_x - x, pen_y - y.
    pub(crate) x: i16,
    pub(crate) y: i16,
    pub(crate) x_off: i16,
    /// Vertical pen advance. Parsed from wire for fidelity but unused —
    /// horizontal-text rendering only advances the x pen between glyphs.
    #[allow(dead_code)]
    pub(crate) y_off: i16,
    /// Row-major A8 bytes, densely packed (no per-row padding).
    pub(crate) pixels: Vec<u8>,
    pub(crate) format: GlyphSetFormat,
}

pub(crate) struct GlyphSetState {
    pub(crate) format: GlyphSetFormat,
    pub(crate) glyphs: HashMap<u32, StoredGlyph>,
}

// ───────────────────────────────────────────────────────────────
// RENDER picture records (CPU-only, Stage 3b).
//
// Per the v2 spec § "KmsCore scope", picture *records* (the
// protocol's idea of a picture: op, src/dst xid, transform,
// filter, clip, repeat, alpha-map) live in `KmsCore`. The Vk
// pipeline / sampler / image-view side belongs to
// `RenderEngine`'s `picture_paint` map. v1 keeps its own
// `KmsBackend.pictures: HashMap<u32, PictureState>` (which
// carries Vk-typed gradients) — these `PictureRecord` types
// are v2-only.
// ───────────────────────────────────────────────────────────────

/// One stop in a gradient. `pos` is X11 fixed-point 16.16;
/// colour channels are 16-bit straight (non-premultiplied) per
/// the X RENDER wire format. v2's `RenderEngine` premultiplies
/// at gradient-LUT build time. Layout-compatible with
/// `crate::kms::vk::gradient::Stop` (a deliberate mirror so the
/// engine can `mem::transmute`-equivalent the slice on
/// conversion; never actually transmuted but a layout-check
/// `#[allow(unused)]` guard sits next to the v1 type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GradientStop {
    pub(crate) pos: i32,
    pub(crate) r: u16,
    pub(crate) g: u16,
    pub(crate) b: u16,
    pub(crate) a: u16,
}

/// Stage 3b: protocol record of an X RENDER Picture. CPU-only;
/// every variant is `Copy`-friendly except for the small `Vec`s
/// (clip rects, gradient stops).
#[allow(
    dead_code,
    reason = "Gradient endpoint + stops fields are consumed by Stage 3c \
              when RenderEngine builds the gradient LUT"
)]
#[derive(Debug, Clone)]
pub(crate) enum PictureRecord {
    /// Picture wraps a Drawable (window or pixmap). Composite ops
    /// read pixels from the backing drawable's storage; paint ops
    /// (Composite as dst, FillRectangles, etc.) write into it.
    Drawable {
        /// XID of the backing window or pixmap.
        host_xid: u32,
        /// X11 `RENDER_PICTFORMAT` ID requested at `CreatePicture`.
        /// Captures the client's *declared* sampling intent — e.g.
        /// marco creates a Picture wrapping a depth-24 backing with
        /// a 24-bit xRGB format, but a depth-32 backing with ARGB32.
        /// The drawable's depth alone is not enough to distinguish
        /// "no alpha" from "real alpha" cases (rare but real for
        /// X RENDER source-format flexibility). Currently
        /// instrumentation-only — the engine still chooses
        /// force-opaque via drawable depth; PictFormat-aware
        /// sampling lands as a follow-on once we have data showing
        /// where the depth-based heuristic diverges.
        pict_format: u32,
        /// Optional clip rectangles (set by
        /// `RenderSetPictureClipRectangles`). Stored in dst-coord
        /// space — the clip-origin has already been folded in.
        clip: Option<Vec<Rectangle16>>,
        clip_x: i16,
        clip_y: i16,
        repeat: Repeat,
        alpha_map: Option<u32>,
        alpha_x: i16,
        alpha_y: i16,
        component_alpha: bool,
        transform: Option<PictTransform>,
        /// X RENDER `RENDER_FILTER` enum. `Nearest` is the only
        /// honoured filter in Stage 3 (per spec § "Out of scope");
        /// `Bilinear`/`Convolution` parse + store but the
        /// `RenderEngine` ignores them at draw time.
        filter: PictureFilter,
        graphics_exposure: bool,
        subwindow_mode: u8,
        poly_edge: u8,
        poly_mode: u8,
    },
    /// `RenderCreateSolidFill` source. The X RENDER wire colour
    /// is **already premultiplied** (per the protocol spec, and
    /// confirmed by rendercheck `main.c:337-345`); v2 stores it
    /// as-is so the pipeline's sampler reads exactly what the
    /// client sent.
    SolidFill {
        premul: [f32; 4],
        repeat: Repeat,
        component_alpha: bool,
    },
    /// `RenderCreateLinearGradient` source. Stops are kept
    /// straight (non-premultiplied) — the v2 RenderEngine
    /// premultiplies when building the gradient LUT in Stage 3c.
    LinearGradient {
        /// Endpoints in X11 fixed-point 16.16 (`p1`, `p2`).
        p1: (i32, i32),
        p2: (i32, i32),
        stops: Vec<GradientStop>,
        repeat: Repeat,
        transform: Option<PictTransform>,
    },
    /// `RenderCreateRadialGradient` source. `(cx, cy, r)` for
    /// both the inner and outer circles, X11 fixed-point.
    RadialGradient {
        inner: (i32, i32, i32),
        outer: (i32, i32, i32),
        stops: Vec<GradientStop>,
        repeat: Repeat,
        transform: Option<PictTransform>,
    },
}

/// X RENDER picture filter selector. The protocol carries the
/// filter as a byte-string name; v2 maps the standard names into
/// this enum at request time. Unknown filters degrade to
/// `Nearest` per Stage 3's "only Nearest is honoured" rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PictureFilter {
    #[default]
    Nearest,
    Bilinear,
    /// Server-known convolution kernel selector. The kernel bytes
    /// live in the request's value-list; v2 doesn't apply them
    /// (Stage 5 territory), but parsing them as `Convolution`
    /// keeps the picture-record round-trip honest.
    Convolution,
}

impl PictureRecord {
    /// Construct a default `PictureRecord::Drawable` against
    /// `host_xid`. All optional / typed fields take their X RENDER
    /// protocol defaults; subsequent `RenderChangePicture` /
    /// `RenderSetPicture*` calls mutate them.
    pub(crate) fn drawable_default(host_xid: u32, pict_format: u32) -> Self {
        PictureRecord::Drawable {
            host_xid,
            pict_format,
            clip: None,
            clip_x: 0,
            clip_y: 0,
            repeat: Repeat::None,
            alpha_map: None,
            alpha_x: 0,
            alpha_y: 0,
            component_alpha: false,
            transform: None,
            filter: PictureFilter::Nearest,
            graphics_exposure: false,
            subwindow_mode: 0,
            poly_edge: 0,
            poly_mode: 0,
        }
    }

    /// Convenience: the backing drawable host xid for a `Drawable`
    /// variant, or `None` for the source-only variants
    /// (SolidFill / Gradient). Used by `render_free_picture` to
    /// decide whether to call `DrawableStore::decref`.
    pub(crate) fn drawable_host_xid(&self) -> Option<u32> {
        if let PictureRecord::Drawable { host_xid, .. } = self {
            Some(*host_xid)
        } else {
            None
        }
    }
}

// ───────────────────────────────────────────────────────────────
// COMPOSITE redirect alias bookkeeping.
//
// `AliasRegistry::decref` returns `true` when the refcount hit zero,
// signalling the caller (backend-specific) that the underlying
// storage can be released. This struct only owns the protocol-side
// refcount + (width, height, depth) snapshot; it doesn't touch
// storage handles itself.
// ───────────────────────────────────────────────────────────────

/// One entry in the [`AliasRegistry`]: tracks the refcount on a
/// `Composite::NameWindowPixmap` backing pixmap, plus the
/// width/height/depth snapshot the alias was created against.
///
/// Refcount sources, per the L2 spec:
///
///   1. Active redirect on the matching window
///      (`Window.redirected_backing`).
///   2. Each live `NameWindowPixmap` alias on this backing.
///
/// The backing drops when refcount reaches zero — Unredirect
/// without aliases (1 → 0), DestroyWindow with surviving aliases
/// (1 → 0 from owner; aliases keep it alive), final FreePixmap of
/// the last alias (after Unredirect or Destroy).
#[allow(dead_code, reason = "width/height/depth consumed by B.6d on resize")]
#[derive(Clone, Copy, Debug)]
pub struct AliasEntry {
    pub refcount: u32,
    pub width: u16,
    pub height: u16,
    pub depth: u8,
}

/// Refcounted backing-pixmap registry on the KMS backend. Keyed by
/// the backing's `host_xid` (the `PixmapHandle` the protocol layer
/// sees through `Window.redirected_backing.host_pixmap`).
/// L2 plan B.3.
#[derive(Default, Debug)]
pub struct AliasRegistry {
    entries: HashMap<u32, AliasEntry>,
}

#[allow(
    dead_code,
    reason = "get/decref/len/is_empty consumed by B.6c (Unredirect teardown) + B.10c (overlay-demote quiescence); B.6d uses the width/height fields for resize check"
)]
impl AliasRegistry {
    /// Add the initial entry for a backing pixmap. Caller seeds the
    /// `refcount` (typically 1 for the redirect-activation hold).
    pub fn insert(&mut self, host_pixmap: PixmapHandle, entry: AliasEntry) {
        self.entries.insert(host_pixmap.as_raw(), entry);
    }

    /// Refcount-only lookup; returns `None` if the backing isn't
    /// tracked (e.g. the redirect was torn down).
    #[must_use]
    pub fn get(&self, host_pixmap: PixmapHandle) -> Option<&AliasEntry> {
        self.entries.get(&host_pixmap.as_raw())
    }

    /// Bump the refcount. Silently no-ops on an unknown handle — the
    /// caller is expected to have inserted the entry first
    /// (`Composite::NameWindowPixmap`'s ordering guarantees that).
    pub fn incref(&mut self, host_pixmap: PixmapHandle) {
        if let Some(e) = self.entries.get_mut(&host_pixmap.as_raw()) {
            e.refcount = e.refcount.saturating_add(1);
        }
    }

    /// Drop one reference. Returns `true` if the entry was removed
    /// (refcount reached zero). Returns `false` on an unknown handle
    /// or when refs remain — caller uses the return value to decide
    /// whether to release the underlying Vulkan image.
    pub fn decref(&mut self, host_pixmap: PixmapHandle) -> bool {
        let key = host_pixmap.as_raw();
        let Some(entry) = self.entries.get_mut(&key) else {
            return false;
        };
        entry.refcount = entry.refcount.saturating_sub(1);
        if entry.refcount == 0 {
            self.entries.remove(&key);
            true
        } else {
            false
        }
    }

    /// Number of tracked backings. Mostly for test introspection.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True iff no backings are tracked. Used by overlay-demotion
    /// quiescence checks (B.10c).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ───────────────────────────────────────────────────────────────
// The shared `KmsCore` struct.
// ───────────────────────────────────────────────────────────────

/// Protocol-bookkeeping state shared between `KmsBackend` (v1) and
/// `KmsBackendV2` (v2). Owns *what the protocol says exists*, not
/// *how the backend stores it*.
pub(crate) struct KmsCore {
    // XID / ID maps
    pub(crate) xid_map: HostXidMap,
    pub(crate) next_host_xid: u32,
    pub(crate) window_id: u32,
    pub(crate) root_visual_xid: u32,
    pub(crate) top_level_order: Vec<u32>,

    // Keyboard protocol state
    #[allow(dead_code)]
    pub(crate) xkb_context: XkbContext,
    pub(crate) xkb_keymap: XkbKeymap,
    pub(crate) xkb_state: XkbState,

    // Font protocol state
    pub(crate) font_loader: FontLoader,
    pub(crate) fonts: HashMap<u32, FontState>,

    // Root background metadata (paint into root storage is
    // backend-specific; the metadata that *what* the root
    // background should be lives here)
    pub(crate) bg_pixel: Option<u32>,
    pub(crate) bg_pixmap: Option<PixmapHandle>,

    // Pointer protocol state
    pub(crate) cursor_x: f32,
    pub(crate) cursor_y: f32,
    pub(crate) active_cursor: Option<u32>,
    pub(crate) button_mask: u16,
    pub(crate) prev_pointer_window: Option<u32>,
    pub(crate) pending_pointer_events: Vec<HostPointerEvent>,

    // Default GC state (the in-progress GC values feeding paint paths)
    pub(crate) current_font: Option<u32>,
    pub(crate) current_function: GcFunction,
    pub(crate) current_foreground: u32,
    pub(crate) current_background: u32,
    pub(crate) current_fill: FillState,
    pub(crate) current_clip: ClipState,

    // SHAPE extension: per-window shape regions keyed by host XID.
    // None entry = no shape (full rectangle). Some(vec![]) = empty region.
    pub(crate) shape_bounding: HashMap<u32, Vec<xfixes::RegionRect>>, // kind=0
    pub(crate) shape_clip: HashMap<u32, Vec<xfixes::RegionRect>>,     // kind=1
    pub(crate) shape_input: HashMap<u32, Vec<xfixes::RegionRect>>,    // kind=2

    // COMPOSITE redirect records
    pub(crate) alias_registry: AliasRegistry,
    /// `host_window → backing_pixmap_handle` map populated by
    /// `allocate_redirected_backing`; `name_window_pixmap` looks
    /// up the matching backing here. Cleared when a redirect is
    /// torn down.
    pub(crate) host_window_to_backing: HashMap<u32, PixmapHandle>,
    /// Stage 4d — Composite Overlay Window refcount.
    ///
    /// Live count of outstanding `XComposite::GetOverlayWindow`
    /// holds. Bumped on every GET, decremented on every RELEASE;
    /// when it hits zero the backend drops the underlying COW
    /// storage. Protocol-side bookkeeping per the v2 plan
    /// §"`KmsCore` scope — narrowly drawn": this is metadata (a
    /// counter), not a storage handle. The `DrawableId` of the
    /// allocated COW storage lives on `KmsBackendV2.cow_id`.
    ///
    /// See Stage 4 plan §4d "Composite Overlay Window (COW) as
    /// first-class scene entry" + spec lines 453, 459, 470 for
    /// the protocol/storage split.
    pub(crate) cow_refcount: u32,

    // RENDER glyphset source data (atlas storage is backend-specific)
    pub(crate) glyphsets: HashMap<u32, GlyphSetState>,

    // RENDER picture records (Stage 3b, v2-only). v1 carries its
    // own `KmsBackend.pictures` with Vk-typed gradient state;
    // these are CPU-only and v2 reads/writes them directly.
    pub(crate) pictures: HashMap<u32, PictureRecord>,
}

impl KmsCore {
    /// Build the shared protocol-bookkeeping state for a real
    /// backend. Initialises the XKB context+keymap+state, the
    /// FontLoader (fontconfig + freetype), and seeds the root
    /// xid map entry.
    ///
    /// Cursor position seeds at the centre of `(fb_w, fb_h)` to
    /// match the input thread's initial pointer position
    /// (`LibinputThreadState::new`) — without this match, the
    /// first synthetic Enter event after window-map can carry a
    /// `(0, 0)` location, which trips GTK's gesture-drag anchor.
    ///
    /// # Errors
    ///
    /// Returns `io::Error` on FontLoader init failure or XKB
    /// keymap construction failure.
    pub(crate) fn new(fb_w: u16, fb_h: u16) -> io::Result<Self> {
        let xkb_context = XkbContext(xkbcommon::xkb::Context::new(
            xkbcommon::xkb::CONTEXT_NO_FLAGS,
        ));
        let keymap = xkbcommon::xkb::Keymap::new_from_names(
            &xkb_context.0,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .or_else(|| {
            xkbcommon::xkb::Keymap::new_from_names(
                &xkb_context.0,
                "",
                "",
                "",
                "",
                None,
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        })
        .ok_or_else(|| io::Error::other("failed to create xkb keymap"))?;
        let xkb_state = XkbState(xkbcommon::xkb::State::new(&keymap));
        let xkb_keymap = XkbKeymap(keymap);

        let mut xid_map: HostXidMap = HashMap::new();
        xid_map.insert(0x0000_0001u32, ResourceId(0x0000_0100));

        Ok(Self {
            xid_map,
            next_host_xid: 0x0040_0000,
            window_id: 1,
            root_visual_xid: 0x21,
            top_level_order: Vec::new(),
            xkb_context,
            xkb_keymap,
            xkb_state,
            font_loader: FontLoader::new()?,
            fonts: HashMap::new(),
            bg_pixel: None,
            bg_pixmap: None,
            cursor_x: f32::from(fb_w) / 2.0,
            cursor_y: f32::from(fb_h) / 2.0,
            active_cursor: None,
            button_mask: 0,
            prev_pointer_window: None,
            pending_pointer_events: Vec::new(),
            current_font: None,
            current_function: GcFunction::Copy,
            current_foreground: 0,
            current_background: 0x00ff_ffff,
            current_fill: FillState::Solid,
            current_clip: ClipState::None,
            shape_bounding: HashMap::new(),
            shape_clip: HashMap::new(),
            shape_input: HashMap::new(),
            alias_registry: AliasRegistry::default(),
            host_window_to_backing: HashMap::new(),
            cow_refcount: 0,
            glyphsets: HashMap::new(),
            pictures: HashMap::new(),
        })
    }

    /// Headless test seed. Mirrors `new` but panics on init
    /// failure (FontLoader / XKB) since test fixtures shouldn't
    /// be running without these. Uses an 800×600 cursor centre
    /// matching `KmsBackend::for_tests`'s synthetic output.
    #[doc(hidden)]
    #[must_use]
    pub(crate) fn for_tests() -> Self {
        let xkb_context = XkbContext(xkbcommon::xkb::Context::new(
            xkbcommon::xkb::CONTEXT_NO_FLAGS,
        ));
        let keymap = xkbcommon::xkb::Keymap::new_from_names(
            &xkb_context.0,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .or_else(|| {
            xkbcommon::xkb::Keymap::new_from_names(
                &xkb_context.0,
                "",
                "",
                "",
                "",
                None,
                xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        })
        .expect("test xkb keymap");
        let xkb_state = XkbState(xkbcommon::xkb::State::new(&keymap));
        let xkb_keymap = XkbKeymap(keymap);

        Self {
            xid_map: HostXidMap::new(),
            next_host_xid: 0x0040_0000,
            window_id: 1,
            root_visual_xid: 0x21,
            top_level_order: Vec::new(),
            xkb_context,
            xkb_keymap,
            xkb_state,
            font_loader: FontLoader::new().expect("test font loader"),
            fonts: HashMap::new(),
            bg_pixel: None,
            bg_pixmap: None,
            cursor_x: 400.0,
            cursor_y: 300.0,
            active_cursor: None,
            button_mask: 0,
            prev_pointer_window: None,
            pending_pointer_events: Vec::new(),
            current_font: None,
            current_function: GcFunction::Copy,
            current_foreground: 0,
            current_background: 0x00ff_ffff,
            current_fill: FillState::Solid,
            current_clip: ClipState::None,
            shape_bounding: HashMap::new(),
            shape_clip: HashMap::new(),
            shape_input: HashMap::new(),
            alias_registry: AliasRegistry::default(),
            host_window_to_backing: HashMap::new(),
            cow_refcount: 0,
            glyphsets: HashMap::new(),
            pictures: HashMap::new(),
        }
    }

    /// Allocate the next host-XID. Monotonic counter starting at
    /// `0x0040_0000`. Wraps around at `u32::MAX` (in practice the
    /// counter never exceeds a few million in any real session).
    pub(crate) fn next_host_xid(&mut self) -> u32 {
        self.next_host_xid = self
            .next_host_xid
            .checked_add(1)
            .expect("host xid counter overflow");
        self.next_host_xid
    }
}
