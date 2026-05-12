//! In-process server fixture for L1 alpha-invariant integration tests.
//!
//! Owns a `ServerState` plus a headless `KmsBackend` so paint and
//! protocol code can run end-to-end inside the test process. A.1a
//! lands the boot path; A.1b/c grow the surface (paint helpers, GPU
//! mirror readback). Until A.1c attaches Vulkan, paint paths that
//! require it short-circuit at the `vk.as_ref()` guards — fine for
//! A.1a's "fixture starts" assertion, deliberately unsuitable for
//! the alpha-invariant tests that come later.
//!
//! Modeled after the production boot sequence in `yserver::lib::run`:
//! construct the backend, pull fb dimensions + RANDR outputs, seed
//! `ServerState::with_randr_outputs`.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    os::unix::net::UnixStream,
    sync::{Arc, Mutex, atomic::AtomicU16},
};

use yserver::kms::KmsBackend;
use yserver_core::{
    backend::Backend,
    core_loop::process_request::{RequestOutcome, process_request},
    resources::{ARGB_VISUAL, ROOT_VISUAL, ROOT_WINDOW},
    server::{ClientState, ServerState},
};
use yserver_protocol::x11::{ClientByteOrder, ClientId, RequestHeader, ResourceId, SequenceNumber};

pub struct ServerFixture {
    pub state: ServerState,
    pub backend: Box<dyn Backend>,
    pub next_client_id: ClientId,
    pub next_sequence: u16,
    pub default_client: ClientId,
    /// Monotonic offset within `default_client`'s resource-id range
    /// used by `next_resource_id`. Starts at zero; helpers pre-increment.
    pub default_client_resource_offset: u32,
    pub last_error: Option<io::Error>,
    /// Peer (read) ends of each client's socketpair. Held so the
    /// writer side doesn't EPIPE on the first reply emit; tests that
    /// want to inspect replies pull these out by client id.
    pub peers: HashMap<u32, UnixStream>,
}

impl ServerFixture {
    /// Boot a fixture. Tries `KmsBackend::for_tests_with_vk()` first
    /// (real `VkContext` + ops command pool + staging buffer) so paint
    /// ops and `capture_window_mirror` work end-to-end; on Vulkan
    /// init failure, falls back to the vk-less `KmsBackend::for_tests()`.
    /// Tests that require GPU paint must therefore be marked
    /// `#[ignore = "needs live Vulkan ICD"]` so CI without a Vulkan
    /// driver can still run the fallback path. A default client is
    /// installed so A.1b paint helpers dispatch without an explicit ID.
    #[must_use]
    pub fn start() -> Self {
        let backend: Box<dyn Backend> = match KmsBackend::for_tests_with_vk() {
            Ok(b) => Box::new(b),
            Err(err) => {
                log::warn!("ServerFixture: vk init failed ({err}); falling back to headless");
                Box::new(KmsBackend::for_tests())
            }
        };
        let (fb_w, fb_h) = backend
            .as_any()
            .downcast_ref::<KmsBackend>()
            .expect("fixture backend is KmsBackend")
            .fb_dimensions();
        let randr_outputs = backend
            .as_any()
            .downcast_ref::<KmsBackend>()
            .expect("fixture backend is KmsBackend")
            .randr_outputs();
        let state = ServerState::with_randr_outputs(fb_w, fb_h, randr_outputs);
        let mut fix = Self {
            state,
            backend,
            next_client_id: ClientId(1),
            next_sequence: 0,
            default_client: ClientId(0),
            default_client_resource_offset: 0,
            last_error: None,
            peers: HashMap::new(),
        };
        let default = fix.install_client();
        fix.default_client = default;
        fix
    }

    /// X11 root window resource ID.
    #[must_use]
    pub fn root_window(&self) -> ResourceId {
        ROOT_WINDOW
    }

    /// Returns true iff the server advertises both the depth-24 root
    /// visual and the depth-32 ARGB visual seeded by `ResourceTable`.
    #[must_use]
    pub fn has_default_visuals(&self) -> bool {
        self.state.resources.visual(ROOT_VISUAL).is_some()
            && self.state.resources.visual(ARGB_VISUAL).is_some()
    }

    /// Install a new X11 client backed by a `UnixStream` pair. The
    /// peer end is retained so the writer half doesn't EPIPE on
    /// reply emission. Client resource-id space is allocated in
    /// 0x100_0000-wide chunks per client (base aligns with the
    /// production setup handshake's typical layout).
    pub fn install_client(&mut self) -> ClientId {
        let id = self.next_client_id;
        self.next_client_id = ClientId(id.0.checked_add(1).expect("client-id overflow"));
        let (writer_side, peer_side) = UnixStream::pair().expect("UnixStream::pair");
        let base = id.0.checked_shl(20).expect("client base overflow");
        self.state.clients.insert(
            id.0,
            ClientState {
                writer: Arc::new(Mutex::new(writer_side)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: base,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
                outbound: VecDeque::new(),
                watching_writable: false,
                focused_window: ROOT_WINDOW,
                reader_control: None,
            },
        );
        self.peers.insert(id.0, peer_side);
        id
    }

    /// Dispatch a single fully-encoded X11 request to `process_request`.
    /// `buf` is the wire bytes: 4-byte header followed by the body
    /// (no `BigRequests` extension; A.1a/A.1b tests fit in 16-bit length).
    /// Errors are stashed on the fixture (see `dispatched_without_error`)
    /// and also returned.
    pub fn dispatch_request(&mut self, client: ClientId, buf: &[u8]) -> io::Result<RequestOutcome> {
        assert!(buf.len() >= 4, "request buf shorter than 4-byte header");
        let header = RequestHeader {
            opcode: buf[0],
            data: buf[1],
            length_units: u32::from(u16::from_le_bytes([buf[2], buf[3]])),
        };
        let body = &buf[4..];
        self.next_sequence = self.next_sequence.wrapping_add(1);
        let seq = SequenceNumber(self.next_sequence);
        let result = process_request(
            &mut self.state,
            &mut *self.backend,
            client,
            seq,
            header,
            body,
            None,
        );
        if let Err(err) = &result {
            self.last_error = Some(io::Error::new(err.kind(), err.to_string()));
        }
        result
    }

    /// True iff every `dispatch_request` invocation so far returned `Ok`.
    /// A.1b's `fixture_can_fill_rectangle` test gates on this.
    #[must_use]
    pub fn dispatched_without_error(&self) -> bool {
        self.last_error.is_none()
    }

    /// Allocate a fresh resource ID inside `default_client`'s range.
    /// Offsets stay well clear of the server-reserved IDs in
    /// `crates/yserver-core/src/resources.rs:30..` (0x100..0x103).
    pub fn next_resource_id(&mut self) -> ResourceId {
        self.default_client_resource_offset = self
            .default_client_resource_offset
            .checked_add(1)
            .expect("resource-id offset overflow");
        let client = self
            .state
            .clients
            .get(&self.default_client.0)
            .expect("default client not installed");
        ResourceId(client.resource_id_base | self.default_client_resource_offset)
    }

    /// `CreateWindow` as a top-level child of root with `bg_pixel`
    /// set in the value list. value_mask = 0x02 (background_pixel).
    pub fn create_window_with_bg_pixel(
        &mut self,
        width: u16,
        height: u16,
        depth: u8,
        bg_pixel: u32,
    ) -> ResourceId {
        let window = self.next_resource_id();
        let visual = if depth == 32 {
            ARGB_VISUAL
        } else {
            ROOT_VISUAL
        };
        let mut buf = vec![0u8; 36];
        buf[0] = 1;
        buf[1] = depth;
        buf[2..4].copy_from_slice(&9u16.to_le_bytes());
        buf[4..8].copy_from_slice(&window.0.to_le_bytes());
        buf[8..12].copy_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        buf[12..14].copy_from_slice(&0i16.to_le_bytes());
        buf[14..16].copy_from_slice(&0i16.to_le_bytes());
        buf[16..18].copy_from_slice(&width.to_le_bytes());
        buf[18..20].copy_from_slice(&height.to_le_bytes());
        buf[20..22].copy_from_slice(&0u16.to_le_bytes()); // border_width
        buf[22..24].copy_from_slice(&1u16.to_le_bytes()); // class = InputOutput
        buf[24..28].copy_from_slice(&visual.0.to_le_bytes());
        buf[28..32].copy_from_slice(&0x0000_0002u32.to_le_bytes()); // value_mask = background_pixel
        buf[32..36].copy_from_slice(&bg_pixel.to_le_bytes());
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("create_window_with_bg_pixel dispatch");
        window
    }

    /// `CopyPlane` (opcode 63) — copy one bit plane from `src` to
    /// `dst`, expanded to foreground/background per the GC.
    #[allow(clippy::too_many_arguments)]
    pub fn copy_plane(
        &mut self,
        src: ResourceId,
        dst: ResourceId,
        gc: ResourceId,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        w: u16,
        h: u16,
        bit_plane: u32,
    ) {
        let mut buf = vec![0u8; 32];
        buf[0] = 63;
        buf[2..4].copy_from_slice(&8u16.to_le_bytes());
        buf[4..8].copy_from_slice(&src.0.to_le_bytes());
        buf[8..12].copy_from_slice(&dst.0.to_le_bytes());
        buf[12..16].copy_from_slice(&gc.0.to_le_bytes());
        buf[16..18].copy_from_slice(&src_x.to_le_bytes());
        buf[18..20].copy_from_slice(&src_y.to_le_bytes());
        buf[20..22].copy_from_slice(&dst_x.to_le_bytes());
        buf[22..24].copy_from_slice(&dst_y.to_le_bytes());
        buf[24..26].copy_from_slice(&w.to_le_bytes());
        buf[26..28].copy_from_slice(&h.to_le_bytes());
        buf[28..32].copy_from_slice(&bit_plane.to_le_bytes());
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("copy_plane dispatch");
    }

    /// `CopyArea` (opcode 62) — blit `(src_x, src_y, w, h)` from
    /// `src` to `(dst_x, dst_y)` on `dst`.
    pub fn copy_area(
        &mut self,
        src: ResourceId,
        dst: ResourceId,
        gc: ResourceId,
        src_x: i16,
        src_y: i16,
        dst_x: i16,
        dst_y: i16,
        w: u16,
        h: u16,
    ) {
        let mut buf = vec![0u8; 28];
        buf[0] = 62;
        buf[2..4].copy_from_slice(&7u16.to_le_bytes());
        buf[4..8].copy_from_slice(&src.0.to_le_bytes());
        buf[8..12].copy_from_slice(&dst.0.to_le_bytes());
        buf[12..16].copy_from_slice(&gc.0.to_le_bytes());
        buf[16..18].copy_from_slice(&src_x.to_le_bytes());
        buf[18..20].copy_from_slice(&src_y.to_le_bytes());
        buf[20..22].copy_from_slice(&dst_x.to_le_bytes());
        buf[22..24].copy_from_slice(&dst_y.to_le_bytes());
        buf[24..26].copy_from_slice(&w.to_le_bytes());
        buf[26..28].copy_from_slice(&h.to_le_bytes());
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("copy_area dispatch");
    }

    /// `ClearArea` (opcode 61). `exposures` is the header `data` byte.
    pub fn clear_area(
        &mut self,
        window: ResourceId,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        exposures: bool,
    ) {
        let mut buf = vec![0u8; 16];
        buf[0] = 61;
        buf[1] = u8::from(exposures);
        buf[2..4].copy_from_slice(&4u16.to_le_bytes());
        buf[4..8].copy_from_slice(&window.0.to_le_bytes());
        buf[8..10].copy_from_slice(&x.to_le_bytes());
        buf[10..12].copy_from_slice(&y.to_le_bytes());
        buf[12..14].copy_from_slice(&w.to_le_bytes());
        buf[14..16].copy_from_slice(&h.to_le_bytes());
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("clear_area dispatch");
    }

    /// `CreateWindow` as a top-level child of root. `depth = 32` picks
    /// `ARGB_VISUAL`; anything else uses `ROOT_VISUAL`. `value_mask=0`
    /// (defaults). Class is `InputOutput` (1).
    pub fn create_window(&mut self, width: u16, height: u16, depth: u8) -> ResourceId {
        let window = self.next_resource_id();
        let visual = if depth == 32 {
            ARGB_VISUAL
        } else {
            ROOT_VISUAL
        };
        let mut buf = vec![0u8; 32];
        buf[0] = 1; // CreateWindow opcode
        buf[1] = depth;
        buf[2..4].copy_from_slice(&8u16.to_le_bytes()); // length_units
        buf[4..8].copy_from_slice(&window.0.to_le_bytes());
        buf[8..12].copy_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        buf[12..14].copy_from_slice(&0i16.to_le_bytes()); // x
        buf[14..16].copy_from_slice(&0i16.to_le_bytes()); // y
        buf[16..18].copy_from_slice(&width.to_le_bytes());
        buf[18..20].copy_from_slice(&height.to_le_bytes());
        buf[20..22].copy_from_slice(&0u16.to_le_bytes()); // border_width
        buf[22..24].copy_from_slice(&1u16.to_le_bytes()); // class = InputOutput
        buf[24..28].copy_from_slice(&visual.0.to_le_bytes());
        buf[28..32].copy_from_slice(&0u32.to_le_bytes()); // value_mask
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("create_window dispatch");
        window
    }

    /// `MapWindow` on `win`.
    pub fn map_window(&mut self, win: ResourceId) {
        let mut buf = vec![0u8; 8];
        buf[0] = 8; // MapWindow opcode
        buf[2..4].copy_from_slice(&2u16.to_le_bytes());
        buf[4..8].copy_from_slice(&win.0.to_le_bytes());
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("map_window dispatch");
    }

    /// `CreateGC` with only the foreground pixel set (`value_mask`
    /// bit 2 = 0x4).
    pub fn create_gc(&mut self, drawable: ResourceId, fg_pixel: u32) -> ResourceId {
        let gc = self.next_resource_id();
        let mut buf = vec![0u8; 20];
        buf[0] = 55; // CreateGC opcode
        buf[2..4].copy_from_slice(&5u16.to_le_bytes());
        buf[4..8].copy_from_slice(&gc.0.to_le_bytes());
        buf[8..12].copy_from_slice(&drawable.0.to_le_bytes());
        buf[12..16].copy_from_slice(&0x0000_0004u32.to_le_bytes()); // mask: foreground
        buf[16..20].copy_from_slice(&fg_pixel.to_le_bytes());
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("create_gc dispatch");
        gc
    }

    /// `CreateGC` with `function` (u8 X11 GC function code, e.g.
    /// 6 = XOR) and `fg_pixel` set. value_mask bit 0 (function) +
    /// bit 2 (foreground) = 0x5.
    pub fn create_gc_with_function(
        &mut self,
        drawable: ResourceId,
        function: u8,
        fg_pixel: u32,
    ) -> ResourceId {
        let gc = self.next_resource_id();
        let mut buf = vec![0u8; 24];
        buf[0] = 55;
        buf[2..4].copy_from_slice(&6u16.to_le_bytes()); // length_units (1 header + 5 body words)
        buf[4..8].copy_from_slice(&gc.0.to_le_bytes());
        buf[8..12].copy_from_slice(&drawable.0.to_le_bytes());
        buf[12..16].copy_from_slice(&0x0000_0005u32.to_le_bytes()); // mask: function | foreground
        buf[16..20].copy_from_slice(&u32::from(function).to_le_bytes());
        buf[20..24].copy_from_slice(&fg_pixel.to_le_bytes());
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("create_gc_with_function dispatch");
        gc
    }

    /// `PolyFillRectangle` with a single rectangle.
    pub fn fill_rectangle(
        &mut self,
        drawable: ResourceId,
        gc: ResourceId,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
    ) {
        let mut buf = vec![0u8; 20];
        buf[0] = 70; // PolyFillRectangle opcode
        buf[2..4].copy_from_slice(&5u16.to_le_bytes());
        buf[4..8].copy_from_slice(&drawable.0.to_le_bytes());
        buf[8..12].copy_from_slice(&gc.0.to_le_bytes());
        buf[12..14].copy_from_slice(&x.to_le_bytes());
        buf[14..16].copy_from_slice(&y.to_le_bytes());
        buf[16..18].copy_from_slice(&w.to_le_bytes());
        buf[18..20].copy_from_slice(&h.to_le_bytes());
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("fill_rectangle dispatch");
    }

    /// Read back the Vulkan mirror for `win`. Panics if the fixture
    /// wasn't booted with Vulkan or if the window has no host XID
    /// (e.g. dispatch was never routed through `process_request`).
    /// Used by the L1 alpha-invariant tests starting in A.3.
    #[must_use]
    pub fn capture_window_mirror(&mut self, win: ResourceId) -> ImageRgba8 {
        let host_xid = self
            .state
            .resources
            .window(win)
            .and_then(|w| w.host_xid)
            .expect("window has no host_xid — was it created through process_request?")
            .as_raw();
        self.capture_mirror_by_host_xid(host_xid)
    }

    /// Read back the Vulkan mirror for a pixmap.
    #[must_use]
    pub fn capture_pixmap_mirror(&mut self, pix: ResourceId) -> ImageRgba8 {
        let host_xid = self
            .state
            .resources
            .pixmap(pix)
            .and_then(|p| p.host_xid)
            .expect("pixmap has no host_xid — was it created through process_request?")
            .as_raw();
        self.capture_mirror_by_host_xid(host_xid)
    }

    fn capture_mirror_by_host_xid(&mut self, host_xid: u32) -> ImageRgba8 {
        let kms = self
            .backend
            .as_any_mut()
            .downcast_mut::<KmsBackend>()
            .expect("fixture backend is KmsBackend");
        let (width, height, bgra) = kms
            .capture_mirror_bgra8(host_xid)
            .expect("capture_mirror_bgra8 returned None — was the fixture booted with Vulkan?");
        ImageRgba8 {
            width,
            height,
            bgra,
        }
    }

    /// `CreatePixmap` of `(width, height, depth)` against the root
    /// window as the reference drawable. Returns the fresh pixmap ID.
    pub fn create_pixmap(&mut self, width: u16, height: u16, depth: u8) -> ResourceId {
        let pix = self.next_resource_id();
        let mut buf = vec![0u8; 16];
        buf[0] = 53; // CreatePixmap opcode
        buf[1] = depth;
        buf[2..4].copy_from_slice(&4u16.to_le_bytes()); // length_units
        buf[4..8].copy_from_slice(&pix.0.to_le_bytes());
        buf[8..12].copy_from_slice(&ROOT_WINDOW.0.to_le_bytes());
        buf[12..14].copy_from_slice(&width.to_le_bytes());
        buf[14..16].copy_from_slice(&height.to_le_bytes());
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("create_pixmap dispatch");
        pix
    }

    /// Sugar: create a transient GC with `fg_pixel` as foreground,
    /// then `PolyFillRectangle` with a single rect. Used by the
    /// alpha-invariant tests where the GC lifetime is irrelevant.
    pub fn fill_rectangle_simple(
        &mut self,
        drawable: ResourceId,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        fg_pixel: u32,
    ) {
        let gc = self.create_gc(drawable, fg_pixel);
        self.fill_rectangle(drawable, gc, x, y, w, h);
    }

    /// `PolyRectangle` (opcode 67) — draw a 1-pixel outline.
    pub fn poly_rectangle(
        &mut self,
        drawable: ResourceId,
        gc: ResourceId,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
    ) {
        let mut buf = vec![0u8; 20];
        buf[0] = 67;
        buf[2..4].copy_from_slice(&5u16.to_le_bytes());
        buf[4..8].copy_from_slice(&drawable.0.to_le_bytes());
        buf[8..12].copy_from_slice(&gc.0.to_le_bytes());
        buf[12..14].copy_from_slice(&x.to_le_bytes());
        buf[14..16].copy_from_slice(&y.to_le_bytes());
        buf[16..18].copy_from_slice(&w.to_le_bytes());
        buf[18..20].copy_from_slice(&h.to_le_bytes());
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("poly_rectangle dispatch");
    }

    /// `PolyLine` (opcode 65) — polyline through `points`,
    /// coordinate_mode = Origin (0).
    pub fn poly_line(&mut self, drawable: ResourceId, gc: ResourceId, points: &[(i16, i16)]) {
        let header_len = 4 + 4 + 4;
        let body_len = header_len + points.len() * 4;
        let total = body_len; // already includes the 4-byte X11 header
        assert!(
            total.is_multiple_of(4),
            "request length must be a multiple of 4"
        );
        let units = u16::try_from(total / 4).expect("request too long");
        let mut buf = vec![0u8; total];
        buf[0] = 65;
        buf[1] = 0; // coordinate_mode = Origin
        buf[2..4].copy_from_slice(&units.to_le_bytes());
        buf[4..8].copy_from_slice(&drawable.0.to_le_bytes());
        buf[8..12].copy_from_slice(&gc.0.to_le_bytes());
        for (i, (px, py)) in points.iter().enumerate() {
            let off = 12 + i * 4;
            buf[off..off + 2].copy_from_slice(&px.to_le_bytes());
            buf[off + 2..off + 4].copy_from_slice(&py.to_le_bytes());
        }
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("poly_line dispatch");
    }

    /// `PolySegment` (opcode 66) — independent line segments.
    pub fn poly_segment(
        &mut self,
        drawable: ResourceId,
        gc: ResourceId,
        segments: &[(i16, i16, i16, i16)],
    ) {
        let total = 4 + 4 + 4 + segments.len() * 8;
        assert!(total.is_multiple_of(4));
        let units = u16::try_from(total / 4).expect("request too long");
        let mut buf = vec![0u8; total];
        buf[0] = 66;
        buf[2..4].copy_from_slice(&units.to_le_bytes());
        buf[4..8].copy_from_slice(&drawable.0.to_le_bytes());
        buf[8..12].copy_from_slice(&gc.0.to_le_bytes());
        for (i, (x1, y1, x2, y2)) in segments.iter().enumerate() {
            let off = 12 + i * 8;
            buf[off..off + 2].copy_from_slice(&x1.to_le_bytes());
            buf[off + 2..off + 4].copy_from_slice(&y1.to_le_bytes());
            buf[off + 4..off + 6].copy_from_slice(&x2.to_le_bytes());
            buf[off + 6..off + 8].copy_from_slice(&y2.to_le_bytes());
        }
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("poly_segment dispatch");
    }

    /// `PolyPoint` (opcode 64) — point list, coordinate_mode = Origin.
    pub fn poly_point(&mut self, drawable: ResourceId, gc: ResourceId, points: &[(i16, i16)]) {
        let total = 4 + 4 + 4 + points.len() * 4;
        assert!(total.is_multiple_of(4));
        let units = u16::try_from(total / 4).expect("request too long");
        let mut buf = vec![0u8; total];
        buf[0] = 64;
        buf[1] = 0; // coordinate_mode = Origin
        buf[2..4].copy_from_slice(&units.to_le_bytes());
        buf[4..8].copy_from_slice(&drawable.0.to_le_bytes());
        buf[8..12].copy_from_slice(&gc.0.to_le_bytes());
        for (i, (px, py)) in points.iter().enumerate() {
            let off = 12 + i * 4;
            buf[off..off + 2].copy_from_slice(&px.to_le_bytes());
            buf[off + 2..off + 4].copy_from_slice(&py.to_le_bytes());
        }
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("poly_point dispatch");
    }

    /// One arc encoded for `PolyArc` / `PolyFillArc`. `angle1` and
    /// `angle2` are in 64ths of a degree per X11 convention; a full
    /// circle is `angle2 = 360 * 64 = 23040`.
    fn encode_arc(
        opcode: u8,
        drawable: ResourceId,
        gc: ResourceId,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        angle1: i16,
        angle2: i16,
    ) -> Vec<u8> {
        let total = 4 + 4 + 4 + 12;
        let units = u16::try_from(total / 4).expect("request too long");
        let mut buf = vec![0u8; total];
        buf[0] = opcode;
        buf[2..4].copy_from_slice(&units.to_le_bytes());
        buf[4..8].copy_from_slice(&drawable.0.to_le_bytes());
        buf[8..12].copy_from_slice(&gc.0.to_le_bytes());
        buf[12..14].copy_from_slice(&x.to_le_bytes());
        buf[14..16].copy_from_slice(&y.to_le_bytes());
        buf[16..18].copy_from_slice(&w.to_le_bytes());
        buf[18..20].copy_from_slice(&h.to_le_bytes());
        buf[20..22].copy_from_slice(&angle1.to_le_bytes());
        buf[22..24].copy_from_slice(&angle2.to_le_bytes());
        buf
    }

    /// `PolyArc` (opcode 68) — stroke a single full circle.
    pub fn poly_arc(
        &mut self,
        drawable: ResourceId,
        gc: ResourceId,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
    ) {
        let buf = Self::encode_arc(68, drawable, gc, x, y, w, h, 0, 23040);
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("poly_arc dispatch");
    }

    /// `PolyFillArc` (opcode 71) — fill a single full circle.
    pub fn poly_fill_arc(
        &mut self,
        drawable: ResourceId,
        gc: ResourceId,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
    ) {
        let buf = Self::encode_arc(71, drawable, gc, x, y, w, h, 0, 23040);
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("poly_fill_arc dispatch");
    }

    /// `PutImage` (opcode 72) ZPixmap of `data` into `drawable` at
    /// `(dst_x, dst_y)`. `data` is the X11 ZPixmap wire payload —
    /// for depth 24/32 that's `width * height` 4-byte BGRA-ish
    /// pixels (X11 actually uses `[r, g, b, a]` byte order on the
    /// wire; the backend permutes to the mirror's BGRA layout).
    /// Pad to a 4-byte boundary; X11 wire requires it.
    pub fn put_image_zpixmap(
        &mut self,
        drawable: ResourceId,
        gc: ResourceId,
        depth: u8,
        dst_x: i16,
        dst_y: i16,
        width: u16,
        height: u16,
        data: &[u8],
    ) {
        let data_padded = (data.len() + 3) & !3;
        let total = 4 + 20 + data_padded;
        assert!(total.is_multiple_of(4));
        let units = u16::try_from(total / 4).expect("request too long");
        let mut buf = vec![0u8; total];
        buf[0] = 72;
        buf[1] = 2; // format = ZPixmap
        buf[2..4].copy_from_slice(&units.to_le_bytes());
        buf[4..8].copy_from_slice(&drawable.0.to_le_bytes());
        buf[8..12].copy_from_slice(&gc.0.to_le_bytes());
        buf[12..14].copy_from_slice(&width.to_le_bytes());
        buf[14..16].copy_from_slice(&height.to_le_bytes());
        buf[16..18].copy_from_slice(&dst_x.to_le_bytes());
        buf[18..20].copy_from_slice(&dst_y.to_le_bytes());
        // buf[20] = body[16] = left_pad (0); buf[21] = depth.
        buf[21] = depth;
        // buf[22..24] = 2-byte pad — already zero.
        buf[24..24 + data.len()].copy_from_slice(data);
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("put_image_zpixmap dispatch");
    }

    /// `FillPoly` (opcode 69) — fill a polygon with absolute vertex
    /// coordinates (`coordinate_mode = Origin`) and `shape = Complex`.
    pub fn fill_poly(&mut self, drawable: ResourceId, gc: ResourceId, points: &[(i16, i16)]) {
        let total = 4 + 4 + 4 + 4 + points.len() * 4;
        assert!(total.is_multiple_of(4));
        let units = u16::try_from(total / 4).expect("request too long");
        let mut buf = vec![0u8; total];
        buf[0] = 69;
        buf[2..4].copy_from_slice(&units.to_le_bytes());
        buf[4..8].copy_from_slice(&drawable.0.to_le_bytes());
        buf[8..12].copy_from_slice(&gc.0.to_le_bytes());
        buf[12] = 0; // shape = Complex
        buf[13] = 0; // coordinate_mode = Origin
        for (i, (px, py)) in points.iter().enumerate() {
            let off = 16 + i * 4;
            buf[off..off + 2].copy_from_slice(&px.to_le_bytes());
            buf[off + 2..off + 4].copy_from_slice(&py.to_le_bytes());
        }
        let client = self.default_client;
        self.dispatch_request(client, &buf)
            .expect("fill_poly dispatch");
    }

    /// Read back the current scanout framebuffer. **Not yet wired** —
    /// landed signature-only in A.1c; the implementation requires a
    /// test-side scanout BO pool and lands with A.16 when the first
    /// pass-through test demands it (see plan A.16's
    /// `composite_passes_unpainted_pixels_through_to_root`).
    #[must_use]
    pub fn capture_scanout(&mut self) -> ImageRgba8 {
        unimplemented!(
            "capture_scanout requires test-side scanout BO setup; \
             lands with composite plan task A.16"
        )
    }
}

/// Mirror / scanout snapshot. Pixels are stored in their native
/// `B8G8R8A8_UNORM` layout; `pixel(x, y)` translates to RGBA so test
/// assertions can read channels in the order the X11 spec talks about.
pub struct ImageRgba8 {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl ImageRgba8 {
    /// `(width, height)` of the snapshot.
    #[must_use]
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Sample the pixel at `(x, y)`. Panics on out-of-range
    /// coordinates — tests should assert against known coords.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Rgba8 {
        assert!(x < self.width && y < self.height, "pixel oob");
        let off = ((y * self.width + x) * 4) as usize;
        Rgba8 {
            b: self.bgra[off],
            g: self.bgra[off + 1],
            r: self.bgra[off + 2],
            a: self.bgra[off + 3],
        }
    }
}
