use std::env;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

const MIT_MAGIC_COOKIE: &str = "MIT-MAGIC-COOKIE-1";

pub struct HostX11 {
    stream: UnixStream,
    window_id: u32,
    gc_id: u32,
    current_foreground: u32,
    sequence: u16,
}

impl HostX11 {
    pub fn open_from_env() -> io::Result<Self> {
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

        let setup = read_setup_reply(&mut stream)?;
        let window_id = setup.resource_id_base;
        let gc_id = setup.resource_id_base + 1;
        create_window(&mut stream, &setup, window_id)?;
        create_gc(&mut stream, window_id, gc_id, setup.black_pixel)?;
        map_window(&mut stream, window_id)?;
        stream.flush()?;

        Ok(Self {
            stream,
            window_id,
            gc_id,
            current_foreground: setup.black_pixel,
            sequence: 4,
        })
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
        self.stream.read_exact(&mut reply)?;
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

    pub fn clear(&mut self) -> io::Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        let mut out = Vec::new();
        out.push(61);
        out.push(0);
        write_u16(&mut out, 4);
        write_u32(&mut out, self.window_id);
        write_i16(&mut out, 0);
        write_i16(&mut out, 0);
        write_u16(&mut out, 0);
        write_u16(&mut out, 0);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    pub fn poly_fill_arc(&mut self, foreground: u32, arcs: &[u8]) -> io::Result<()> {
        self.draw_arcs(71, foreground, arcs)
    }

    pub fn poly_arc(&mut self, foreground: u32, arcs: &[u8]) -> io::Result<()> {
        self.draw_arcs(68, foreground, arcs)
    }

    pub fn poly_fill_rectangle(&mut self, foreground: u32, rectangles: &[u8]) -> io::Result<()> {
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
        out.push(70);
        out.push(0);
        write_u16(&mut out, length_units);
        write_u32(&mut out, self.window_id);
        write_u32(&mut out, self.gc_id);
        out.extend_from_slice(rectangles);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    fn draw_arcs(&mut self, opcode: u8, foreground: u32, arcs: &[u8]) -> io::Result<()> {
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
        write_u32(&mut out, self.window_id);
        write_u32(&mut out, self.gc_id);
        out.extend_from_slice(arcs);
        self.stream.write_all(&out)?;
        self.stream.flush()
    }

    fn change_foreground(&mut self, foreground: u32) -> io::Result<()> {
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

fn create_gc(
    stream: &mut UnixStream,
    drawable: u32,
    gc_id: u32,
    foreground: u32,
) -> io::Result<()> {
    let mut out = Vec::new();
    out.push(55);
    out.push(0);
    write_u16(&mut out, 5);
    write_u32(&mut out, gc_id);
    write_u32(&mut out, drawable);
    write_u32(&mut out, 1 << 2);
    write_u32(&mut out, foreground);
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
    while out.len() % 4 != 0 {
        out.push(0);
    }
}
