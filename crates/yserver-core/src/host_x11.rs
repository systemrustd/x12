use std::{
    collections::HashMap,
    env, fs,
    io::{self, ErrorKind, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use log::debug;
use yserver_protocol::x11::{self, ClipRectangles, FontMetrics, ResourceId};

const MIT_MAGIC_COOKIE: &str = "MIT-MAGIC-COOKIE-1";

struct HostRenderInfo {
    opcode: u8,
    fmt_a1: u32,
    fmt_a8: u32,
    fmt_rgb24: u32,
    fmt_argb32: u32,
}

struct HostXkbInfo {
    opcode: u8,
    first_event: u8,
    first_error: u8,
}

pub struct HostX11 {
    stream: UnixStream,
    window_id: u32,
    gc_id: u32,
    current_foreground: u32,
    current_background: u32,
    current_clip: HostClipState,
    sequence: u16,
    next_xid: u32,
    render: Option<HostRenderInfo>,
    xkb: Option<HostXkbInfo>,
    /// Major opcode of the host's SHAPE extension, cached on init. `None`
    /// means the host doesn't advertise SHAPE — forwarders become no-ops.
    shape_opcode: Option<u8>,
    /// Major opcode of the host's XFIXES extension. Used so far only by
    /// `ChangeCursorByName`; other XFIXES requests are still served locally.
    xfixes_opcode: Option<u8>,
    /// Major opcode of the host's COMPOSITE extension. Used to forward
    /// `Composite::NameWindowPixmap` so that compositors (picom, mutter)
    /// see actual host backing-store contents through our nested layer.
    /// `None` means the host doesn't advertise COMPOSITE — clients then
    /// receive `BadAlloc` for `NameWindowPixmap`.
    composite_opcode: Option<u8>,
    // Responses read during create_subwindow drain loops that belong to future
    // requests (sequence > geom_seq at time of read). Without this buffer,
    // the drain loop for window N discards the GetGeometry reply for window N+k,
    // causing the subsequent drain loop to hang forever.
    reply_buffer: Vec<HostResponse>,
    // GCs cached per pixmap depth. The default `gc_id` is bound to a depth-24
    // drawable so PutImage onto pixmaps with a different depth (e.g. depth-8
    // alpha masks for RENDER) would BadMatch. We lazily create one GC per
    // depth using the target drawable as the screen-and-depth reference.
    depth_gcs: HashMap<u8, u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostClipRectangles {
    pub ordering: u8,
    pub x_origin: i16,
    pub y_origin: i16,
    pub rectangles: Vec<u8>,
}

/// Tracks what clip-state the host shared GC currently has, so we don't
/// re-issue identical `SetClipRectangles` / `ChangeGC(clip-mask)` calls.
#[derive(Clone, Debug, Eq, PartialEq)]
enum HostClipState {
    /// `clip-mask = None` — no clipping, draw everywhere.
    None,
    /// Clip to a list of rectangles set via `SetClipRectangles`.
    Rectangles(HostClipRectangles),
    /// Clip to the 1-bits of a depth-1 host pixmap, shifted by
    /// `(x_origin, y_origin)`. Used by wmaker for window-decoration
    /// symbols (close-button "X" etc.).
    Pixmap {
        host_pixmap: u32,
        x_origin: i16,
        y_origin: i16,
    },
}

pub type HostXidMap = Arc<Mutex<HashMap<u32, ResourceId>>>;

pub struct HostInputPump {
    read_stream: UnixStream,
    handle: HostInputPumpHandle,
}

#[derive(Clone)]
pub struct HostInputPumpHandle {
    write_stream: Arc<Mutex<UnixStream>>,
    xid_map: HostXidMap,
}

impl HostX11 {
    pub fn open_from_env() -> io::Result<Self> {
        let mut stream = connect_to_host()?;
        let setup = read_setup_reply(&mut stream)?;
        let window_id = setup.resource_id_base;
        let gc_id = setup.resource_id_base + 1;
        let font_id = setup.resource_id_base + 2;
        create_window(&mut stream, &setup, window_id)?;
        open_font(&mut stream, font_id, b"fixed")?;
        create_gc(
            &mut stream,
            window_id,
            gc_id,
            setup.black_pixel,
            setup.white_pixel,
            font_id,
        )?;
        map_window(&mut stream, window_id)?;
        stream.flush()?;

        let mut this = Self {
            stream,
            window_id,
            gc_id,
            current_foreground: setup.black_pixel,
            current_background: setup.white_pixel,
            current_clip: HostClipState::None,
            sequence: 5,
            next_xid: setup.resource_id_base + 3,
            render: None,
            xkb: None,
            shape_opcode: None,
            xfixes_opcode: None,
            composite_opcode: None,
            reply_buffer: Vec::new(),
            depth_gcs: HashMap::new(),
        };
        this.render = this.init_render().ok();
        this.xkb = this.init_xkb().ok();
        this.shape_opcode = this.query_extension_opcode(b"SHAPE").ok().flatten();
        if this.shape_opcode.is_none() {
            log::info!("host SHAPE extension absent — top-level shape forwarding disabled");
        }
        this.xfixes_opcode = this.query_extension_opcode(b"XFIXES").ok().flatten();
        if this.xfixes_opcode.is_none() {
            log::info!("host XFIXES extension absent — cursor-by-name forwarding disabled");
        }
        this.composite_opcode = this.query_extension_opcode(b"Composite").ok().flatten();
        if this.composite_opcode.is_none() {
            log::info!("host COMPOSITE extension absent — NameWindowPixmap will return BadAlloc");
        }
        Ok(this)
    }

    /// Major opcode of the host's COMPOSITE extension, or `None` if the
    /// host didn't advertise it at startup. The nested COMPOSITE handler
    /// uses this to gate `NameWindowPixmap` forwarding.
    #[must_use]
    pub fn composite_opcode(&self) -> Option<u8> {
        self.composite_opcode
    }

    /// Forward `Composite::NameWindowPixmap(window, pixmap)` to the host.
    /// Caller is responsible for validating `host_window` is a redirected
    /// host top-level and for allocating `host_pixmap` via `allocate_xid`.
    /// No reply is generated by the host.
    pub fn name_window_pixmap(&mut self, host_window: u32, host_pixmap: u32) -> io::Result<()> {
        let Some(major) = self.composite_opcode else {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "host COMPOSITE extension not available",
            ));
        };
        // Wire layout: opcode(1) minor(1) length(2 = 3) window(4) pixmap(4)
        let mut out = [0u8; 12];
        out[0] = major;
        out[1] = yserver_protocol::x11::composite::NAME_WINDOW_PIXMAP;
        out[2..4].copy_from_slice(&3u16.to_le_bytes());
        out[4..8].copy_from_slice(&host_window.to_le_bytes());
        out[8..12].copy_from_slice(&host_pixmap.to_le_bytes());
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.sequence = self.sequence.wrapping_add(1);
        Ok(())
    }

    pub fn allocate_xid(&mut self) -> u32 {
        let xid = self.next_xid;
        self.next_xid = self.next_xid.wrapping_add(1);
        xid
    }

    pub fn open_font(&mut self, name: &str) -> io::Result<(u32, FontMetrics)> {
        let host_xid = self.allocate_xid();
        let open_seq = self.sequence;
        write_open_font(&mut self.stream, host_xid, name.as_bytes())?;
        self.sequence = self.sequence.wrapping_add(1);
        let query_seq = self.sequence;
        write_query_font(&mut self.stream, host_xid)?;
        self.sequence = self.sequence.wrapping_add(1);
        self.stream.flush()?;

        // OpenFont yields no response on success and one error on failure.
        // QueryFont always yields exactly one response (reply or error).
        // Drain by sequence to keep the host stream aligned.
        let mut open_error: Option<u8> = None;
        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence == open_seq && resp.bytes[0] == 0 {
                open_error = Some(resp.bytes[1]);
                continue;
            }
            if resp.sequence == query_seq {
                if resp.bytes[0] == 0 {
                    let code = open_error.unwrap_or(resp.bytes[1]);
                    return Err(io::Error::other(format!(
                        "host OpenFont {name:?} failed (error {code})"
                    )));
                }
                if let Some(code) = open_error {
                    return Err(io::Error::other(format!(
                        "host OpenFont {name:?} failed (error {code})"
                    )));
                }
                let metrics = x11::parse_query_font_reply(&resp.bytes[8..]).ok_or_else(|| {
                    io::Error::new(
                        ErrorKind::InvalidData,
                        "could not parse host QueryFont reply",
                    )
                })?;
                return Ok((host_xid, metrics));
            }
        }
    }

    /// Send `GetKeyboardMapping` (op 101) to the host and return
    /// `(keysyms_per_keycode, keysyms_flat)` where the keysyms slice has
    /// `count * keysyms_per_keycode` u32 values.
    pub fn get_keyboard_mapping(
        &mut self,
        first_keycode: u8,
        count: u8,
    ) -> io::Result<(u8, Vec<u32>)> {
        let target = self.sequence;
        let mut out = [0u8; 8];
        out[0] = 101;
        out[2] = 2; // length in 4-byte units
        out[3] = 0;
        out[4] = first_keycode;
        out[5] = count;
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.sequence = self.sequence.wrapping_add(1);
        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence != target {
                continue;
            }
            if resp.bytes[0] == 0 {
                return Err(io::Error::other(format!(
                    "host GetKeyboardMapping failed (error {})",
                    resp.bytes[1]
                )));
            }
            let kpc = resp.bytes[1];
            // Body bytes start at offset 32 in the response.
            let n = usize::from(count) * usize::from(kpc);
            if resp.bytes.len() < 32 + n * 4 {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "GetKeyboardMapping reply truncated",
                ));
            }
            let mut keysyms = Vec::with_capacity(n);
            for i in 0..n {
                let base = 32 + i * 4;
                keysyms.push(u32::from_le_bytes([
                    resp.bytes[base],
                    resp.bytes[base + 1],
                    resp.bytes[base + 2],
                    resp.bytes[base + 3],
                ]));
            }
            return Ok((kpc, keysyms));
        }
    }

    /// Send `GetModifierMapping` (op 119) to the host and return
    /// `(keycodes_per_modifier, keycodes)` where keycodes has
    /// `8 * keycodes_per_modifier` bytes.
    pub fn get_modifier_mapping(&mut self) -> io::Result<(u8, Vec<u8>)> {
        let target = self.sequence;
        let out = [119u8, 0, 1, 0];
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.sequence = self.sequence.wrapping_add(1);
        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence != target {
                continue;
            }
            if resp.bytes[0] == 0 {
                return Err(io::Error::other(format!(
                    "host GetModifierMapping failed (error {})",
                    resp.bytes[1]
                )));
            }
            let kpm = resp.bytes[1];
            let n = 8 * usize::from(kpm);
            if resp.bytes.len() < 32 + n {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "GetModifierMapping reply truncated",
                ));
            }
            return Ok((kpm, resp.bytes[32..32 + n].to_vec()));
        }
    }

    pub fn close_font(&mut self, host_xid: u32) -> io::Result<()> {
        let mut out = Vec::new();
        out.push(46);
        out.push(0);
        write_u16(&mut out, 2);
        write_u32(&mut out, host_xid);
        self.sequence = self.sequence.wrapping_add(1);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// Send a `ListFonts` request to the host and return the full reply
    /// bytes (including the 32-byte standard reply header).
    pub fn list_fonts_proxy(&mut self, max_names: u16, pattern: &str) -> io::Result<Vec<u8>> {
        let target = self.sequence;
        write_list_fonts(&mut self.stream, 49, max_names, pattern.as_bytes())?;
        self.sequence = self.sequence.wrapping_add(1);
        self.stream.flush()?;
        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence == target {
                return Ok(resp.bytes);
            }
        }
    }

    /// Send a `ListFontsWithInfo` request and return all replies (one per
    /// match plus the trailing sentinel reply with name length 0).
    pub fn list_fonts_with_info_proxy(
        &mut self,
        max_names: u16,
        pattern: &str,
    ) -> io::Result<Vec<Vec<u8>>> {
        let target = self.sequence;
        write_list_fonts(&mut self.stream, 50, max_names, pattern.as_bytes())?;
        self.sequence = self.sequence.wrapping_add(1);
        self.stream.flush()?;

        let mut replies = Vec::new();
        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence != target {
                continue;
            }
            let name_len = resp.bytes[1];
            replies.push(resp.bytes);
            if name_len == 0 || replies.last().is_some_and(|r| r[0] == 0) {
                return Ok(replies);
            }
        }
    }

    pub fn window_id(&self) -> u32 {
        self.window_id
    }

    pub fn render_opcode(&self) -> Option<u8> {
        self.render.as_ref().map(|r| r.opcode)
    }

    pub fn xkb_opcode(&self) -> Option<u8> {
        self.xkb.as_ref().map(|r| r.opcode)
    }

    pub fn xkb_info(&self) -> Option<(u8, u8, u8)> {
        self.xkb
            .as_ref()
            .map(|r| (r.opcode, r.first_event, r.first_error))
    }

    pub fn render_format_for_ynest_id(&self, ynest_fmt: u32) -> Option<u32> {
        let r = self.render.as_ref()?;
        match ynest_fmt {
            1 => Some(r.fmt_a1),
            2 => Some(r.fmt_a8),
            3 => Some(r.fmt_rgb24),
            4 => Some(r.fmt_argb32),
            _ => None,
        }
    }

    fn init_render(&mut self) -> io::Result<HostRenderInfo> {
        let ext_name = b"RENDER";
        let padded = padded_len(ext_name.len());
        let length_units = 2 + (padded / 4) as u16;
        let ext_seq = self.sequence; // use current BEFORE increment (matches open_font pattern)
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(98u8);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u16(&mut out, ext_name.len() as u16);
        write_u16(&mut out, 0);
        out.extend_from_slice(ext_name);
        out.resize(8 + padded, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        debug!(
            "init_render: sent QueryExtension RENDER, expecting seq={}",
            ext_seq
        );

        let opcode;
        loop {
            let resp = read_response(&mut self.stream)?;
            debug!(
                "init_render: got response byte0={} seq={}",
                resp.bytes[0], resp.sequence
            );
            if resp.sequence == ext_seq {
                if resp.bytes[8] == 0 {
                    return Err(io::Error::other("host RENDER extension not present"));
                }
                opcode = resp.bytes[9];
                debug!("init_render: RENDER present, opcode={}", opcode);
                break;
            }
        }

        let fmt_seq = self.sequence; // use current BEFORE increment
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(1); // QueryPictFormats
        write_u16(&mut out, 1);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        debug!(
            "init_render: sent QueryPictFormats, expecting seq={}",
            fmt_seq
        );

        loop {
            let resp = read_response(&mut self.stream)?;
            debug!(
                "init_render: got response byte0={} seq={}",
                resp.bytes[0], resp.sequence
            );
            if resp.sequence == fmt_seq {
                let info = parse_host_pict_formats(&resp.bytes, opcode)?;
                debug!(
                    "init_render: host formats a1=0x{:x} a8=0x{:x} rgb24=0x{:x} argb32=0x{:x}",
                    info.fmt_a1, info.fmt_a8, info.fmt_rgb24, info.fmt_argb32
                );
                return Ok(info);
            }
        }
    }

    fn init_xkb(&mut self) -> io::Result<HostXkbInfo> {
        let ext_name = b"XKEYBOARD";
        let padded = padded_len(ext_name.len());
        let length_units = 2 + (padded / 4) as u16;
        let ext_seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(98u8);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u16(&mut out, ext_name.len() as u16);
        write_u16(&mut out, 0);
        out.extend_from_slice(ext_name);
        out.resize(8 + padded, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()?;

        let (opcode, first_event, first_error);
        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence == ext_seq {
                if resp.bytes[8] == 0 {
                    return Err(io::Error::other("host XKEYBOARD extension not present"));
                }
                opcode = resp.bytes[9];
                first_event = resp.bytes[10];
                first_error = resp.bytes[11];
                break;
            }
        }

        // We also need to send UseExtension to the host for XKB to be fully functional.
        let use_seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(0); // UseExtension
        write_u16(&mut out, 2);
        write_u16(&mut out, 1); // want major 1
        write_u16(&mut out, 0); // want minor 0
        self.stream.write_all(&out)?;
        self.stream.flush()?;

        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence == use_seq {
                // byte 8 is supported (bool)
                if resp.bytes[8] == 0 {
                    return Err(io::Error::other("host XKB UseExtension failed"));
                }
                break;
            }
        }

        Ok(HostXkbInfo {
            opcode,
            first_event,
            first_error,
        })
    }

    /// Issue `QueryExtension(name)` on the host stream and return the major
    /// opcode if the extension is present. Used for capability probes that
    /// don't need the first-event/first-error fields (`init_render` and
    /// `init_xkb` cache those for their own bookkeeping).
    fn query_extension_opcode(&mut self, name: &[u8]) -> io::Result<Option<u8>> {
        let padded = padded_len(name.len());
        let length_units = 2 + (padded / 4) as u16;
        let ext_seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(98u8); // QueryExtension
        out.push(0);
        write_u16(&mut out, length_units);
        write_u16(&mut out, name.len() as u16);
        write_u16(&mut out, 0);
        out.extend_from_slice(name);
        out.resize(8 + padded, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence == ext_seq {
                if resp.bytes[8] == 0 {
                    return Ok(None);
                }
                return Ok(Some(resp.bytes[9]));
            }
            self.reply_buffer.push(resp);
        }
    }

    /// Mirror the resolved bounding/clip rectangles for a top-level subwindow
    /// to the host's SHAPE extension. The local `ServerState::shape_windows`
    /// remains the source of truth for protocol-visible queries (`QueryExtents`,
    /// `GetRectangles`, `InputSelected`); this call exists so the host actually
    /// renders themed WM frames with the right silhouette.
    ///
    /// `kind` follows the SHAPE protocol enum: 0 = Bounding, 1 = Clip.
    /// No-op when the host doesn't advertise SHAPE.
    pub fn set_shape_rectangles(
        &mut self,
        host_xid: u32,
        kind: u8,
        rects: &[yserver_protocol::x11::xfixes::RegionRect],
    ) -> io::Result<()> {
        let Some(bytes) = build_shape_rectangles(self.shape_opcode, host_xid, kind, rects) else {
            return Ok(());
        };
        self.sequence = self.sequence.wrapping_add(1);
        self.stream.write_all(&bytes)?;
        self.stream.flush()
    }

    /// Forward XFIXES `ChangeCursorByName` (minor 23) to the host. The host
    /// resolves the cursor name against its own theme, so we pass through
    /// `name_bytes` verbatim. No-op when the host doesn't advertise XFIXES.
    pub fn xfixes_change_cursor_by_name(
        &mut self,
        host_cursor_xid: u32,
        name_bytes: &[u8],
    ) -> io::Result<()> {
        let Some(bytes) =
            build_xfixes_change_cursor_by_name(self.xfixes_opcode, host_cursor_xid, name_bytes)
        else {
            return Ok(());
        };
        self.sequence = self.sequence.wrapping_add(1);
        self.stream.write_all(&bytes)?;
        self.stream.flush()
    }

    pub fn render_create_picture(
        &mut self,
        host_pic: u32,
        host_drawable: u32,
        ynest_format: u32,
        value_mask: u32,
        values: &[u8],
    ) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let host_fmt = if ynest_format == 0 {
            0u32
        } else {
            match ynest_format {
                1 => r.fmt_a1,
                2 => r.fmt_a8,
                3 => r.fmt_rgb24,
                4 => r.fmt_argb32,
                _ => 0,
            }
        };
        if host_fmt == 0 && ynest_format != 0 {
            return Ok(());
        }
        let opcode = r.opcode;
        let nvals = (values.len() / 4) as u16;
        // header(4) + pid(4) + drawable(4) + format(4) + value-mask(4) = 20 bytes = 5 units
        let length_units = 5 + nvals;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(4); // CreatePicture
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_pic);
        write_u32(&mut out, host_drawable);
        write_u32(&mut out, host_fmt);
        write_u32(&mut out, value_mask);
        out.extend_from_slice(values);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_composite(
        &mut self,
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
    ) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        // header(4) + op_pad(4) + src(4) + mask(4) + dst(4) + src_xy(4)
        // + mask_xy(4) + dst_xy(4) + size(4) = 36 bytes = 9 units
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::with_capacity(36);
        out.push(opcode);
        out.push(8); // Composite
        write_u16(&mut out, 9);
        out.push(op);
        out.extend_from_slice(&[0, 0, 0]); // pad
        write_u32(&mut out, host_src);
        write_u32(&mut out, host_mask);
        write_u32(&mut out, host_dst);
        write_i16(&mut out, src_x);
        write_i16(&mut out, src_y);
        write_i16(&mut out, mask_x);
        write_i16(&mut out, mask_y);
        write_i16(&mut out, dst_x);
        write_i16(&mut out, dst_y);
        write_u16(&mut out, width);
        write_u16(&mut out, height);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn render_free_picture(&mut self, host_pic: u32) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(7); // FreePicture
        write_u16(&mut out, 2);
        write_u32(&mut out, host_pic);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn render_create_glyphset(&mut self, host_gs: u32, ynest_format: u32) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let host_fmt = match ynest_format {
            1 => r.fmt_a1,
            2 => r.fmt_a8,
            3 => r.fmt_rgb24,
            4 => r.fmt_argb32,
            _ => r.fmt_a8,
        };
        let opcode = r.opcode;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(17); // CreateGlyphSet
        write_u16(&mut out, 3);
        write_u32(&mut out, host_gs);
        write_u32(&mut out, host_fmt);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn render_free_glyphset(&mut self, host_gs: u32) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(19); // FreeGlyphSet
        write_u16(&mut out, 2);
        write_u32(&mut out, host_gs);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn render_add_glyphs(&mut self, host_gs: u32, body_tail: &[u8]) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        // body_tail (already padded on the wire): num_glyphs(4) + glyph_ids + glyph_infos + glyph_data
        let padded_tail = padded_len(body_tail.len());
        let length_units = 2 + (padded_tail / 4) as u16;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(20); // AddGlyphs
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_gs);
        out.extend_from_slice(body_tail);
        out.resize(8 + padded_tail, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn render_free_glyphs(&mut self, host_gs: u32, glyph_ids: &[u8]) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        let padded_ids = padded_len(glyph_ids.len());
        let length_units = 2 + (padded_ids / 4) as u16;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(22); // FreeGlyphs
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_gs);
        out.extend_from_slice(glyph_ids);
        out.resize(8 + padded_ids, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_composite_glyphs(
        &mut self,
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
    ) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        let id_size = match minor {
            23 => 1, // CompositeGlyphs8
            24 => 2, // CompositeGlyphs16
            25 => 4, // CompositeGlyphs32
            _ => 1,  // shouldn't happen, fall back to 8-bit stride
        };
        let mut patched = items.to_vec();
        patch_glyph_command_offsets(&mut patched, x_off, y_off, id_size);
        let padded_items = padded_len(patched.len());
        let length_units = 7 + (padded_items / 4) as u16;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(minor);
        write_u16(&mut out, length_units);
        out.push(op);
        out.extend_from_slice(&[0, 0, 0]); // pad
        write_u32(&mut out, host_src);
        write_u32(&mut out, host_dst);
        write_u32(&mut out, mask_fmt);
        write_u32(&mut out, host_gs);
        write_i16(&mut out, src_x);
        write_i16(&mut out, src_y);
        out.extend_from_slice(&patched);
        out.resize(28 + padded_items, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// Forward a RENDER request whose first body word is a picture XID.
    /// `minor` is the RENDER sub-opcode; `body` is everything after the 4-byte
    /// request header.  The first 4 bytes of `body` are replaced with
    /// `host_pic` before forwarding.
    fn render_forward_picture_op(
        &mut self,
        minor: u8,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        let total = 4 + body.len();
        let length_units =
            u16::try_from(total / 4).map_err(|_| io::Error::other("RENDER request too large"))?;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::with_capacity(total);
        out.push(opcode);
        out.push(minor);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_pic); // overwrite client picture XID
        if body.len() > 4 {
            out.extend_from_slice(&body[4..]);
        }
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// RENDER::SetPictureTransform (minor=28): forward transform matrix to host.
    pub fn render_set_picture_transform(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()> {
        self.render_forward_picture_op(28, host_pic, body)
    }

    /// RENDER::SetPictureFilter (minor=30): forward filter name + values to host.
    pub fn render_set_picture_filter(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()> {
        self.render_forward_picture_op(30, host_pic, body)
    }

    /// RENDER::CreateLinearGradient (minor=34): create gradient picture on host.
    /// `host_pic` is a fresh XID already allocated by the caller.
    /// `body` is the client body after the picture XID field (p1 x/y, p2 x/y,
    /// num_stops, offsets[], colors[]).
    pub fn render_create_linear_gradient(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()> {
        self.render_forward_picture_op(34, host_pic, body)
    }

    pub fn render_create_radial_gradient(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()> {
        self.render_forward_picture_op(35, host_pic, body)
    }

    pub fn render_change_picture(&mut self, host_pic: u32, body: &[u8]) -> io::Result<()> {
        self.render_forward_picture_op(5, host_pic, body)
    }

    pub fn render_set_picture_clip_rectangles(
        &mut self,
        host_pic: u32,
        body: &[u8],
    ) -> io::Result<()> {
        self.render_forward_picture_op(6, host_pic, body)
    }

    pub fn render_create_solid_fill(&mut self, host_pic: u32, color: [u8; 8]) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(33); // CreateSolidFill
        write_u16(&mut out, 4);
        write_u32(&mut out, host_pic);
        out.extend_from_slice(&color);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn render_fill_rectangles(
        &mut self,
        host_dst: u32,
        op: u8,
        color: [u8; 8],
        rects: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        let nrects = rects.len() / 8;
        // header(4) + op_pad(4) + dst(4) + color(8) = 20 bytes = 5 units, plus 2 units per rect
        let length_units = 5 + (nrects * 2) as u16;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(26); // FillRectangles
        write_u16(&mut out, length_units);
        out.push(op);
        out.extend_from_slice(&[0, 0, 0]); // pad
        write_u32(&mut out, host_dst);
        out.extend_from_slice(&color);
        // Translate rectangles
        for rect in rects.chunks_exact(8) {
            let x = i16::from_le_bytes([rect[0], rect[1]]).wrapping_add(x_off);
            let y = i16::from_le_bytes([rect[2], rect[3]]).wrapping_add(y_off);
            let w = u16::from_le_bytes([rect[4], rect[5]]);
            let h = u16::from_le_bytes([rect[6], rect[7]]);
            write_i16(&mut out, x);
            write_i16(&mut out, y);
            write_u16(&mut out, w);
            write_u16(&mut out, h);
        }
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// Forward RENDER::Trapezoids (minor=10) verbatim to the host.
    /// Per-trapezoid FIXED-point coords have `x_off`/`y_off` (in pixels)
    /// added so child-window pictures land at the right offset on the
    /// shared host top-level. For top-level pictures the offsets are 0
    /// and this is a straight passthrough.
    pub fn render_trapezoids(
        &mut self,
        op: u8,
        host_src: u32,
        host_dst: u32,
        host_mask_format: u32,
        src_x: i16,
        src_y: i16,
        traps: &[u8],
        x_off: i16,
        y_off: i16,
    ) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        let n_traps = traps.len() / 40;
        // header(4) + op_pad(4) + src(4) + dst(4) + mask_format(4)
        // + src_xy(4) = 24 bytes = 6 units, plus 10 units (40 bytes) per trap
        let length_units = 6u16 + (n_traps as u16) * 10;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::with_capacity(24 + n_traps * 40);
        out.push(opcode);
        out.push(10); // Trapezoids
        write_u16(&mut out, length_units);
        out.push(op);
        out.extend_from_slice(&[0, 0, 0]); // pad
        write_u32(&mut out, host_src);
        write_u32(&mut out, host_dst);
        write_u32(&mut out, host_mask_format);
        write_i16(&mut out, src_x);
        write_i16(&mut out, src_y);

        // FIXED is i32 with 16 bits of fraction; integer offset is
        // shifted left by 16 before adding.
        let dx = (i32::from(x_off)) << 16;
        let dy = (i32::from(y_off)) << 16;
        for trap in traps.chunks_exact(40) {
            // Y-coords at offsets 0, 4, 12, 20, 28, 36
            // X-coords at offsets 8, 16, 24, 32
            let mut t = [0u8; 40];
            t.copy_from_slice(trap);
            let patch = |t: &mut [u8; 40], off: usize, delta: i32| {
                let v = i32::from_le_bytes([t[off], t[off + 1], t[off + 2], t[off + 3]])
                    .wrapping_add(delta);
                t[off..off + 4].copy_from_slice(&v.to_le_bytes());
            };
            for &y_off_in_trap in &[0usize, 4, 12, 20, 28, 36] {
                patch(&mut t, y_off_in_trap, dy);
            }
            for &x_off_in_trap in &[8usize, 16, 24, 32] {
                patch(&mut t, x_off_in_trap, dx);
            }
            out.extend_from_slice(&t);
        }
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn render_query_version(&mut self) -> io::Result<(u32, u32)> {
        let Some(r) = self.render.as_ref() else {
            return Ok((0, 0));
        };
        let opcode = r.opcode;
        let seq = self.sequence; // use current BEFORE increment
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(0); // QueryVersion
        write_u16(&mut out, 3);
        write_u32(&mut out, 0); // client major
        write_u32(&mut out, 11); // client minor
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence == seq {
                let major = read_u32(&resp.bytes[8..12]);
                let minor = read_u32(&resp.bytes[12..16]);
                return Ok((major, minor));
            }
        }
    }

    pub fn xkb_proxy(&mut self, minor: u8, body: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let Some(xkb) = self.xkb.as_ref() else {
            return Err(io::Error::other("XKB not available on host"));
        };
        let target = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);

        // Standard X11 request: major(1) minor(1) length(2)
        let total_len = body.len() + 4;
        let length_units = u16::try_from(total_len / 4)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "XKB request too large"))?;

        let mut out = Vec::with_capacity(total_len);
        out.push(xkb.opcode);
        out.push(minor);
        write_u16(&mut out, length_units);
        out.extend_from_slice(body);
        self.stream.write_all(&out)?;
        self.stream.flush()?;

        if !xkb_minor_has_reply(minor) {
            return Ok(None);
        }

        if let Some(pos) = self.reply_buffer.iter().position(|r| r.sequence == target) {
            return Ok(Some(self.reply_buffer.remove(pos).bytes));
        }

        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence == target {
                return Ok(Some(resp.bytes));
            }
            self.reply_buffer.push(resp);
        }
    }

    pub fn ping(&mut self) -> io::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        self.stream.write_all(&[127, 0, 1, 0])
    }

    pub fn query_pointer(&mut self) -> io::Result<PointerPosition> {
        self.sequence = self.sequence.wrapping_add(1);

        let mut out = Vec::new();
        out.push(38);
        out.push(0);
        write_u16(&mut out, 2);
        write_u32(&mut out, self.window_id);
        self.stream.write_all(&out)?;
        self.stream.flush()?;

        let mut reply = [0; 32];
        self.read_fixed_reply(&mut reply)?;
        if reply[0] != 1 {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                format!("expected QueryPointer reply, got response {}", reply[0]),
            ));
        }

        Ok(PointerPosition {
            same_screen: reply[1] != 0,
            win_x: read_i16(&reply[20..22]),
            win_y: read_i16(&reply[22..24]),
            mask: read_u16(&reply[24..26]),
        })
    }

    pub fn create_subwindow(
        &mut self,
        host_xid: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        // 1. CreateWindow request — parent is the container (self.window_id).
        let cw_seq = self.sequence;
        let mut out = Vec::new();
        // Top-level subwindow attributes:
        // - bit-gravity = NorthWest (1<<4): preserve pixels on a resize.
        // - backing-store = Always (1<<6, value 2): the host preserves the
        //   subwindow's pixel content even when occluded or its parent is
        //   resized in ways that would otherwise discard it. Without this
        //   the host can drop pixels when the container is resized and
        //   apps appear blank until the next Expose-driven redraw.
        let value_mask: u32 = (1 << 4) | (1 << 6);
        out.push(1); // CreateWindow opcode
        out.push(0); // depth = CopyFromParent
        write_u16(&mut out, 10); // length: 8 fixed + 2 values = 10 units
        write_u32(&mut out, host_xid);
        write_u32(&mut out, self.window_id); // parent = container
        write_i16(&mut out, x);
        write_i16(&mut out, y);
        let safe_width = width.max(1);
        let safe_height = height.max(1);
        write_u16(&mut out, safe_width);
        write_u16(&mut out, safe_height);
        write_u16(&mut out, 0); // border_width
        write_u16(&mut out, 0); // class = CopyFromParent
        write_u32(&mut out, 0); // visual = CopyFromParent
        write_u32(&mut out, value_mask);
        write_u32(&mut out, 1); // bit-gravity = NorthWest
        write_u32(&mut out, 2); // backing-store = Always
        self.stream.write_all(&out)?;
        self.sequence = self.sequence.wrapping_add(1);

        // 2. GetInputFocus round-trip — always returns a reply, ensuring
        //    CreateWindow has been committed by the host before we return.
        //    (GetGeometry can fail with BadDrawable if CreateWindow failed,
        //    and the error response is identical in layout to a real answer.)
        let sync_seq = self.sequence;
        let mut sync = Vec::new();
        sync.push(43); // GetInputFocus opcode
        sync.push(0);
        write_u16(&mut sync, 1);
        self.stream.write_all(&sync)?;
        self.sequence = self.sequence.wrapping_add(1);
        self.stream.flush()?;

        log::debug!(
            "create_subwindow: host_xid=0x{:x} cw_seq={} sync_seq={} buf_len={}",
            host_xid,
            cw_seq,
            sync_seq,
            self.reply_buffer.len()
        );

        // Drain replies/errors until we see the GetInputFocus reply at sync_seq.
        // Check buffered responses first — a previous call's drain loop may have
        // read past sync_seq and saved the reply here.
        if let Some(pos) = self
            .reply_buffer
            .iter()
            .position(|r| r.sequence == sync_seq)
        {
            log::debug!("create_subwindow: found in buffer at pos={}", pos);
            self.reply_buffer.remove(pos);
            return Ok(());
        }
        loop {
            let resp = read_response(&mut self.stream)?;
            let detail = if resp.bytes[0] == 0 {
                format!("err_code={}", resp.bytes[1])
            } else {
                String::new()
            };
            log::debug!(
                "create_subwindow: stream resp type={} seq={} {} (want {})",
                resp.bytes[0],
                resp.sequence,
                detail,
                sync_seq
            );
            if resp.sequence == sync_seq {
                return Ok(());
            }
            self.reply_buffer.push(resp);
        }
    }

    pub fn destroy_subwindow(&mut self, host_xid: u32) -> io::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(4); // DestroyWindow
        out.push(0);
        write_u16(&mut out, 2);
        write_u32(&mut out, host_xid);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    #[allow(
        dead_code,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::cast_lossless
    )]
    // Sign-extension of i16 → u32 is intentional per X11 wire format (INT16
    // in a CARD32 slot must be sign-extended).
    pub fn configure_subwindow(
        &mut self,
        host_xid: u32,
        config: HostSubwindowConfig,
    ) -> io::Result<()> {
        let mut value_mask: u16 = 0;
        let mut values: Vec<u8> = Vec::new();
        if let Some(x) = config.x {
            value_mask |= 1 << 0;
            write_u32(&mut values, x as i32 as u32);
        }
        if let Some(y) = config.y {
            value_mask |= 1 << 1;
            write_u32(&mut values, y as i32 as u32);
        }
        if let Some(width) = config.width {
            value_mask |= 1 << 2;
            write_u32(&mut values, u32::from(width.max(1)));
        }
        if let Some(height) = config.height {
            value_mask |= 1 << 3;
            write_u32(&mut values, u32::from(height.max(1)));
        }
        if let Some(sibling) = config.sibling {
            value_mask |= 1 << 5;
            write_u32(&mut values, sibling);
        }
        if let Some(stack_mode) = config.stack_mode {
            value_mask |= 1 << 6;
            write_u32(&mut values, u32::from(stack_mode));
        }
        if value_mask == 0 {
            return Ok(());
        }

        let length_units = 3 + u16::try_from(values.len() / 4).map_err(|_| {
            io::Error::new(ErrorKind::InvalidInput, "too many ConfigureWindow values")
        })?;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(12); // ConfigureWindow
        out.push(0);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u16(&mut out, value_mask);
        write_u16(&mut out, 0); // pad
        out.extend_from_slice(&values);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// Set the host container window's background pixmap so the host
    /// server auto-fills regions uncovered when nested top-levels move,
    /// without us needing to plumb Expose events through the nested root.
    /// Pass `None` (or 0) for `None` (no bg pixmap → window stays its
    /// previous content).
    pub fn set_container_background_pixmap(&mut self, host_pixmap_xid: u32) -> io::Result<()> {
        // ChangeWindowAttributes (opcode 2): window(4) value-mask(4) values(4*n)
        // value-mask CWBackPixmap = 0x00000001
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::with_capacity(16);
        out.push(2);
        out.push(0);
        write_u16(&mut out, 4); // length 4 = 16 bytes
        write_u32(&mut out, self.window_id);
        write_u32(&mut out, 0x0000_0001); // CWBackPixmap
        write_u32(&mut out, host_pixmap_xid);
        self.stream.write_all(&out)?;

        // Force the host to repaint immediately so the new bg shows up
        // even if nothing has triggered an Expose yet. ClearArea(window,
        // 0,0,0,0, false) clears the entire window.
        self.sequence = self.sequence.wrapping_add(1);
        let mut clear = Vec::with_capacity(16);
        clear.push(61); // ClearArea
        clear.push(0); // exposures = false
        write_u16(&mut clear, 4); // length 4 = 16 bytes
        write_u32(&mut clear, self.window_id);
        write_i16(&mut clear, 0);
        write_i16(&mut clear, 0);
        write_u16(&mut clear, 0);
        write_u16(&mut clear, 0);
        self.stream.write_all(&clear)?;
        self.stream.flush()
    }

    pub fn set_container_background_pixel(&mut self, pixel: u32) -> io::Result<()> {
        // ChangeWindowAttributes with value-mask CWBackPixel = 0x00000002.
        // Setting bg-pixel makes the host auto-clear the container with a
        // solid color whenever a region is exposed (e.g. a top-level
        // subwindow is moved across it). Without this, drags leave trails of
        // stale window content on the desktop.
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::with_capacity(16);
        out.push(2);
        out.push(0);
        write_u16(&mut out, 4);
        write_u32(&mut out, self.window_id);
        write_u32(&mut out, 0x0000_0002); // CWBackPixel
        write_u32(&mut out, pixel);
        self.stream.write_all(&out)?;

        // ClearArea so the new color is visible immediately.
        self.sequence = self.sequence.wrapping_add(1);
        let mut clear = Vec::with_capacity(16);
        clear.push(61);
        clear.push(0);
        write_u16(&mut clear, 4);
        write_u32(&mut clear, self.window_id);
        write_i16(&mut clear, 0);
        write_i16(&mut clear, 0);
        write_u16(&mut clear, 0);
        write_u16(&mut clear, 0);
        self.stream.write_all(&clear)?;
        self.stream.flush()
    }

    pub fn map_subwindow(&mut self, host_xid: u32) -> io::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(8); // MapWindow
        out.push(0);
        write_u16(&mut out, 2);
        write_u32(&mut out, host_xid);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn unmap_subwindow(&mut self, host_xid: u32) -> io::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(10); // UnmapWindow
        out.push(0);
        write_u16(&mut out, 2);
        write_u32(&mut out, host_xid);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn warp_pointer(&mut self, dst_host_xid: u32, dst_x: i16, dst_y: i16) -> io::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(41); // WarpPointer
        out.push(0);
        write_u16(&mut out, 6); // length: 6 units = 24 bytes
        write_u32(&mut out, 0); // src_window = None
        write_u32(&mut out, dst_host_xid);
        write_i16(&mut out, 0); // src_x
        write_i16(&mut out, 0); // src_y
        write_u16(&mut out, 0); // src_width
        write_u16(&mut out, 0); // src_height
        write_i16(&mut out, dst_x);
        write_i16(&mut out, dst_y);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    /// Forward `GetAtomName(atom)` to the host and return its name, or
    /// `Ok(None)` if the host doesn't know the atom (returns `BadAtom`).
    /// Used to resolve atoms that leak in via host-proxied replies — most
    /// notably the `FONTPROP` atoms in `ListFontsWithInfo` payloads.
    pub fn get_atom_name(&mut self, atom: u32) -> io::Result<Option<String>> {
        let target = self.sequence;
        let mut out = Vec::new();
        out.push(17u8); // GetAtomName
        out.push(0);
        write_u16(&mut out, 2); // length: 2 units = 8 bytes
        write_u32(&mut out, atom);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.sequence = self.sequence.wrapping_add(1);
        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence != target {
                self.reply_buffer.push(resp);
                continue;
            }
            if resp.bytes[0] == 0 {
                // BadAtom (or other) error — host doesn't know this atom.
                return Ok(None);
            }
            let name_len = u16::from_le_bytes([resp.bytes[8], resp.bytes[9]]) as usize;
            let name_start = 32usize;
            let end = name_start.checked_add(name_len).ok_or_else(|| {
                io::Error::new(ErrorKind::InvalidData, "GetAtomName name length overflow")
            })?;
            if resp.bytes.len() < end {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "GetAtomName reply truncated",
                ));
            }
            let name = String::from_utf8_lossy(&resp.bytes[name_start..end]).into_owned();
            return Ok(Some(name));
        }
    }

    pub fn create_pixmap(
        &mut self,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(53); // CreatePixmap opcode
        out.push(depth);
        write_u16(&mut out, 4); // length: 4 units = 16 bytes
        write_u32(&mut out, host_xid);
        write_u32(&mut out, self.window_id); // screen-compatible drawable
        write_u16(&mut out, width);
        write_u16(&mut out, height);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn free_pixmap(&mut self, host_xid: u32) -> io::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(54); // FreePixmap opcode
        out.push(0);
        write_u16(&mut out, 2);
        write_u32(&mut out, host_xid);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn create_cursor(
        &mut self,
        source_pixmap_xid: u32,
        mask_pixmap_xid: u32,
        fore: (u16, u16, u16),
        back: (u16, u16, u16),
        hot_x: u16,
        hot_y: u16,
    ) -> io::Result<u32> {
        let cursor_xid = self.allocate_xid();
        self.sequence = self.sequence.wrapping_add(1);
        let mut buf = Vec::with_capacity(32);
        buf.push(93u8);
        buf.push(0u8);
        write_u16(&mut buf, 8u16);
        write_u32(&mut buf, cursor_xid);
        write_u32(&mut buf, source_pixmap_xid);
        write_u32(&mut buf, mask_pixmap_xid);
        write_u16(&mut buf, fore.0);
        write_u16(&mut buf, fore.1);
        write_u16(&mut buf, fore.2);
        write_u16(&mut buf, back.0);
        write_u16(&mut buf, back.1);
        write_u16(&mut buf, back.2);
        write_u16(&mut buf, hot_x);
        write_u16(&mut buf, hot_y);
        self.stream.write_all(&buf)?;
        self.stream.flush()?;
        Ok(cursor_xid)
    }

    pub fn render_create_cursor(
        &mut self,
        cursor_xid: u32,
        host_src_pic: u32,
        x: u16,
        y: u16,
    ) -> io::Result<()> {
        let Some(r) = self.render.as_ref() else {
            return Ok(());
        };
        let opcode = r.opcode;
        self.sequence = self.sequence.wrapping_add(1);
        let mut buf = Vec::with_capacity(16);
        buf.push(opcode);
        buf.push(27); // CreateCursor minor opcode
        write_u16(&mut buf, 4u16);
        write_u32(&mut buf, cursor_xid);
        write_u32(&mut buf, host_src_pic);
        write_u16(&mut buf, x);
        write_u16(&mut buf, y);
        self.stream.write_all(&buf)?;
        self.stream.flush()
    }

    pub fn define_cursor(&mut self, host_window_xid: u32, cursor_host_xid: u32) -> io::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        let mut buf = Vec::with_capacity(12);
        buf.push(43u8);
        buf.push(0u8);
        write_u16(&mut buf, 3u16);
        write_u32(&mut buf, host_window_xid);
        write_u32(&mut buf, cursor_host_xid);
        self.stream.write_all(&buf)?;
        self.stream.flush()
    }

    pub fn set_clip_rectangles(
        &mut self,
        clip: Option<ClipRectangles>,
        x_offset: i16,
        y_offset: i16,
    ) -> io::Result<()> {
        let new_state = match clip {
            Some(clip) => HostClipState::Rectangles(HostClipRectangles {
                ordering: clip.ordering,
                x_origin: clip.x_origin.wrapping_add(x_offset),
                y_origin: clip.y_origin.wrapping_add(y_offset),
                rectangles: clip.rectangles,
            }),
            None => HostClipState::None,
        };
        self.set_host_clip(new_state)
    }

    pub fn clear_clip_rectangles(&mut self) -> io::Result<()> {
        self.set_host_clip(HostClipState::None)
    }

    /// Set the host shared GC's `clip-mask` to a host pixmap, shifted
    /// by `(x_offset + clip_x_origin, y_offset + clip_y_origin)`. Used
    /// when forwarding a client `ChangeGC(clip_mask=Pixmap)` so the
    /// host clips drawing to the 1-bits of the depth-1 mask pixmap.
    pub fn set_clip_pixmap(
        &mut self,
        host_pixmap: u32,
        clip_x_origin: i16,
        clip_y_origin: i16,
        x_offset: i16,
        y_offset: i16,
    ) -> io::Result<()> {
        self.set_host_clip(HostClipState::Pixmap {
            host_pixmap,
            x_origin: clip_x_origin.wrapping_add(x_offset),
            y_origin: clip_y_origin.wrapping_add(y_offset),
        })
    }

    fn set_host_clip(&mut self, new_state: HostClipState) -> io::Result<()> {
        if self.current_clip == new_state {
            return Ok(());
        }

        match &new_state {
            HostClipState::Rectangles(clip) => {
                let length_units = 3 + u16::try_from(clip.rectangles.len() / 4).map_err(|_| {
                    io::Error::new(
                        ErrorKind::InvalidInput,
                        "too many clip rectangles for one X11 request",
                    )
                })?;
                self.sequence = self.sequence.wrapping_add(1);
                let mut out = Vec::new();
                out.push(59); // SetClipRectangles
                out.push(clip.ordering);
                write_u16(&mut out, length_units);
                write_u32(&mut out, self.gc_id);
                write_i16(&mut out, clip.x_origin);
                write_i16(&mut out, clip.y_origin);
                out.extend_from_slice(&clip.rectangles);
                self.stream.write_all(&out)?;
            }
            HostClipState::Pixmap {
                host_pixmap,
                x_origin,
                y_origin,
            } => {
                // Single ChangeGC with three components: clip_x_origin
                // (1<<17) + clip_y_origin (1<<18) + clip-mask (1<<19).
                self.sequence = self.sequence.wrapping_add(1);
                let mut out = Vec::new();
                out.push(56); // ChangeGC
                out.push(0);
                write_u16(&mut out, 6); // header(4) + mask(4) + 3 values(12) = 24 bytes = 6 units
                write_u32(&mut out, self.gc_id);
                write_u32(&mut out, (1 << 17) | (1 << 18) | (1 << 19));
                write_i16(&mut out, *x_origin);
                write_u16(&mut out, 0); // pad to 4 bytes
                write_i16(&mut out, *y_origin);
                write_u16(&mut out, 0); // pad
                write_u32(&mut out, *host_pixmap);
                self.stream.write_all(&out)?;
            }
            HostClipState::None => {
                self.sequence = self.sequence.wrapping_add(1);
                let mut out = Vec::new();
                out.push(56); // ChangeGC
                out.push(0);
                write_u16(&mut out, 4);
                write_u32(&mut out, self.gc_id);
                write_u32(&mut out, 1 << 19); // clip-mask
                write_u32(&mut out, 0); // None
                self.stream.write_all(&out)?;
            }
        }

        self.current_clip = new_state;
        self.stream.flush()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn copy_area(
        &mut self,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(62); // CopyArea opcode
        out.push(0);
        write_u16(&mut out, 7); // length: 7 units = 28 bytes
        write_u32(&mut out, src_host_xid);
        write_u32(&mut out, dst_host_xid);
        write_u32(&mut out, self.gc_id);
        write_i16(&mut out, src_x);
        write_i16(&mut out, src_y);
        write_i16(&mut out, dst_x);
        write_i16(&mut out, dst_y);
        write_u16(&mut out, width);
        write_u16(&mut out, height);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn copy_plane(
        &mut self,
        src_host_xid: u32,
        dst_host_xid: u32,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        plane: u32,
    ) -> io::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(63); // CopyPlane opcode
        out.push(0);
        write_u16(&mut out, 8); // length: 8 units = 32 bytes
        write_u32(&mut out, src_host_xid);
        write_u32(&mut out, dst_host_xid);
        write_u32(&mut out, self.gc_id);
        write_i16(&mut out, src_x);
        write_i16(&mut out, src_y);
        write_i16(&mut out, dst_x);
        write_i16(&mut out, dst_y);
        write_u16(&mut out, width);
        write_u16(&mut out, height);
        write_u32(&mut out, plane);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    #[allow(clippy::too_many_arguments)]
    /// Send GetImage to the host and return the raw reply bytes (32-byte header
    /// + image data).  Returns None on host error or if the region is invalid.
    pub fn get_image(
        &mut self,
        host_xid: u32,
        format: u8,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
        plane_mask: u32,
    ) -> io::Result<Option<Vec<u8>>> {
        let target_seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);

        let mut out = [0u8; 20];
        out[0] = 73; // GetImage
        out[1] = format;
        out[2] = 5;
        out[3] = 0; // request length = 5 words = 20 bytes
        out[4..8].copy_from_slice(&host_xid.to_le_bytes());
        out[8..10].copy_from_slice(&x.to_le_bytes());
        out[10..12].copy_from_slice(&y.to_le_bytes());
        out[12..14].copy_from_slice(&width.to_le_bytes());
        out[14..16].copy_from_slice(&height.to_le_bytes());
        out[16..20].copy_from_slice(&plane_mask.to_le_bytes());
        self.stream.write_all(&out)?;
        self.stream.flush()?;

        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence == target_seq {
                if resp.bytes[0] == 0 {
                    // host returned an error (e.g. BadMatch for out-of-bounds)
                    return Ok(None);
                }
                return Ok(Some(resp.bytes));
            }
        }
    }

    /// Returns a host GC bound to a drawable of the given depth, creating
    /// one on demand. The default `gc_id` is depth-24; pass that drawable
    /// (or any depth-24 drawable) for that case to reuse it.
    fn ensure_gc_for_depth(&mut self, depth: u8, drawable: u32) -> io::Result<u32> {
        if depth == 24 {
            return Ok(self.gc_id);
        }
        if let Some(&gc) = self.depth_gcs.get(&depth) {
            return Ok(gc);
        }
        let gc = self.allocate_xid();
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(55); // CreateGC opcode
        out.push(0);
        write_u16(&mut out, 4); // length units (no values)
        write_u32(&mut out, gc);
        write_u32(&mut out, drawable);
        write_u32(&mut out, 0); // value-mask = 0
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.depth_gcs.insert(depth, gc);
        Ok(gc)
    }

    pub fn put_image(
        &mut self,
        host_xid: u32,
        depth: u8,
        width: u16,
        height: u16,
        dst_x: i16,
        dst_y: i16,
        data: &[u8],
    ) -> io::Result<()> {
        // GCs are bound to a drawable's root and depth at creation; using the
        // default depth-24 GC against a depth-1/8/32 pixmap would BadMatch and
        // silently discard the image data on the host.
        let gc = self.ensure_gc_for_depth(depth, host_xid)?;
        if height == 0 || data.is_empty() {
            return Ok(());
        }
        // Standard PutImage has a 16-bit length field (in 4-byte units),
        // so max body bytes = 65535 * 4 - 24 = 262_116. e16's root
        // background can be 800x600 d24 = 1.9 MB which won't fit; chunk
        // by row count to stay under the limit.
        const MAX_PAYLOAD: usize = (u16::MAX as usize - 6) * 4;
        let stride = data.len() / usize::from(height);
        if stride == 0 {
            return Ok(());
        }
        let max_rows = (MAX_PAYLOAD / stride).max(1);
        let total_rows = usize::from(height);
        let mut row = 0usize;
        while row < total_rows {
            let rows = (total_rows - row).min(max_rows);
            let chunk = &data[row * stride..(row + rows) * stride];
            let padded_data_len = padded_len(chunk.len());
            let length_units = 6 + (padded_data_len / 4) as u16;
            let chunk_dst_y = dst_y.wrapping_add(row as i16);
            let chunk_height = rows as u16;
            self.sequence = self.sequence.wrapping_add(1);
            let mut out = Vec::with_capacity(24 + padded_data_len);
            out.push(72); // PutImage opcode
            out.push(2); // ZPixmap format
            write_u16(&mut out, length_units);
            write_u32(&mut out, host_xid);
            write_u32(&mut out, gc);
            write_u16(&mut out, width);
            write_u16(&mut out, chunk_height);
            write_i16(&mut out, dst_x);
            write_i16(&mut out, chunk_dst_y);
            out.push(0); // left-pad
            out.push(depth);
            write_u16(&mut out, 0); // unused
            out.extend_from_slice(chunk);
            out.resize(24 + padded_data_len, 0);
            self.stream.write_all(&out)?;
            row += rows;
        }
        self.stream.flush()
    }

    pub fn poly_fill_arc(&mut self, host_xid: u32, foreground: u32, arcs: &[u8]) -> io::Result<()> {
        self.draw_arcs(host_xid, 71, foreground, arcs)
    }

    pub fn poly_arc(&mut self, host_xid: u32, foreground: u32, arcs: &[u8]) -> io::Result<()> {
        self.draw_arcs(host_xid, 68, foreground, arcs)
    }

    pub fn poly_rectangle(
        &mut self,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        self.draw_rectangles(host_xid, 67, foreground, rectangles)
    }

    pub fn poly_segment(
        &mut self,
        host_xid: u32,
        foreground: u32,
        segments: &[u8],
    ) -> io::Result<()> {
        self.draw_rectangles(host_xid, 66, foreground, segments)
    }

    pub fn poly_fill_rectangle(
        &mut self,
        host_xid: u32,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        self.draw_rectangles(host_xid, 70, foreground, rectangles)
    }

    fn draw_rectangles(
        &mut self,
        host_xid: u32,
        opcode: u8,
        foreground: u32,
        rectangles: &[u8],
    ) -> io::Result<()> {
        if rectangles.is_empty() {
            return Ok(());
        }
        if self.current_foreground != foreground {
            self.change_foreground(foreground)?;
        }

        let length_units = 3 + u16::try_from(rectangles.len() / 4).map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "too many rectangles for one X11 request",
            )
        })?;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, self.gc_id);
        out.extend_from_slice(rectangles);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn fill_rectangle(
        &mut self,
        host_xid: u32,
        foreground: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> io::Result<()> {
        let mut rectangle = Vec::with_capacity(8);
        write_i16(&mut rectangle, x);
        write_i16(&mut rectangle, y);
        write_u16(&mut rectangle, width);
        write_u16(&mut rectangle, height);
        self.poly_fill_rectangle(host_xid, foreground, &rectangle)
    }

    pub fn poly_line(
        &mut self,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        if self.current_foreground != foreground {
            self.change_foreground(foreground)?;
        }

        let length_units = 3 + u16::try_from(points.len() / 4).map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "too many points for one X11 request",
            )
        })?;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(65);
        out.push(coordinate_mode);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, self.gc_id);
        out.extend_from_slice(points);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn image_text8(
        &mut self,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        if body.len() < 12 {
            return Ok(());
        }
        self.change_colors(foreground, background)?;

        let text = &body[12..];
        let length_units = 4 + u16::try_from(text.len() / 4)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "text request is too large"))?;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(76);
        out.push(text_len);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, self.gc_id);
        out.extend_from_slice(&body[8..12]);
        out.extend_from_slice(text);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn image_text16(
        &mut self,
        host_xid: u32,
        foreground: u32,
        background: u32,
        text_len: u8,
        body: &[u8],
    ) -> io::Result<()> {
        if body.len() < 12 {
            return Ok(());
        }
        self.change_colors(foreground, background)?;

        let text = &body[12..];
        let length_units = 4 + u16::try_from(text.len() / 4)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "text request is too large"))?;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(77);
        out.push(text_len);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, self.gc_id);
        out.extend_from_slice(&body[8..12]);
        out.extend_from_slice(text);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn poly_text8(&mut self, host_xid: u32, foreground: u32, body: &[u8]) -> io::Result<()> {
        if body.len() < 12 {
            return Ok(());
        }
        self.change_foreground(foreground)?;

        let length_units = 1 + u16::try_from(body.len() / 4)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "text request is too large"))?;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(74);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, self.gc_id);
        out.extend_from_slice(&body[8..]);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn poly_text16(&mut self, host_xid: u32, foreground: u32, body: &[u8]) -> io::Result<()> {
        if body.len() < 12 {
            return Ok(());
        }
        self.change_foreground(foreground)?;

        let length_units = 1 + u16::try_from(body.len() / 4)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "text request is too large"))?;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(75);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, self.gc_id);
        out.extend_from_slice(&body[8..]);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn poly_point(
        &mut self,
        host_xid: u32,
        foreground: u32,
        coordinate_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        if self.current_foreground != foreground {
            self.change_foreground(foreground)?;
        }

        let length_units = 3 + u16::try_from(points.len() / 4).map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "too many points for one X11 request",
            )
        })?;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(64);
        out.push(coordinate_mode);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, self.gc_id);
        out.extend_from_slice(points);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn fill_poly(
        &mut self,
        host_xid: u32,
        foreground: u32,
        coord_mode: u8,
        points: &[u8],
    ) -> io::Result<()> {
        if points.len() < 4 {
            return Ok(());
        }
        if self.current_foreground != foreground {
            self.change_foreground(foreground)?;
        }

        // FillPoly opcode 69: drawable, gc, shape(1), coord-mode(1), pad(2), points...
        // length = 4 (header words) + npoints
        let length_units = 4 + u16::try_from(points.len() / 4).map_err(|_| {
            io::Error::new(
                ErrorKind::InvalidInput,
                "too many points for one X11 request",
            )
        })?;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(69);
        out.push(0); // unused
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, self.gc_id);
        out.push(0); // shape = Complex
        out.push(coord_mode);
        write_u16(&mut out, 0); // pad
        out.extend_from_slice(points);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    fn draw_arcs(
        &mut self,
        host_xid: u32,
        opcode: u8,
        foreground: u32,
        arcs: &[u8],
    ) -> io::Result<()> {
        if arcs.is_empty() {
            return Ok(());
        }
        if self.current_foreground != foreground {
            self.change_foreground(foreground)?;
        }

        let length_units = 3 + u16::try_from(arcs.len() / 4).map_err(|_| {
            io::Error::new(ErrorKind::InvalidInput, "too many arcs for one X11 request")
        })?;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(opcode);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, self.gc_id);
        out.extend_from_slice(arcs);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    fn change_foreground(&mut self, foreground: u32) -> io::Result<()> {
        if self.current_foreground == foreground {
            return Ok(());
        }

        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(56);
        out.push(0);
        write_u16(&mut out, 4);
        write_u32(&mut out, self.gc_id);
        write_u32(&mut out, 1 << 2);
        write_u32(&mut out, foreground);
        self.stream.write_all(&out)?;
        self.current_foreground = foreground;
        Ok(())
    }

    fn change_colors(&mut self, foreground: u32, background: u32) -> io::Result<()> {
        if self.current_foreground == foreground && self.current_background == background {
            return Ok(());
        }

        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(56);
        out.push(0);
        write_u16(&mut out, 5);
        write_u32(&mut out, self.gc_id);
        write_u32(&mut out, (1 << 2) | (1 << 3));
        write_u32(&mut out, foreground);
        write_u32(&mut out, background);
        self.stream.write_all(&out)?;
        self.current_foreground = foreground;
        self.current_background = background;
        Ok(())
    }

    fn read_fixed_reply(&mut self, reply: &mut [u8; 32]) -> io::Result<()> {
        loop {
            self.stream.read_exact(reply)?;
            if reply[0] == 1 {
                return Ok(());
            }
        }
    }
}

#[must_use]
pub(crate) fn xkb_minor_has_reply(minor: u8) -> bool {
    // Reply-producing XKB requests. Source: X11/extensions/XKB.h,
    // XKBproto.h, and Xlib call sites that enter _XReply().
    matches!(
        minor,
        0 | 3
            | 4
            | 5
            | 6
            | 8
            | 10
            | 12
            | 14
            | 16
            | 17
            | 18
            | 20
            | 21
            | 22
            | 24
            | 26
            | 28
            | 30
            | 33
            | 101
    )
}

fn connect_to_host() -> io::Result<UnixStream> {
    let display = env::var("DISPLAY").map_err(|_| {
        io::Error::new(
            ErrorKind::NotFound,
            "DISPLAY is not set for host X11 backend",
        )
    })?;
    let display_number = parse_display_number(&display)?;
    let socket_path = format!("/tmp/.X11-unix/X{display_number}");

    let auth = XAuthority::load(display_number).unwrap_or_default();
    let mut stream = UnixStream::connect(socket_path)?;
    write_setup_request(&mut stream, auth.as_ref())?;
    Ok(stream)
}

impl HostInputPump {
    pub fn open_from_env(window_id: u32) -> io::Result<Self> {
        let mut stream = connect_to_host()?;
        let _setup = read_setup_reply(&mut stream)?;
        select_keyboard_events(&mut stream, window_id)?;
        stream.flush()?;
        let read_stream = stream.try_clone()?;
        // Map the host container window to ynest's ROOT_WINDOW so Expose
        // events on the container (raised by the host when subwindows uncover
        // the desktop area) are delivered as Expose on ROOT_WINDOW. Without
        // this entry expose_event_fanout drops the event and the desktop
        // background is never repainted after a window drag.
        let mut xid_map = HashMap::new();
        xid_map.insert(window_id, crate::resources::ROOT_WINDOW);
        let handle = HostInputPumpHandle {
            write_stream: Arc::new(Mutex::new(stream)),
            xid_map: Arc::new(Mutex::new(xid_map)),
        };
        Ok(Self {
            read_stream,
            handle,
        })
    }

    #[must_use]
    pub fn handle(&self) -> HostInputPumpHandle {
        self.handle.clone()
    }

    pub fn read_event(&mut self) -> io::Result<HostEvent> {
        loop {
            let mut event = [0; 32];
            self.read_stream.read_exact(&mut event)?;
            let event_type = event[0] & 0x7f;
            match event_type {
                2 | 3 => {
                    return Ok(HostEvent::Key(HostKeyEvent {
                        pressed: event_type == 2,
                        keycode: event[1],
                        time: read_u32(&event[4..8]),
                        root_x: read_i16(&event[20..22]),
                        root_y: read_i16(&event[22..24]),
                        event_x: read_i16(&event[24..26]),
                        event_y: read_i16(&event[26..28]),
                        state: read_u16(&event[28..30]),
                    }));
                }
                4..=6 => {
                    let kind = match event_type {
                        4 => PointerEventKind::ButtonPress,
                        5 => PointerEventKind::ButtonRelease,
                        _ => PointerEventKind::MotionNotify,
                    };
                    return Ok(HostEvent::Pointer(HostPointerEvent {
                        kind,
                        host_xid: read_u32(&event[12..16]), // event window
                        detail: event[1],
                        time: read_u32(&event[4..8]),
                        root_x: read_i16(&event[20..22]),
                        root_y: read_i16(&event[22..24]),
                        event_x: read_i16(&event[24..26]),
                        event_y: read_i16(&event[26..28]),
                        state: read_u16(&event[28..30]),
                    }));
                }
                7 | 8 => {
                    let kind = if event_type == 7 {
                        PointerEventKind::EnterNotify
                    } else {
                        PointerEventKind::LeaveNotify
                    };
                    return Ok(HostEvent::Pointer(HostPointerEvent {
                        kind,
                        host_xid: read_u32(&event[12..16]),
                        detail: 0,
                        time: read_u32(&event[4..8]),
                        root_x: read_i16(&event[20..22]),
                        root_y: read_i16(&event[22..24]),
                        event_x: read_i16(&event[24..26]),
                        event_y: read_i16(&event[26..28]),
                        state: read_u16(&event[28..30]),
                    }));
                }
                12 => {
                    let host_xid = read_u32(&event[4..8]);
                    let x = read_u16(&event[8..10]);
                    let y = read_u16(&event[10..12]);
                    let width = read_u16(&event[12..14]);
                    let height = read_u16(&event[14..16]);
                    let count = read_u16(&event[16..18]);
                    log::trace!(
                        "host pump: Expose host_xid=0x{host_xid:x} x={x} y={y} w={width} h={height} count={count}",
                    );
                    return Ok(HostEvent::Expose(HostExposeEvent {
                        host_xid,
                        x,
                        y,
                        width,
                        height,
                        count,
                    }));
                }
                22 => {
                    return Ok(HostEvent::Configure(HostConfigureEvent {
                        host_xid: read_u32(&event[8..12]),
                        x: read_i16(&event[16..18]),
                        y: read_i16(&event[18..20]),
                        width: read_u16(&event[20..22]),
                        height: read_u16(&event[22..24]),
                    }));
                }
                17 => return Ok(HostEvent::Closed),
                _ => {}
            }
        }
    }
}

const POINTER_EVENT_MASK: u32 = 0x0000_0004 // ButtonPress
    | 0x0000_0008 // ButtonRelease
    | 0x0000_0010 // EnterWindow
    | 0x0000_0020 // LeaveWindow
    | 0x0000_0040 // PointerMotion
    | 0x0000_8000; // Exposure

impl HostInputPumpHandle {
    pub fn register_top_level(&self, nested_id: ResourceId, host_xid: u32) -> io::Result<()> {
        // Insert into the map *before* writing to X11 so that any pointer
        // events arriving on this subwindow after ChangeWindowAttributes are
        // sent can be resolved to a nested window id immediately.
        if let Ok(mut map) = self.xid_map.lock() {
            map.insert(host_xid, nested_id);
        }
        // ChangeWindowAttributes — value-mask = (1<<11) (event-mask), value = pointer mask.
        let mut out = Vec::new();
        out.push(2); // ChangeWindowAttributes
        out.push(0);
        write_u16(&mut out, 4);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, 1 << 11);
        write_u32(&mut out, POINTER_EVENT_MASK);
        let mut stream = self
            .write_stream
            .lock()
            .map_err(|_| io::Error::new(ErrorKind::BrokenPipe, "host pump stream poisoned"))?;
        stream.write_all(&out)?;
        stream.flush()?;
        log::debug!(
            "host pump: register_top_level nested=0x{:x} host=0x{:x}",
            nested_id.0,
            host_xid
        );
        Ok(())
    }

    pub fn unregister_top_level(&self, host_xid: u32) {
        if let Ok(mut map) = self.xid_map.lock() {
            map.remove(&host_xid);
        }
    }

    #[must_use]
    pub fn xid_map(&self) -> HostXidMap {
        self.xid_map.clone()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum HostEvent {
    Key(HostKeyEvent),
    Pointer(HostPointerEvent),
    Expose(HostExposeEvent),
    Configure(HostConfigureEvent),
    Closed,
}

#[derive(Clone, Copy, Debug)]
pub struct HostExposeEvent {
    pub host_xid: u32,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub count: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct HostConfigureEvent {
    pub host_xid: u32,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerEventKind {
    ButtonPress,
    ButtonRelease,
    MotionNotify,
    EnterNotify,
    LeaveNotify,
}

#[derive(Clone, Copy, Debug)]
pub struct HostPointerEvent {
    pub kind: PointerEventKind,
    pub host_xid: u32,
    pub detail: u8,
    pub time: u32,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct HostKeyEvent {
    pub pressed: bool,
    pub keycode: u8,
    pub time: u32,
    pub root_x: i16,
    pub root_y: i16,
    pub event_x: i16,
    pub event_y: i16,
    pub state: u16,
}

#[cfg(test)]
mod tests {
    use super::{build_shape_rectangles, xkb_minor_has_reply};
    use yserver_protocol::x11::xfixes::RegionRect;

    #[test]
    fn xkb_reply_minor_audit_includes_known_blocking_requests() {
        for minor in [0, 4, 8, 10, 14, 17, 20, 21, 24, 101] {
            assert!(
                xkb_minor_has_reply(minor),
                "minor {minor} must wait for reply"
            );
        }
    }

    #[test]
    fn xkb_void_minor_audit_keeps_select_events_fire_and_forget() {
        assert!(!xkb_minor_has_reply(1)); // SelectEvents
        assert!(!xkb_minor_has_reply(2)); // Bell
    }

    #[test]
    fn shape_rectangles_no_host_opcode_returns_none() {
        // No host SHAPE opcode → caller should send nothing.
        assert!(build_shape_rectangles(None, 0xdead_beef, 0, &[]).is_none());
    }

    #[test]
    fn change_cursor_by_name_no_host_opcode_returns_none() {
        // No host XFIXES opcode → caller should send nothing and not crash.
        assert!(super::build_xfixes_change_cursor_by_name(None, 0x10, b"left_ptr").is_none());
    }

    #[test]
    fn change_cursor_by_name_wire_shape_unpadded_name() {
        // 8-byte name "left_ptr" → no padding needed.
        let bytes =
            super::build_xfixes_change_cursor_by_name(Some(140), 0xdead_beef, b"left_ptr").unwrap();
        // header(4) + cursor(4) + nbytes(2)+pad(2) + name(8) = 20 bytes = 5 units
        assert_eq!(bytes.len(), 20);
        assert_eq!(bytes[0], 140, "major = XFIXES");
        assert_eq!(bytes[1], 23, "minor = ChangeCursorByName");
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 5);
        assert_eq!(
            u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            0xdead_beef,
        );
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 8);
        assert_eq!(&bytes[12..20], b"left_ptr");
    }

    #[test]
    fn change_cursor_by_name_wire_shape_pads_name_to_4() {
        // 5-byte name → 3 padding bytes appended.
        let bytes = super::build_xfixes_change_cursor_by_name(Some(140), 0x1, b"hand1").unwrap();
        // 12 (header) + padded_len(5) = 12 + 8 = 20 bytes = 5 units
        assert_eq!(bytes.len(), 20);
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 5);
        assert_eq!(u16::from_le_bytes([bytes[8], bytes[9]]), 5);
        assert_eq!(&bytes[12..17], b"hand1");
        assert_eq!(&bytes[17..20], &[0, 0, 0]);
    }

    fn glyphcmd(count: u8, dx: i16, dy: i16) -> [u8; 8] {
        let mut buf = [0u8; 8];
        buf[0] = count;
        buf[4..6].copy_from_slice(&dx.to_le_bytes());
        buf[6..8].copy_from_slice(&dy.to_le_bytes());
        buf
    }

    fn read_dx(items: &[u8], pos: usize) -> i16 {
        i16::from_le_bytes([items[pos + 4], items[pos + 5]])
    }

    fn read_dy(items: &[u8], pos: usize) -> i16 {
        i16::from_le_bytes([items[pos + 6], items[pos + 7]])
    }

    #[test]
    fn glyph_offset_patches_every_non_255_command() {
        // Two real glyph runs (count=1, count=2) separated by a 255 sentinel
        // marking a glyphset switch. Every non-sentinel command's delta must
        // be shifted by (x_off, y_off) so multi-run composites line up at the
        // host's destination origin.
        let mut items = Vec::new();
        items.extend_from_slice(&glyphcmd(1, 10, 20)); // run 1
        items.extend_from_slice(&glyphcmd(255, 0, 0)); // glyphset switch
        items.extend_from_slice(&[0u8; 4]); // 8-byte alignment for the 255 cmd
        items.extend_from_slice(&glyphcmd(2, 30, 40)); // run 2

        super::patch_glyph_command_offsets(&mut items, 5, -3, 1);

        assert_eq!(read_dx(&items, 0), 15);
        assert_eq!(read_dy(&items, 0), 17);
        // 255 sentinel cmd untouched (its delta bytes are still 0).
        assert_eq!(read_dx(&items, 8), 0);
        assert_eq!(read_dy(&items, 8), 0);
        // Second run patched too.
        assert_eq!(read_dx(&items, 20), 35);
        assert_eq!(read_dy(&items, 20), 37);
    }

    #[test]
    fn glyph_offset_zero_offset_is_identity() {
        let mut items = Vec::new();
        items.extend_from_slice(&glyphcmd(1, 10, 20));
        items.extend_from_slice(&glyphcmd(2, 30, 40));
        let original = items.clone();
        super::patch_glyph_command_offsets(&mut items, 0, 0, 1);
        assert_eq!(items, original);
    }

    #[test]
    fn glyph_offset_handles_16bit_glyph_id_stride() {
        // Two CompositeGlyphs16 runs back to back: header(8) + count*2 padded
        // to 4. count=3 → 6 bytes payload → padded 8 bytes → next header at
        // pos+16.
        let mut items = Vec::new();
        items.extend_from_slice(&glyphcmd(3, 10, 20));
        items.extend_from_slice(&[0xaa, 0xaa, 0xbb, 0xbb, 0xcc, 0xcc, 0, 0]); // 3 ids + pad
        items.extend_from_slice(&glyphcmd(2, 30, 40));
        items.extend_from_slice(&[0xdd, 0xdd, 0xee, 0xee]); // 2 ids, no pad needed

        super::patch_glyph_command_offsets(&mut items, 1, 2, 2);

        assert_eq!(read_dx(&items, 0), 11);
        assert_eq!(read_dy(&items, 0), 22);
        assert_eq!(read_dx(&items, 16), 31);
        assert_eq!(read_dy(&items, 16), 42);
    }

    #[test]
    fn shape_rectangles_empty_list_clears_shape() {
        // Empty rect list with op=Set is the canonical "no shape" — header only,
        // no rectangle bodies.
        let bytes = build_shape_rectangles(Some(141), 0xdead_beef, 0, &[]).unwrap();
        assert_eq!(bytes.len(), 16);
        assert_eq!(bytes[0], 141, "major opcode");
        assert_eq!(bytes[1], 1, "minor = RECTANGLES");
        assert_eq!(
            u16::from_le_bytes([bytes[2], bytes[3]]),
            4,
            "length = 4 units"
        );
        assert_eq!(bytes[4], 0, "op = Set");
        assert_eq!(bytes[5], 0, "kind = Bounding");
        assert_eq!(bytes[6], 0, "ordering = Unsorted");
        assert_eq!(
            u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            0xdead_beef,
            "dest = host xid",
        );
        // x_off, y_off are zero for top-level forwarding.
        assert_eq!(i16::from_le_bytes([bytes[12], bytes[13]]), 0);
        assert_eq!(i16::from_le_bytes([bytes[14], bytes[15]]), 0);
    }

    #[test]
    fn shape_rectangles_clip_kind_mapping() {
        // KIND_CLIP is 1; ensure it makes it onto the wire as-is.
        let bytes = build_shape_rectangles(Some(141), 0x1, 1, &[]).unwrap();
        assert_eq!(bytes[5], 1, "kind = Clip");
    }

    #[test]
    fn shape_rectangles_multi_rect_payload_unchanged() {
        // Top-level shape coords are already in the host subwindow's local
        // coords (subwindow position itself is the offset). Forwarder must
        // not translate per-rect — assert the rectangle bytes are forwarded
        // verbatim.
        let rects = [
            RegionRect {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            },
            RegionRect {
                x: 10,
                y: 20,
                width: 30,
                height: 40,
            },
        ];
        let bytes = build_shape_rectangles(Some(141), 0xabad_cafe, 0, &rects).unwrap();
        // header(16) + 2 rects * 8 bytes = 32 bytes = 8 units
        assert_eq!(bytes.len(), 32);
        assert_eq!(u16::from_le_bytes([bytes[2], bytes[3]]), 8);

        assert_eq!(i16::from_le_bytes([bytes[16], bytes[17]]), 1);
        assert_eq!(i16::from_le_bytes([bytes[18], bytes[19]]), 2);
        assert_eq!(u16::from_le_bytes([bytes[20], bytes[21]]), 3);
        assert_eq!(u16::from_le_bytes([bytes[22], bytes[23]]), 4);

        assert_eq!(i16::from_le_bytes([bytes[24], bytes[25]]), 10);
        assert_eq!(i16::from_le_bytes([bytes[26], bytes[27]]), 20);
        assert_eq!(u16::from_le_bytes([bytes[28], bytes[29]]), 30);
        assert_eq!(u16::from_le_bytes([bytes[30], bytes[31]]), 40);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PointerPosition {
    pub same_screen: bool,
    pub win_x: i16,
    pub win_y: i16,
    pub mask: u16,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HostSubwindowConfig {
    pub x: Option<i16>,
    pub y: Option<i16>,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub sibling: Option<u32>,
    pub stack_mode: Option<u8>,
}

#[derive(Clone, Debug, Default)]
struct XAuthority {
    name: Vec<u8>,
    data: Vec<u8>,
}

impl XAuthority {
    fn load(display_number: u16) -> io::Result<Option<Self>> {
        let path = env::var_os("XAUTHORITY")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".Xauthority")))
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "no Xauthority path"))?;

        let bytes = fs::read(path)?;
        let display_number = display_number.to_string();
        let mut cursor = 0;
        let mut fallback = None;

        while cursor < bytes.len() {
            let Some(_family) = read_be_u16_record(&bytes, &mut cursor) else {
                break;
            };
            let Some(address) = read_record_field(&bytes, &mut cursor) else {
                break;
            };
            let Some(number) = read_record_field(&bytes, &mut cursor) else {
                break;
            };
            let Some(name) = read_record_field(&bytes, &mut cursor) else {
                break;
            };
            let Some(data) = read_record_field(&bytes, &mut cursor) else {
                break;
            };

            if name == MIT_MAGIC_COOKIE.as_bytes() && number == display_number.as_bytes() {
                let auth = Self { name, data };
                if address.is_empty() {
                    return Ok(Some(auth));
                }
                fallback = Some(auth);
            }
        }

        Ok(fallback)
    }
}

#[derive(Clone, Copy, Debug)]
struct HostSetup {
    resource_id_base: u32,
    root: u32,
    root_visual: u32,
    root_depth: u8,
    white_pixel: u32,
    black_pixel: u32,
}

fn parse_display_number(display: &str) -> io::Result<u16> {
    let display = display
        .rsplit_once(':')
        .map_or(display, |(_, suffix)| suffix);
    let number = display.split('.').next().unwrap_or(display);
    number.parse::<u16>().map_err(|err| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("unsupported DISPLAY value {display:?}: {err}"),
        )
    })
}

fn write_setup_request(stream: &mut UnixStream, auth: Option<&XAuthority>) -> io::Result<()> {
    let (name, data) = auth
        .map(|auth| (auth.name.as_slice(), auth.data.as_slice()))
        .unwrap_or((&[][..], &[][..]));

    let mut out = Vec::new();
    out.push(b'l');
    out.push(0);
    write_u16(&mut out, 11);
    write_u16(&mut out, 0);
    write_u16(&mut out, name.len() as u16);
    write_u16(&mut out, data.len() as u16);
    write_u16(&mut out, 0);
    out.extend_from_slice(name);
    pad4(&mut out);
    out.extend_from_slice(data);
    pad4(&mut out);
    stream.write_all(&out)
}

fn read_setup_reply(stream: &mut UnixStream) -> io::Result<HostSetup> {
    let mut header = [0; 8];
    stream.read_exact(&mut header)?;
    if header[0] != 1 {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("host X11 setup failed with status {}", header[0]),
        ));
    }

    let length = u16::from_le_bytes([header[6], header[7]]) as usize * 4;
    let mut body = vec![0; length];
    stream.read_exact(&mut body)?;
    if body.len() < 40 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "host X11 setup body is too short",
        ));
    }

    let resource_id_base = read_u32(&body[4..8]);
    let vendor_len = read_u16(&body[16..18]) as usize;
    let roots_len = body[20] as usize;
    let pixmap_formats_len = body[21] as usize;
    if roots_len == 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "host X11 server has no roots",
        ));
    }

    let screen_offset = 32 + padded_len(vendor_len) + pixmap_formats_len * 8;
    if body.len() < screen_offset + 40 {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "host X11 screen body is too short",
        ));
    }

    let screen = &body[screen_offset..];
    Ok(HostSetup {
        resource_id_base,
        root: read_u32(&screen[0..4]),
        root_visual: read_u32(&screen[32..36]),
        root_depth: screen[38],
        white_pixel: read_u32(&screen[8..12]),
        black_pixel: read_u32(&screen[12..16]),
    })
}

fn create_window(stream: &mut UnixStream, setup: &HostSetup, window_id: u32) -> io::Result<()> {
    // Value-mask: bg-pixel (bit 1) | bit-gravity (bit 4) | event-mask (bit 11).
    // bit-gravity = NorthWest (1) so a host-side resize preserves the NW pixels.
    // Without this the gravity defaults to Forget and the host server is free
    // to clear the entire container on resize, which paints over every visible
    // subwindow and leaves the desktop blank until the apps redraw.
    let value_mask: u32 = (1 << 1) | (1 << 4) | (1 << 11);
    // length = 3 fixed words + 1 word per value bit (3 values). 3 + 3 = 6
    // fixed; add 4-word CreateWindow header → 10 total length units.
    let mut out = Vec::new();
    out.push(1);
    out.push(setup.root_depth);
    write_u16(&mut out, 11);
    write_u32(&mut out, window_id);
    write_u32(&mut out, setup.root);
    write_i16(&mut out, 80);
    write_i16(&mut out, 80);
    write_u16(&mut out, 800);
    write_u16(&mut out, 600);
    write_u16(&mut out, 0);
    write_u16(&mut out, 1);
    write_u32(&mut out, setup.root_visual);
    write_u32(&mut out, value_mask);
    write_u32(&mut out, setup.white_pixel); // bg-pixel
    write_u32(&mut out, 1); // bit-gravity = NorthWest
    write_u32(&mut out, 0x0000_8000 | 0x0002_0000); // event-mask
    stream.write_all(&out)
}

fn select_keyboard_events(stream: &mut UnixStream, window_id: u32) -> io::Result<()> {
    let mut out = Vec::new();
    out.push(2);
    out.push(0);
    write_u16(&mut out, 4);
    write_u32(&mut out, window_id);
    write_u32(&mut out, 1 << 11);
    // KeyPress | KeyRelease | StructureNotify plus pointer/exposure events.
    // Pointer events on the host container are root-window events in ynest;
    // top-level subwindows register their own pointer masks separately.
    write_u32(
        &mut out,
        (1 << 0) | (1 << 1) | (1 << 17) | POINTER_EVENT_MASK,
    );
    stream.write_all(&out)
}

fn create_gc(
    stream: &mut UnixStream,
    drawable: u32,
    gc_id: u32,
    foreground: u32,
    background: u32,
    font_id: u32,
) -> io::Result<()> {
    let mut out = Vec::new();
    out.push(55);
    out.push(0);
    write_u16(&mut out, 7);
    write_u32(&mut out, gc_id);
    write_u32(&mut out, drawable);
    write_u32(&mut out, (1 << 2) | (1 << 3) | (1 << 14));
    write_u32(&mut out, foreground);
    write_u32(&mut out, background);
    write_u32(&mut out, font_id);
    stream.write_all(&out)
}

fn open_font(stream: &mut UnixStream, font_id: u32, name: &[u8]) -> io::Result<()> {
    let padded_name_len = padded_len(name.len());
    let length_units = 3 + u16::try_from(padded_name_len / 4)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "font name is too long"))?;

    let mut out = Vec::new();
    out.push(45);
    out.push(0);
    write_u16(&mut out, length_units);
    write_u32(&mut out, font_id);
    write_u16(
        &mut out,
        u16::try_from(name.len())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "font name is too long"))?,
    );
    write_u16(&mut out, 0);
    out.extend_from_slice(name);
    out.resize(12 + padded_name_len, 0);
    stream.write_all(&out)
}

fn map_window(stream: &mut UnixStream, window_id: u32) -> io::Result<()> {
    let mut out = Vec::new();
    out.push(8);
    out.push(0);
    write_u16(&mut out, 2);
    write_u32(&mut out, window_id);
    stream.write_all(&out)
}

fn read_be_u16_record(bytes: &[u8], cursor: &mut usize) -> Option<u16> {
    let end = *cursor + 2;
    let value = u16::from_be_bytes(bytes.get(*cursor..end)?.try_into().ok()?);
    *cursor = end;
    Some(value)
}

fn read_record_field(bytes: &[u8], cursor: &mut usize) -> Option<Vec<u8>> {
    let len = read_be_u16_record(bytes, cursor)? as usize;
    let end = *cursor + len;
    let value = bytes.get(*cursor..end)?.to_vec();
    *cursor = end;
    Some(value)
}

fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_i16(bytes: &[u8]) -> i16 {
    i16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn write_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_i16(out: &mut Vec<u8>, value: i16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn padded_len(len: usize) -> usize {
    (len + 3) & !3
}

fn pad4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

/// Add `(x_off, y_off)` to every non-sentinel glyph command's delta in a
/// `CompositeGlyphs{8,16,32}` payload. Previously this only patched the first
/// command's delta and stopped, which mispositioned multi-run composites
/// — anything containing a 255 sentinel that switches glyphsets between runs.
///
/// `id_size` is 1 for `CompositeGlyphs8` (minor 23), 2 for the 16-bit variant
/// (minor 24), 4 for the 32-bit variant (minor 25). The glyph-id payload that
/// follows each non-sentinel header is `count * id_size` bytes, padded to a
/// 4-byte boundary; the next header begins immediately after that.
fn patch_glyph_command_offsets(items: &mut [u8], x_off: i16, y_off: i16, id_size: usize) {
    if x_off == 0 && y_off == 0 {
        return;
    }
    debug_assert!(matches!(id_size, 1 | 2 | 4));
    let mut pos = 0;
    while pos + 8 <= items.len() {
        let count = items[pos];
        if count == 255 {
            // Glyphset-switch sentinel: 8 bytes total (count, pad×3, glyphset
            // XID), no payload follows.
            pos += 8;
            continue;
        }
        let dx = i16::from_le_bytes([items[pos + 4], items[pos + 5]]).wrapping_add(x_off);
        let dy = i16::from_le_bytes([items[pos + 6], items[pos + 7]]).wrapping_add(y_off);
        items[pos + 4..pos + 6].copy_from_slice(&dx.to_le_bytes());
        items[pos + 6..pos + 8].copy_from_slice(&dy.to_le_bytes());
        let payload_bytes = usize::from(count) * id_size;
        let padded = (payload_bytes + 3) & !3;
        pos += 8 + padded;
    }
}

/// Build the wire bytes for XFIXES `ChangeCursorByName` (minor 23). Returns
/// `None` when the host XFIXES extension is unavailable.
fn build_xfixes_change_cursor_by_name(
    host_xfixes_opcode: Option<u8>,
    host_cursor_xid: u32,
    name_bytes: &[u8],
) -> Option<Vec<u8>> {
    let opcode = host_xfixes_opcode?;
    let nbytes = u16::try_from(name_bytes.len()).ok()?;
    let padded_name = padded_len(name_bytes.len());
    let length_units = u16::try_from(3 + padded_name / 4).ok()?;
    let mut out = Vec::with_capacity(12 + padded_name);
    out.push(opcode);
    out.push(yserver_protocol::x11::xfixes::CHANGE_CURSOR_BY_NAME);
    write_u16(&mut out, length_units);
    write_u32(&mut out, host_cursor_xid);
    write_u16(&mut out, nbytes);
    write_u16(&mut out, 0); // pad
    out.extend_from_slice(name_bytes);
    out.resize(12 + padded_name, 0);
    Some(out)
}

/// Build the wire bytes for SHAPE `Rectangles(op=Set, ordering=Unsorted)`
/// targeting `host_xid`. Returns `None` when the host SHAPE extension is
/// unavailable so the caller doesn't waste a stream write.
fn build_shape_rectangles(
    host_shape_opcode: Option<u8>,
    host_xid: u32,
    kind: u8,
    rects: &[yserver_protocol::x11::xfixes::RegionRect],
) -> Option<Vec<u8>> {
    let opcode = host_shape_opcode?;
    let length_units = u16::try_from(4 + rects.len() * 2).ok()?;
    let mut out = Vec::with_capacity(16 + rects.len() * 8);
    out.push(opcode);
    out.push(yserver_protocol::x11::shape::RECTANGLES);
    write_u16(&mut out, length_units);
    out.push(yserver_protocol::x11::shape::OP_SET);
    out.push(kind);
    out.push(0); // ordering = Unsorted
    out.push(0); // pad
    write_u32(&mut out, host_xid);
    write_i16(&mut out, 0); // x_off (top-level coords already match)
    write_i16(&mut out, 0); // y_off
    for rect in rects {
        write_i16(&mut out, rect.x);
        write_i16(&mut out, rect.y);
        write_u16(&mut out, rect.width);
        write_u16(&mut out, rect.height);
    }
    Some(out)
}

fn write_open_font(stream: &mut UnixStream, font_id: u32, name: &[u8]) -> io::Result<()> {
    open_font(stream, font_id, name)
}

fn write_query_font(stream: &mut UnixStream, font_id: u32) -> io::Result<()> {
    let mut out = Vec::new();
    out.push(47);
    out.push(0);
    write_u16(&mut out, 2);
    write_u32(&mut out, font_id);
    stream.write_all(&out)
}

fn write_list_fonts(
    stream: &mut UnixStream,
    opcode: u8,
    max_names: u16,
    pattern: &[u8],
) -> io::Result<()> {
    let padded = padded_len(pattern.len());
    let length_units = 2 + u16::try_from(padded / 4)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "font pattern is too long"))?;

    let mut out = Vec::new();
    out.push(opcode);
    out.push(0);
    write_u16(&mut out, length_units);
    write_u16(&mut out, max_names);
    write_u16(
        &mut out,
        u16::try_from(pattern.len())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "font pattern is too long"))?,
    );
    out.extend_from_slice(pattern);
    out.resize(8 + padded, 0);
    stream.write_all(&out)
}

fn parse_host_pict_formats(bytes: &[u8], opcode: u8) -> io::Result<HostRenderInfo> {
    if bytes.len() < 32 {
        return Err(io::Error::other("QueryPictFormats reply too short"));
    }
    let num_formats = read_u32(&bytes[8..12]) as usize;
    let mut fmt_a1 = 0u32;
    let mut fmt_a8 = 0u32;
    let mut fmt_rgb24 = 0u32;
    let mut fmt_argb32 = 0u32;
    for i in 0..num_formats {
        let base = 32 + i * 28;
        if base + 28 > bytes.len() {
            break;
        }
        let id = read_u32(&bytes[base..base + 4]);
        let type_ = bytes[base + 4];
        let depth = bytes[base + 5];
        let alpha_shift = read_u16(&bytes[base + 20..base + 22]);
        let alpha_mask = read_u16(&bytes[base + 22..base + 24]);
        let red_shift = read_u16(&bytes[base + 8..base + 10]);
        let red_mask = read_u16(&bytes[base + 10..base + 12]);
        if type_ == 1 {
            match depth {
                1 if alpha_mask == 1 => fmt_a1 = id,
                8 if alpha_mask == 0xFF && alpha_shift == 0 => fmt_a8 = id,
                24 if red_mask == 0xFF && red_shift == 16 && alpha_mask == 0 => fmt_rgb24 = id,
                32 if alpha_mask == 0xFF && alpha_shift == 24 => fmt_argb32 = id,
                _ => {}
            }
        }
    }
    Ok(HostRenderInfo {
        opcode,
        fmt_a1,
        fmt_a8,
        fmt_rgb24,
        fmt_argb32,
    })
}

struct HostResponse {
    sequence: u16,
    bytes: Vec<u8>,
}

fn read_response(stream: &mut UnixStream) -> io::Result<HostResponse> {
    let mut header = [0u8; 32];
    loop {
        stream.read_exact(&mut header)?;
        match header[0] {
            0 | 1 => break,
            35 => {
                // GenericEvent: may have extra data beyond the 32-byte header.
                // Read and discard any extra bytes to keep the stream aligned.
                let extra =
                    u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize * 4;
                log::debug!(
                    "read_response: GenericEvent extra={} seq={}",
                    extra,
                    u16::from_le_bytes([header[2], header[3]])
                );
                if extra > 0 {
                    let mut tail = vec![0u8; extra];
                    stream.read_exact(&mut tail)?;
                }
                continue;
            }
            t => {
                log::debug!(
                    "read_response: skipping event type={} seq={}",
                    t,
                    u16::from_le_bytes([header[2], header[3]])
                );
                continue;
            }
        }
    }
    let sequence = u16::from_le_bytes([header[2], header[3]]);
    let extra = if header[0] == 1 {
        u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize * 4
    } else {
        0
    };
    let mut bytes = Vec::with_capacity(32 + extra);
    bytes.extend_from_slice(&header);
    if extra > 0 {
        let mut tail = vec![0u8; extra];
        stream.read_exact(&mut tail)?;
        bytes.extend_from_slice(&tail);
    }
    Ok(HostResponse { sequence, bytes })
}
