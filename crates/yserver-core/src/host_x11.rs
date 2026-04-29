use std::{
    collections::HashMap,
    env, fs,
    io::{self, ErrorKind, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use yserver_protocol::x11::{self, ClipRectangles, FontMetrics, ResourceId};

const MIT_MAGIC_COOKIE: &str = "MIT-MAGIC-COOKIE-1";

pub struct HostX11 {
    stream: UnixStream,
    window_id: u32,
    gc_id: u32,
    current_foreground: u32,
    current_background: u32,
    current_clip: Option<HostClipRectangles>,
    sequence: u16,
    next_xid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostClipRectangles {
    pub ordering: u8,
    pub x_origin: i16,
    pub y_origin: i16,
    pub rectangles: Vec<u8>,
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

        Ok(Self {
            stream,
            window_id,
            gc_id,
            current_foreground: setup.black_pixel,
            current_background: setup.white_pixel,
            current_clip: None,
            sequence: 5,
            next_xid: setup.resource_id_base + 3,
        })
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
            root_x: read_i16(&reply[16..18]),
            root_y: read_i16(&reply[18..20]),
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
        let mut out = Vec::new();
        out.push(1); // CreateWindow opcode
        out.push(0); // depth = CopyFromParent
        write_u16(&mut out, 8); // length: 8 units * 4 = 32 bytes
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
        write_u32(&mut out, 0); // value-mask = 0
        self.stream.write_all(&out)?;
        self.sequence = self.sequence.wrapping_add(1);

        // 2. GetGeometry round-trip — forces the host to commit CreateWindow
        //    before any later request (e.g. ChangeWindowAttributes from the
        //    pump's connection) can be processed. See spec §"Cross-connection
        //    ordering hazard".
        let geom_seq = self.sequence;
        let mut geom = Vec::new();
        geom.push(14); // GetGeometry opcode
        geom.push(0);
        write_u16(&mut geom, 2);
        write_u32(&mut geom, host_xid);
        self.stream.write_all(&geom)?;
        self.sequence = self.sequence.wrapping_add(1);
        self.stream.flush()?;

        // Drain replies/errors until we see geom_seq.
        loop {
            let resp = read_response(&mut self.stream)?;
            if resp.sequence == geom_seq {
                return Ok(());
            }
            // Ignore any earlier responses (e.g. an error on CreateWindow);
            // GetGeometry will then also fail and we'll see its reply here.
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
        x: Option<i16>,
        y: Option<i16>,
        width: Option<u16>,
        height: Option<u16>,
    ) -> io::Result<()> {
        let mut value_mask: u16 = 0;
        let mut values: Vec<u8> = Vec::new();
        if let Some(x) = x {
            value_mask |= 1 << 0;
            write_u32(&mut values, x as i32 as u32);
        }
        if let Some(y) = y {
            value_mask |= 1 << 1;
            write_u32(&mut values, y as i32 as u32);
        }
        if let Some(width) = width {
            value_mask |= 1 << 2;
            write_u32(&mut values, u32::from(width.max(1)));
        }
        if let Some(height) = height {
            value_mask |= 1 << 3;
            write_u32(&mut values, u32::from(height.max(1)));
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

    pub fn set_clip_rectangles(
        &mut self,
        clip: Option<ClipRectangles>,
        x_offset: i16,
        y_offset: i16,
    ) -> io::Result<()> {
        let clip = clip.map(|clip| HostClipRectangles {
            ordering: clip.ordering,
            x_origin: clip.x_origin.wrapping_add(x_offset),
            y_origin: clip.y_origin.wrapping_add(y_offset),
            rectangles: clip.rectangles,
        });
        self.set_host_clip_rectangles(clip)
    }

    pub fn clear_clip_rectangles(&mut self) -> io::Result<()> {
        self.set_host_clip_rectangles(None)
    }

    fn set_host_clip_rectangles(&mut self, clip: Option<HostClipRectangles>) -> io::Result<()> {
        if self.current_clip == clip {
            return Ok(());
        }

        if let Some(clip) = &clip {
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
        } else {
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

        self.current_clip = clip;
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
        let padded_data_len = padded_len(data.len());
        let length_units = 6 + u16::try_from(padded_data_len / 4)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "image data too large"))?;
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(72); // PutImage opcode
        out.push(2); // ZPixmap format
        write_u16(&mut out, length_units);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, self.gc_id);
        write_u16(&mut out, width);
        write_u16(&mut out, height);
        write_i16(&mut out, dst_x);
        write_i16(&mut out, dst_y);
        out.push(0); // left-pad
        out.push(depth);
        write_u16(&mut out, 0); // unused
        out.extend_from_slice(data);
        out.resize(24 + padded_data_len, 0);
        self.stream.write_all(&out)?;
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
        let handle = HostInputPumpHandle {
            write_stream: Arc::new(Mutex::new(stream)),
            xid_map: Arc::new(Mutex::new(HashMap::new())),
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
    | 0x0000_0040; // PointerMotion

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
        stream.flush()
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
    Closed,
}

#[derive(Clone, Copy, Debug)]
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

#[derive(Clone, Copy, Debug)]
pub struct PointerPosition {
    pub same_screen: bool,
    pub root_x: i16,
    pub root_y: i16,
    pub win_x: i16,
    pub win_y: i16,
    pub mask: u16,
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
    let mut out = Vec::new();
    out.push(1);
    out.push(setup.root_depth);
    write_u16(&mut out, 10);
    write_u32(&mut out, window_id);
    write_u32(&mut out, setup.root);
    write_i16(&mut out, 80);
    write_i16(&mut out, 80);
    write_u16(&mut out, 800);
    write_u16(&mut out, 600);
    write_u16(&mut out, 0);
    write_u16(&mut out, 1);
    write_u32(&mut out, setup.root_visual);
    write_u32(&mut out, (1 << 1) | (1 << 11));
    write_u32(&mut out, setup.white_pixel);
    write_u32(&mut out, 0x0000_8000 | 0x0002_0000);
    stream.write_all(&out)
}

fn select_keyboard_events(stream: &mut UnixStream, window_id: u32) -> io::Result<()> {
    let mut out = Vec::new();
    out.push(2);
    out.push(0);
    write_u16(&mut out, 4);
    write_u32(&mut out, window_id);
    write_u32(&mut out, 1 << 11);
    // KeyPress | KeyRelease | StructureNotify
    write_u32(&mut out, (1 << 0) | (1 << 1) | (1 << 17));
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
            _ => continue,
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
