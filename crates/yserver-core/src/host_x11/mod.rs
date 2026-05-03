mod pump;
mod request;
mod sequence_map;
mod trait_impl;

pub use pump::{
    HostConfigureEvent, HostEvent, HostExposeEvent, HostKeyEvent, HostPointerEvent,
    HostSubwindowConfig, PointerEventKind, PointerPosition,
};

use std::{
    collections::HashMap,
    io::{self, ErrorKind, Read, Write},
    os::unix::net::UnixStream,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use log::debug;
use yserver_protocol::x11::ResourceId;

use crate::backend::{BackendEvent, BackendEventSink, OriginContext, PixmapHandle, WindowHandle};

use pump::{HostSetup, connect_to_host, decode_host_event, read_setup_reply};
use sequence_map::SequenceMap;

pub(super) const POINTER_EVENT_MASK: u32 = 0x0000_0004 // ButtonPress
    | 0x0000_0008 // ButtonRelease
    | 0x0000_0010 // EnterWindow
    | 0x0000_0020 // LeaveWindow
    | 0x0000_0040 // PointerMotion
    | 0x0000_8000; // Exposure

pub(super) const SUBWINDOW_EVENT_MASK: u32 = 0x0000_8000; // Exposure

pub(super) enum ResponseMatch {
    Return,
    Buffer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct HostError {
    pub code: u8,
    pub sequence: u16,
    pub sequence_full: u64,
    pub major_opcode: u8,
    pub minor_opcode: u16,
    pub bad_value: u32,
}

/// Producer is the dispatcher thread (or, before the dispatcher is up,
/// the synchronous `read_until_response` path used during init). It
/// inserts a response and signals the Condvar. Consumer is the request
/// handler waiting for a specific 64-bit sequence in `wait_for_reply`.
///
/// The internal `Mutex<PendingRepliesState>` is a *separate* lock from
/// the `Backend` mutex — by design, so the dispatcher can deliver
/// replies (and wake waiters) while a request handler is still holding
/// the Backend lock. Without that separation, the dispatcher would
/// always block on the Backend lock during writes, and `wait_for_reply`
/// would deadlock against itself.
///
/// `disconnected` flips when the host connection drops; pending waiters
/// are then woken with an EOF error rather than hanging forever.
pub(super) struct PendingReplies {
    state: Mutex<PendingRepliesState>,
    ready: Condvar,
}

struct PendingRepliesState {
    replies: SequenceMap<HostResponse>,
    disconnected: bool,
}

impl PendingReplies {
    fn new(max_window: u64) -> Self {
        Self {
            state: Mutex::new(PendingRepliesState {
                replies: SequenceMap::new(max_window),
                disconnected: false,
            }),
            ready: Condvar::new(),
        }
    }

    pub(super) fn insert(&self, response: HostResponse) {
        let mut state = self.state.lock().expect("pending replies mutex poisoned");
        state.replies.insert(response.sequence_full, response);
        self.ready.notify_all();
    }

    pub(super) fn take(&self, seq_full: u64) -> Option<HostResponse> {
        let mut state = self.state.lock().expect("pending replies mutex poisoned");
        state.replies.take(seq_full)
    }

    /// Block until either `seq_full` is available or the host connection
    /// drops. The Backend mutex MAY be held by the caller — the dispatcher
    /// only ever locks `PendingReplies::state`, never the Backend mutex,
    /// so there's no inversion.
    pub(super) fn wait_for(&self, seq_full: u64) -> io::Result<HostResponse> {
        let mut state = self.state.lock().expect("pending replies mutex poisoned");
        loop {
            if let Some(response) = state.replies.take(seq_full) {
                return Ok(response);
            }
            if state.disconnected {
                return Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "host connection closed before reply arrived",
                ));
            }
            state = self
                .ready
                .wait(state)
                .expect("pending replies condvar poisoned");
        }
    }

    /// Mark the host stream as disconnected and wake every waiter. After
    /// this the next `wait_for` returns `UnexpectedEof` immediately.
    pub(super) fn disconnect(&self) {
        let mut state = self.state.lock().expect("pending replies mutex poisoned");
        state.disconnected = true;
        self.ready.notify_all();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.state
            .lock()
            .expect("pending replies mutex poisoned")
            .replies
            .len()
    }
}

/// Sliding-window cache of host errors that arrived without a waiter in
/// `pending_replies`. Kept around so a synchronous reader can still pick
/// up an error for an awaited request that the legacy single-threaded
/// path queued before `wait_for_reply` was called. Once the dispatcher is
/// the canonical producer, errors land in `pending_replies` directly and
/// this map ends up mostly empty — but it's still consulted as a
/// fallback.
pub(super) struct PendingErrors {
    state: Mutex<PendingErrorsState>,
    ready: Condvar,
}

struct PendingErrorsState {
    errors: SequenceMap<HostError>,
}

impl PendingErrors {
    fn new(max_window: u64) -> Self {
        Self {
            state: Mutex::new(PendingErrorsState {
                errors: SequenceMap::new(max_window),
            }),
            ready: Condvar::new(),
        }
    }

    pub(super) fn insert(&self, error: HostError) {
        let mut state = self.state.lock().expect("pending errors mutex poisoned");
        state.errors.insert(error.sequence_full, error);
        self.ready.notify_all();
    }

    pub(super) fn take(&self, seq_full: u64) -> Option<HostError> {
        let mut state = self.state.lock().expect("pending errors mutex poisoned");
        state.errors.take(seq_full)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.state
            .lock()
            .expect("pending errors mutex poisoned")
            .errors
            .len()
    }
}

impl HostError {
    pub(super) fn from_response(response: &HostResponse) -> Option<Self> {
        if response.bytes[0] != 0 {
            return None;
        }
        Some(Self {
            code: response.bytes[1],
            sequence: response.sequence,
            sequence_full: response.sequence_full,
            bad_value: read_u32(&response.bytes[4..8]),
            minor_opcode: read_u16(&response.bytes[8..10]),
            major_opcode: response.bytes[10],
        })
    }

    pub(super) fn into_io_error(self, request_name: &str) -> io::Error {
        io::Error::other(format!(
            "host {request_name} failed (error {} major={} minor={} bad=0x{:x} seq={} seq_full={})",
            self.code,
            self.major_opcode,
            self.minor_opcode,
            self.bad_value,
            self.sequence,
            self.sequence_full,
        ))
    }
}

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

pub struct HostX11Backend {
    stream: UnixStream,
    window_id: u32,
    gc_id: u32,
    current_foreground: u32,
    current_background: u32,
    current_clip: HostClipState,
    current_fill: HostFillState,
    /// Phase 6.2 additive scope: cached values on the host's shared GC
    /// for the GC attributes that yserver-core now forwards. Setting
    /// these on first use means we don't re-issue identical ChangeGC's.
    /// Using `None` for the initial value means "host default, don't
    /// know exact byte" — the first non-default request will issue a
    /// ChangeGC unconditionally.
    current_function: Option<u8>,
    current_plane_mask: Option<u32>,
    current_line_width: Option<u16>,
    current_line_style: Option<u8>,
    current_cap_style: Option<u8>,
    current_join_style: Option<u8>,
    current_fill_rule: Option<u8>,
    current_subwindow_mode: Option<u8>,
    current_graphics_exposures: Option<bool>,
    current_dash_offset: Option<i16>,
    current_dashes: Option<Vec<u8>>,
    current_arc_mode: Option<u8>,
    active_origin: Option<OriginContext>,
    sequence: u16,
    next_seq_full: u64,
    next_xid_counter: u32,
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
    /// Host XID of the host root visual. Pushed into `ResourceTable` so
    /// that core CreateWindow forwarding for our `ROOT_VISUAL` resolves
    /// to a real host visual.
    root_visual_xid: u32,
    /// Host XID of an ARGB (32-bit TrueColor) visual on the host, if
    /// one was advertised at setup. `None` means we can't honour
    /// `ARGB_VISUAL` for top-level CreateWindow on this host.
    argb_visual_xid: Option<u32>,
    /// Host XID of a colormap allocated for `argb_visual_xid` during
    /// init. Required by `CreateWindow` whenever the child visual is
    /// not `CopyFromParent`.
    argb_colormap_xid: Option<u32>,
    // Replies read while waiting on some other request and matched by full
    // promoted sequence number. Async host errors are logged and dropped
    // instead of being buffered as if they were replies. The `Arc`
    // sharing lets the background dispatcher thread (`dispatch_loop`)
    // be the producer side without taking the Backend mutex.
    pending_replies: Arc<PendingReplies>,
    pending_errors: Arc<PendingErrors>,
    pending_origins: Arc<Mutex<SequenceMap<OriginContext>>>,
    /// Latest issued 64-bit sequence, mirrored to an atomic so the
    /// dispatcher thread can do 16→64 promotion without locking the
    /// Backend mutex. Set by every `issue_sequence` and `advance_sequence`
    /// after the local `next_seq_full` bumps.
    seq_full_atomic: Arc<AtomicU64>,
    /// Sender into the dispatcher → consumer channel. The dispatcher
    /// owns its own clone (passed at spawn time); this side stays around
    /// so `set_event_sink` / shutdown paths can synthesise events too.
    event_tx: Sender<BackendEvent>,
    event_rx: Option<Receiver<BackendEvent>>,
    host_event_masks: HashMap<u32, u32>,
    /// Tracks the consumer thread spawned by `set_event_sink`. The
    /// thread's lifetime is tied to the channel — when the last
    /// `Sender` drops the consumer exits its drain loop. Phase 6.3
    /// keeps the join handle so future tear-down can wait on it.
    sink_consumer: Option<thread::JoinHandle<()>>,
    /// Held alive for as long as the backend is alive. The thread reads
    /// the host stream and routes responses into `pending_replies` /
    /// `event_tx`. Phase 6.3 doesn't try to join it — process exit drops
    /// the connection which makes the `read_response` call return EOF.
    dispatcher: Option<thread::JoinHandle<()>>,
    /// `host_xid → ResourceId` lookup table consulted by the sink's
    /// pointer / expose fan-outs. Phase 6.3 Step 4 moves this off the
    /// (now-removed) pump handle and onto the backend itself so the
    /// dispatcher and the sink share the same Arc — adds remain O(1)
    /// and the read side is uncontended with the dispatcher's
    /// event-decode hot path.
    xid_map: HostXidMap,
    /// Phase 6.3 Step 4: per-client keyboard forwarders register a
    /// `Sender<HostKeyEvent>` here at connect-time so the dispatcher
    /// can fan KeyPress / KeyRelease out to every connected client.
    /// Each client's thread then applies its own focus state in
    /// `spawn_keyboard_forwarder`. The list is append-only — pruning
    /// disconnected senders happens lazily on the next add.
    key_subscribers: Arc<Mutex<Vec<Sender<HostKeyEvent>>>>,
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
pub(super) enum HostClipState {
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

/// Tracks the fill-style on the host shared GC so we don't re-issue
/// identical `ChangeGC(fill-style+tile)` calls. e16 paints popup
/// backgrounds via Tiled fill; the fill handlers must flip to Tiled
/// before the draw and back to Solid after, otherwise other clients'
/// later draws would inherit the tile pixmap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum HostFillState {
    Solid,
    Tiled {
        host_pixmap: u32,
        x_origin: i16,
        y_origin: i16,
    },
}

/// Visual / depth / colormap selector for [`HostX11Backend::create_subwindow`].
/// `CopyFromParent` is the historical path — depth=0, visual=0, no
/// colormap value — used when the requested child visual matches the
/// host container's visual. `Explicit` carries the host xids needed to
/// honour ARGB top-levels: the host requires both a real visual id and
/// a colormap whose visual matches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostSubwindowVisual {
    CopyFromParent,
    Explicit {
        depth: u8,
        visual_xid: u32,
        colormap_xid: u32,
    },
}

impl HostSubwindowVisual {
    pub(super) fn depth(self) -> u8 {
        match self {
            Self::CopyFromParent => 0,
            Self::Explicit { depth, .. } => depth,
        }
    }

    pub(super) fn visual_xid(self) -> u32 {
        match self {
            Self::CopyFromParent => 0,
            Self::Explicit { visual_xid, .. } => visual_xid,
        }
    }
}

pub type HostXidMap = Arc<Mutex<HashMap<u32, ResourceId>>>;

impl HostX11Backend {
    pub fn open_from_env(width: u16, height: u16) -> io::Result<Self> {
        let mut stream = connect_to_host()?;
        let setup = read_setup_reply(&mut stream)?;
        let window_id = setup.resource_id_base;
        let gc_id = setup.resource_id_base + 1;
        let font_id = setup.resource_id_base + 2;
        create_window(&mut stream, &setup, window_id, width, height)?;
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

        let pending_replies = Arc::new(PendingReplies::new(65_536));
        let pending_errors = Arc::new(PendingErrors::new(65_536));
        let pending_origins = Arc::new(Mutex::new(SequenceMap::new(65_536)));
        let seq_full_atomic = Arc::new(AtomicU64::new(5));
        let (event_tx, event_rx) = unbounded::<BackendEvent>();

        // Map the host container window to ynest's ROOT_WINDOW so that
        // Expose / pointer events on the container area route through
        // ROOT_WINDOW in `expose_event_fanout` / `pointer_event_fanout`.
        // Pre-Phase-6.3 this lived on the pump handle; the merged
        // dispatcher needs the same translation.
        let mut initial_xid_map = HashMap::new();
        initial_xid_map.insert(window_id, crate::resources::ROOT_WINDOW);

        let mut this = Self {
            stream,
            window_id,
            gc_id,
            current_foreground: setup.black_pixel,
            current_background: setup.white_pixel,
            current_clip: HostClipState::None,
            current_fill: HostFillState::Solid,
            current_function: None,
            current_plane_mask: None,
            current_line_width: None,
            current_line_style: None,
            current_cap_style: None,
            current_join_style: None,
            current_fill_rule: None,
            current_subwindow_mode: None,
            current_graphics_exposures: None,
            current_dash_offset: None,
            current_dashes: None,
            current_arc_mode: None,
            active_origin: None,
            sequence: 5,
            next_seq_full: 5,
            next_xid_counter: setup.resource_id_base + 3,
            render: None,
            xkb: None,
            shape_opcode: None,
            xfixes_opcode: None,
            composite_opcode: None,
            pending_replies,
            pending_errors,
            pending_origins,
            seq_full_atomic,
            event_tx,
            event_rx: Some(event_rx),
            host_event_masks: HashMap::new(),
            sink_consumer: None,
            dispatcher: None,
            xid_map: Arc::new(Mutex::new(initial_xid_map)),
            key_subscribers: Arc::new(Mutex::new(Vec::new())),
            depth_gcs: HashMap::new(),
            root_visual_xid: setup.root_visual,
            argb_visual_xid: setup.argb_visual,
            argb_colormap_xid: None,
        };
        // Init paths run synchronously on the main connection — the
        // dispatcher hasn't been spawned yet, so `read_until_response`
        // is still the canonical reader. Once these complete we hand
        // the read half to the dispatcher and never touch it from the
        // main thread again.
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
        if let Some(argb_visual) = this.argb_visual_xid {
            match this.create_argb_colormap(setup.root, argb_visual) {
                Ok(xid) => this.argb_colormap_xid = Some(xid),
                Err(err) => {
                    log::warn!(
                        "could not allocate host ARGB colormap (visual=0x{argb_visual:x}): {err}; \
                         ARGB CreateWindow will fall back to CopyFromParent"
                    );
                }
            }
        } else {
            log::info!("host advertises no depth-32 TrueColor visual — ARGB CreateWindow disabled");
        }
        // Spawn the dispatcher. The clone of the read half lets the
        // dispatcher block on `read_response` without contending with
        // the main thread's writes; the OS-level socket is one stream,
        // so flushes from the main thread still go to the same FD.
        let read_stream = this.stream.try_clone()?;
        let dispatcher = spawn_dispatch_thread(
            read_stream,
            Arc::clone(&this.pending_replies),
            Arc::clone(&this.pending_errors),
            Arc::clone(&this.pending_origins),
            Arc::clone(&this.seq_full_atomic),
            this.event_tx.clone(),
            Arc::clone(&this.key_subscribers),
        );
        this.dispatcher = Some(dispatcher);
        Ok(this)
    }

    /// Host XIDs the upper layer pushes into the visual / colormap
    /// tables in `ResourceTable`. `argb_*` are `None` when the host
    /// has no depth-32 TrueColor visual.
    pub fn root_visual_xid(&self) -> u32 {
        self.root_visual_xid
    }

    pub fn argb_visual_xid(&self) -> Option<u32> {
        self.argb_visual_xid
    }

    pub fn argb_colormap_xid(&self) -> Option<u32> {
        self.argb_colormap_xid
    }

    /// Allocate a host colormap for our ARGB visual via `XCreateColormap(
    /// alloc=None, mid, root, visual)`. Sent fire-and-forget — host errors
    /// (visual not depth-32, etc.) become async and are absorbed silently;
    /// the resulting xid is still returned but if the host failed, later
    /// CreateWindow attempts using it will surface a host BadColor / BadValue
    /// (also absorbed). This is acceptable here — the alternative is a
    /// blocking sync round-trip during HostX11Backend init.
    fn create_argb_colormap(&mut self, host_root: u32, argb_visual: u32) -> io::Result<u32> {
        let cmap_id = self.next_xid();
        let mut out = Vec::with_capacity(16);
        out.push(78); // CreateColormap opcode
        out.push(0); // alloc = None
        write_u16(&mut out, 4); // length = 4 words
        write_u32(&mut out, cmap_id);
        write_u32(&mut out, host_root);
        write_u32(&mut out, argb_visual);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.advance_sequence();
        Ok(cmap_id)
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
    /// host top-level. Allocates a fresh host pixmap XID and returns it.
    /// No reply is generated by the host.
    pub fn name_window_pixmap(&mut self, host_window: WindowHandle) -> io::Result<PixmapHandle> {
        let Some(major) = self.composite_opcode else {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "host COMPOSITE extension not available",
            ));
        };
        let host_pixmap = self.next_xid();
        // Wire layout: opcode(1) minor(1) length(2 = 3) window(4) pixmap(4)
        let mut out = [0u8; 12];
        out[0] = major;
        out[1] = yserver_protocol::x11::composite::NAME_WINDOW_PIXMAP;
        out[2..4].copy_from_slice(&3u16.to_le_bytes());
        out[4..8].copy_from_slice(&host_window.as_raw().to_le_bytes());
        out[8..12].copy_from_slice(&host_pixmap.to_le_bytes());
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.advance_sequence();
        Ok(PixmapHandle::from_raw_panicking(host_pixmap))
    }

    pub(super) fn next_xid(&mut self) -> u32 {
        let xid = self.next_xid_counter;
        self.next_xid_counter = self.next_xid_counter.wrapping_add(1);
        xid
    }

    pub(super) fn set_event_sink(&mut self, sink: Option<Box<dyn BackendEventSink>>) {
        // The dispatcher publishes BackendEvents into a crossbeam channel
        // up-front, regardless of whether anyone has registered a sink.
        // Setting a sink starts a consumer thread that drains the channel
        // and calls into the sink. Lock order: consumer never reaches
        // back into `HostX11Backend`, so we sidestep the inversion that
        // would deadlock if the sink call had to relock the Backend
        // mutex.
        match sink {
            Some(sink) => {
                if self.sink_consumer.is_some() {
                    log::warn!(
                        "set_event_sink called twice; ignoring the second call (Phase 6.3 lifecycle expects exactly one sink)"
                    );
                    return;
                }
                let Some(rx) = self.event_rx.take() else {
                    log::warn!("set_event_sink: no receiver available (already taken)");
                    return;
                };
                let mut sink = sink;
                let handle = thread::Builder::new()
                    .name("hostx11-sink".into())
                    .spawn(move || {
                        // `recv` returns Err only when every Sender has
                        // dropped — i.e. backend tear-down. That's our
                        // exit signal; nothing else to clean up.
                        for event in rx.iter() {
                            sink.handle_backend_event(event);
                        }
                    })
                    .expect("hostx11-sink thread spawn");
                self.sink_consumer = Some(handle);
            }
            None => {
                // Stop accepting new sinks for the remainder of the
                // backend's life. Existing consumer (if any) keeps
                // running; tearing it down requires dropping the
                // backend itself.
                self.event_rx = None;
            }
        }
    }

    /// Best-effort send into the dispatcher → consumer channel. The
    /// channel is unbounded, so the only failure mode is "the consumer
    /// thread has dropped its receiver" — which we treat as "no sink",
    /// matching the pre-Phase-6.3 behaviour.
    fn emit_backend_event(&self, event: BackendEvent) {
        if let Err(err) = self.event_tx.send(event) {
            log::trace!("backend event channel closed: {err}");
        }
    }

    pub(super) fn update_host_event_mask(
        &mut self,
        host_xid: u32,
        mask: u32,
        enabled: bool,
    ) -> u32 {
        let mut current = self.host_event_masks.get(&host_xid).copied().unwrap_or(0);
        if enabled {
            current |= mask;
        } else {
            current &= !mask;
        }
        if current == 0 {
            self.host_event_masks.remove(&host_xid);
            0
        } else {
            self.host_event_masks.insert(host_xid, current);
            current
        }
    }

    /// Phase 6.3 Step 4: write the `event-mask` value-list bit on the
    /// host child via the merged main connection. Caller has already
    /// updated the registry through [`update_host_event_mask`]; this
    /// is the wire-side commit. Pre-Step-4 the per-pump pump-handle
    /// `register_*` issued the same write on a *separate* socket —
    /// folding it onto the main stream is the "Big Flip" core.
    pub(super) fn write_event_mask(&mut self, host_xid: u32, event_mask: u32) -> io::Result<()> {
        // ChangeWindowAttributes — value-mask bit 11 (event-mask).
        let mut out = Vec::with_capacity(16);
        out.push(2); // ChangeWindowAttributes opcode
        out.push(0);
        write_u16(&mut out, 4);
        write_u32(&mut out, host_xid);
        write_u32(&mut out, 1 << 11);
        write_u32(&mut out, event_mask);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        self.advance_sequence();
        Ok(())
    }

    /// Phase 6.3 Step 4: clone of the shared `xid_map`. The dispatcher
    /// hands one clone to the sink (so `pointer_event_fanout` /
    /// `expose_event_fanout` can resolve `host_xid → ResourceId`), and
    /// `register_top_level` / `register_subwindow` mutate it through
    /// the same Arc.
    pub(super) fn xid_map(&self) -> HostXidMap {
        Arc::clone(&self.xid_map)
    }

    /// Phase 6.3 Step 4: per-client kb forwarder calls this once at
    /// connect time to receive every host KeyPress / KeyRelease the
    /// dispatcher decodes. The forwarder thread applies its own focus
    /// state on the events it receives; the dispatcher fans the same
    /// event out to every subscriber (mirrors the pre-Phase-6.3
    /// "every kb pump sees every key event" shape, just on one
    /// connection).
    ///
    /// Disconnected senders accumulate in the list — `crossbeam` only
    /// surfaces a closed channel via `send()`'s `Err`, which the
    /// dispatcher already discards. Periodic compaction is left for
    /// Step 6 cleanup; with 1–10 clients the list never grows past
    /// ~10 entries in practice.
    pub(super) fn add_key_subscriber(&mut self, tx: Sender<HostKeyEvent>) {
        let mut subs = self
            .key_subscribers
            .lock()
            .expect("key subscribers mutex poisoned");
        subs.push(tx);
    }

    /// Phase 6.3 Step 4: register a host top-level window so its
    /// pointer / expose events can be routed to `nested_id`. Replaces
    /// the deleted pump-handle `register_top_level` — same callers
    /// (CreateWindow on root parent, ReparentWindow into root) but
    /// now goes through the merged main connection.
    pub(super) fn register_top_level(
        &mut self,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        // Insert into the map *before* the wire write so any pointer
        // events arriving on this xid after the host commits the new
        // event-mask immediately resolve to the right ResourceId.
        if let Ok(mut map) = self.xid_map.lock() {
            map.insert(host_xid, nested_id);
        }
        let combined = self.update_host_event_mask(host_xid, POINTER_EVENT_MASK, true);
        self.write_event_mask(host_xid, combined)
    }

    /// Phase 6.3 Step 4: counterpart of [`register_top_level`] for
    /// sub-windows — registers `Exposure` only so pointer events bubble
    /// up to the top-level ancestor (X11 propagation rule), keeping
    /// dispatch on the top-level.
    pub(super) fn register_subwindow(
        &mut self,
        nested_id: ResourceId,
        host_xid: u32,
    ) -> io::Result<()> {
        if let Ok(mut map) = self.xid_map.lock() {
            map.insert(host_xid, nested_id);
        }
        let combined = self.update_host_event_mask(host_xid, SUBWINDOW_EVENT_MASK, true);
        self.write_event_mask(host_xid, combined)
    }

    /// Phase 6.3 Step 4: drop the `host_xid → ResourceId` mapping at
    /// DestroyWindow / Reparent-out. Errors are silent — the host
    /// child is about to disappear; clearing the registry purely
    /// prevents stale lookups in the dispatcher.
    pub(super) fn unregister_host_window(&mut self, host_xid: u32) {
        if let Ok(mut map) = self.xid_map.lock() {
            map.remove(&host_xid);
        }
        self.host_event_masks.remove(&host_xid);
    }

    #[cfg(test)]
    fn host_event_mask(&self, host_xid: u32) -> u32 {
        self.host_event_masks.get(&host_xid).copied().unwrap_or(0)
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn set_active_origin(&mut self, origin: Option<OriginContext>) {
        self.active_origin = origin;
    }

    pub(super) fn with_active_origin<R>(
        &mut self,
        origin: Option<OriginContext>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let previous = self.active_origin;
        self.active_origin = origin;
        let result = f(self);
        self.active_origin = previous;
        result
    }

    fn record_origin(&mut self, seq_full: u64) {
        if let Some(origin) = self.active_origin {
            self.pending_origins
                .lock()
                .expect("pending origins mutex poisoned")
                .insert(seq_full, origin);
        }
    }

    fn take_origin_for_sequence(&mut self, target_full: u64) -> Option<OriginContext> {
        self.pending_origins
            .lock()
            .expect("pending origins mutex poisoned")
            .take(target_full)
    }

    pub(super) fn issue_sequence(&mut self) -> (u16, u64) {
        let wire = self.sequence;
        let full = self.next_seq_full;
        self.record_origin(full);
        self.sequence = self.sequence.wrapping_add(1);
        self.next_seq_full = self.next_seq_full.wrapping_add(1);
        // Mirror the new value into the atomic so the dispatcher thread
        // sees the latest seq for 16→64 promotion. SeqCst keeps the
        // ordering simple; this is not a hot path (one store per request).
        self.seq_full_atomic
            .store(self.next_seq_full, Ordering::SeqCst);
        (wire, full)
    }

    pub(super) fn advance_sequence(&mut self) {
        let _ = self.issue_sequence();
    }

    fn promote_sequence(&self, wire: u16) -> u64 {
        promote_seq_from_atomic(self.next_seq_full, wire)
    }

    pub(super) fn read_host_response(&mut self) -> io::Result<HostResponse> {
        let mut response = read_response(&mut self.stream)?;
        response.sequence_full = self.promote_sequence(response.sequence);
        Ok(response)
    }

    pub(super) fn take_buffered_reply(&mut self, target_full: u64) -> Option<HostResponse> {
        let response = self.pending_replies.take(target_full)?;
        self.take_origin_for_sequence(target_full);
        Some(response)
    }

    /// Pre-dispatcher buffering helper used by `init_render`, `init_xkb`,
    /// and `query_extension_opcode`. After Phase 6.3 these run during
    /// `open_from_env` *before* the dispatcher is spawned, which is why
    /// they keep the synchronous read-loop shape.
    pub(super) fn stash_or_log_response(&mut self, response: HostResponse) {
        if response.bytes[0] == 0 {
            let origin = self.take_origin_for_sequence(response.sequence_full);
            let error = HostError::from_response(&response).expect("host error response");
            debug!(
                "host async error dropped: code={} major={} minor={} seq={} seq_full={} origin={origin:?}",
                error.code,
                error.major_opcode,
                error.minor_opcode,
                error.sequence,
                error.sequence_full,
            );
            self.emit_backend_event(BackendEvent::HostError {
                origin,
                error: error.into_io_error("async host request"),
            });
            self.pending_errors.insert(error);
            return;
        }
        self.take_origin_for_sequence(response.sequence_full);
        self.pending_replies.insert(response);
    }

    /// Phase 6.3 pathway: block on the `PendingReplies` Condvar until
    /// the dispatcher delivers `target_full`. The Backend mutex stays
    /// locked while we wait — that's safe because the dispatcher thread
    /// only locks `pending_replies.state`, not the Backend mutex (see
    /// the `PendingReplies` doc-comment).
    ///
    /// The synchronous fallback (`read_target_reply`) is preserved for
    /// the init phase when the dispatcher hasn't been spawned yet —
    /// `init_render`, `init_xkb`, and `query_extension_opcode` go
    /// through `read_until_response` directly. By the time the
    /// constructor returns, every subsequent reply waiter goes through
    /// the Condvar pathway.
    pub(super) fn wait_for_reply(
        &mut self,
        target_full: u64,
    ) -> io::Result<Result<Vec<u8>, HostError>> {
        // Fast path: dispatcher already produced the response.
        if let Some(response) = self.take_buffered_reply(target_full) {
            return Ok(decode_reply_or_error(response));
        }
        // Pre-dispatcher fallback: an error was queued in pending_errors
        // by `stash_or_log_response` during init. After init this map
        // stays empty, but we still consult it so the init-phase tests
        // and the legacy synchronous read path keep working.
        if let Some(error) = self.pending_errors.take(target_full) {
            self.take_origin_for_sequence(target_full);
            return Ok(Err(error));
        }
        if self.dispatcher.is_some() {
            // Background dispatcher will signal via the Condvar.
            let response = self.pending_replies.wait_for(target_full)?;
            self.take_origin_for_sequence(target_full);
            return Ok(decode_reply_or_error(response));
        }
        // No dispatcher yet (init path). Fall back to draining the
        // stream synchronously like the pre-Phase-6.3 code did.
        let response = self.read_until_response(|response| {
            if response.sequence_full == target_full {
                ResponseMatch::Return
            } else {
                ResponseMatch::Buffer
            }
        })?;
        Ok(decode_reply_or_error(response))
    }

    pub(super) fn read_until_response<F>(&mut self, mut matcher: F) -> io::Result<HostResponse>
    where
        F: FnMut(&HostResponse) -> ResponseMatch,
    {
        loop {
            let response = self.read_host_response()?;
            match matcher(&response) {
                ResponseMatch::Return => {
                    self.take_origin_for_sequence(response.sequence_full);
                    return Ok(response);
                }
                ResponseMatch::Buffer => self.stash_or_log_response(response),
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
        let (ext_seq, ext_seq_full) = self.issue_sequence();
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

        let resp = self.read_until_response(|response| {
            debug!(
                "init_render: got response byte0={} seq={}",
                response.bytes[0], response.sequence
            );
            if response.sequence_full == ext_seq_full {
                ResponseMatch::Return
            } else {
                ResponseMatch::Buffer
            }
        })?;
        if resp.bytes[8] == 0 {
            return Err(io::Error::other("host RENDER extension not present"));
        }
        let opcode = resp.bytes[9];
        debug!("init_render: RENDER present, opcode={}", opcode);

        let (fmt_seq, fmt_seq_full) = self.issue_sequence();
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

        let resp = self.read_until_response(|response| {
            debug!(
                "init_render: got response byte0={} seq={}",
                response.bytes[0], response.sequence
            );
            if response.sequence_full == fmt_seq_full {
                ResponseMatch::Return
            } else {
                ResponseMatch::Buffer
            }
        })?;
        let info = parse_host_pict_formats(&resp.bytes, opcode)?;
        debug!(
            "init_render: host formats a1=0x{:x} a8=0x{:x} rgb24=0x{:x} argb32=0x{:x}",
            info.fmt_a1, info.fmt_a8, info.fmt_rgb24, info.fmt_argb32
        );
        Ok(info)
    }

    fn init_xkb(&mut self) -> io::Result<HostXkbInfo> {
        let ext_name = b"XKEYBOARD";
        let padded = padded_len(ext_name.len());
        let length_units = 2 + (padded / 4) as u16;
        let (_ext_seq, ext_seq_full) = self.issue_sequence();
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

        let resp = self.read_until_response(|response| {
            if response.sequence_full == ext_seq_full {
                ResponseMatch::Return
            } else {
                ResponseMatch::Buffer
            }
        })?;
        if resp.bytes[8] == 0 {
            return Err(io::Error::other("host XKEYBOARD extension not present"));
        }
        let opcode = resp.bytes[9];
        let first_event = resp.bytes[10];
        let first_error = resp.bytes[11];

        // We also need to send UseExtension to the host for XKB to be fully functional.
        let (_use_seq, use_seq_full) = self.issue_sequence();
        let mut out = Vec::new();
        out.push(opcode);
        out.push(0); // UseExtension
        write_u16(&mut out, 2);
        write_u16(&mut out, 1); // want major 1
        write_u16(&mut out, 0); // want minor 0
        self.stream.write_all(&out)?;
        self.stream.flush()?;

        let resp = self.read_until_response(|response| {
            if response.sequence_full == use_seq_full {
                ResponseMatch::Return
            } else {
                ResponseMatch::Buffer
            }
        })?;
        if resp.bytes[8] == 0 {
            return Err(io::Error::other("host XKB UseExtension failed"));
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
        let (_ext_seq, ext_seq_full) = self.issue_sequence();
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
        let resp = self.read_until_response(|response| {
            if response.sequence_full == ext_seq_full {
                ResponseMatch::Return
            } else {
                ResponseMatch::Buffer
            }
        })?;
        if resp.bytes[8] == 0 {
            return Ok(None);
        }
        Ok(Some(resp.bytes[9]))
    }
}

fn create_window(
    stream: &mut UnixStream,
    setup: &HostSetup,
    window_id: u32,
    width: u16,
    height: u16,
) -> io::Result<()> {
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
    write_u16(&mut out, width);
    write_u16(&mut out, height);
    write_u16(&mut out, 0);
    write_u16(&mut out, 1);
    write_u32(&mut out, setup.root_visual);
    write_u32(&mut out, value_mask);
    write_u32(&mut out, setup.white_pixel); // bg-pixel
    write_u32(&mut out, 1); // bit-gravity = NorthWest
    // Phase 6.3 Step 4: container holds the *union* of every kind of
    // event we care about so the merged dispatcher sees them all on
    // one connection — KeyPress / KeyRelease (per-client kb fanout),
    // ButtonPress / ButtonRelease / EnterWindow / LeaveWindow /
    // PointerMotion (POINTER_EVENT_MASK), Exposure (root background
    // repaint), StructureNotify (container resize → RANDR fanout).
    let event_mask: u32 = CONTAINER_EVENT_MASK;
    write_u32(&mut out, event_mask);
    stream.write_all(&out)
}

/// Combined event-mask the host container window selects via
/// `CreateWindow`'s value-list. Phase 6.3 Step 4 unifies the masks
/// the deleted `HostInputPump`s used to select on three separate
/// connections — see the comment in [`create_window`].
const CONTAINER_EVENT_MASK: u32 = 0x0000_0001 // KeyPress
    | 0x0000_0002 // KeyRelease
    | 0x0000_0004 // ButtonPress
    | 0x0000_0008 // ButtonRelease
    | 0x0000_0010 // EnterWindow
    | 0x0000_0020 // LeaveWindow
    | 0x0000_0040 // PointerMotion
    | 0x0000_8000 // Exposure
    | 0x0002_0000; // StructureNotify

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

pub(super) fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

pub(super) fn read_i16(bytes: &[u8]) -> i16 {
    i16::from_le_bytes([bytes[0], bytes[1]])
}

pub(super) fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

pub(super) fn write_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn write_i16(out: &mut Vec<u8>, value: i16) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn padded_len(len: usize) -> usize {
    (len + 3) & !3
}

pub(super) fn pad4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
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

pub(super) struct HostResponse {
    sequence: u16,
    sequence_full: u64,
    bytes: Vec<u8>,
}

fn decode_reply_or_error(response: HostResponse) -> Result<Vec<u8>, HostError> {
    if let Some(error) = HostError::from_response(&response) {
        Err(error)
    } else {
        Ok(response.bytes)
    }
}

/// Promote a 16-bit wire sequence to 64-bit using the latest known
/// 64-bit counter. `next_full` is the value the *next* request will
/// take — i.e. one past the most recently issued sequence — so a wire
/// sequence equal to `next_full as u16` should resolve to
/// `next_full - (1 << 16)` because the host can't possibly have replied
/// to a not-yet-issued request.
///
/// The 32k sliding window in `SequenceMap` limits how stale a response
/// can be before it gets dropped on the floor; with 65k seq space we
/// have ~50% headroom before this promotion would alias.
pub(super) fn promote_seq_from_atomic(next_full: u64, wire: u16) -> u64 {
    let base = next_full & !0xffff;
    let mut candidate = base | u64::from(wire);
    if candidate >= next_full {
        candidate = candidate.saturating_sub(1 << 16);
    }
    candidate
}

/// Spawn the background dispatcher. Owns a clone of the read half of
/// the host UnixStream and is the *only* reader after `open_from_env`
/// returns. Loops:
///   1. Block on `read_response` until a reply (header[0]==1) or error
///      (header[0]==0) arrives. Events (header[0]>=2) are skipped at
///      the framing layer (`read_response`). Step 4 will flip that to
///      route events through the channel too.
///   2. Promote the 16-bit wire seq to 64-bit using `seq_full_atomic`.
///   3. For both replies and errors: insert into `pending_replies` and
///      signal the Condvar so any waiter in `wait_for_reply` wakes up.
///   4. For errors: ALSO emit `BackendEvent::HostError` to the channel,
///      with the `OriginContext` looked up from `pending_origins` (if
///      present). This handles the async-error case where no waiter is
///      blocked — the sink hears about it instead.
///
/// The thread exits on read error (host disconnected). It does NOT
/// touch the Backend mutex, so it can't deadlock against any request
/// handler that holds it.
fn spawn_dispatch_thread(
    mut read_stream: UnixStream,
    pending_replies: Arc<PendingReplies>,
    _pending_errors: Arc<PendingErrors>,
    pending_origins: Arc<Mutex<SequenceMap<OriginContext>>>,
    seq_full_atomic: Arc<AtomicU64>,
    event_tx: Sender<BackendEvent>,
    key_subscribers: Arc<Mutex<Vec<Sender<HostKeyEvent>>>>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("hostx11-dispatch".into())
        .spawn(move || {
            loop {
                let message = match read_dispatch_message(&mut read_stream) {
                    Ok(msg) => msg,
                    Err(err) => {
                        log::info!("hostx11-dispatch: connection closed ({err}), exiting");
                        pending_replies.disconnect();
                        return;
                    }
                };
                match message {
                    HostMessage::Skip => continue,
                    HostMessage::Event(event) => {
                        // Phase 6.3 Step 4: per-client kb subscribers
                        // get their own copy of every Key event so each
                        // client's `spawn_keyboard_forwarder` can apply
                        // its own focus state. The sink receives the
                        // same event via `event_tx` for completeness;
                        // sink fan-out drops Key events because routing
                        // them lives in the per-client thread.
                        if let HostEvent::Key(key) = event {
                            let subs = key_subscribers
                                .lock()
                                .expect("key subscribers mutex poisoned");
                            // Snapshot the current sender list — a
                            // disconnected client's Sender returns
                            // Err on send and we'd skip it without
                            // mutating the list here. Periodic
                            // pruning happens lazily via
                            // `register_key_subscriber`.
                            for tx in subs.iter() {
                                let _ = tx.send(key);
                            }
                            drop(subs);
                        }
                        let _ = event_tx.send(BackendEvent::HostEvent(event));
                        continue;
                    }
                    HostMessage::Response(mut response) => {
                        let next_full = seq_full_atomic.load(Ordering::SeqCst);
                        response.sequence_full =
                            promote_seq_from_atomic(next_full, response.sequence);

                        let is_error = response.bytes[0] == 0;
                        if is_error {
                            let error = HostError::from_response(&response)
                                .expect("error response missing header");
                            let origin = pending_origins
                                .lock()
                                .expect("pending origins mutex poisoned")
                                .take(response.sequence_full);
                            log::debug!(
                                "hostx11-dispatch: host error code={} major={} minor={} seq={} seq_full={} origin={origin:?}",
                                error.code,
                                error.major_opcode,
                                error.minor_opcode,
                                error.sequence,
                                error.sequence_full,
                            );
                            let _ = event_tx.send(BackendEvent::HostError {
                                origin,
                                error: error.into_io_error("async host request"),
                            });
                            // Also publish the response so any waiter in
                            // `wait_for_reply` wakes up. Reply consumers
                            // distinguish reply-vs-error by header[0].
                            pending_replies.insert(response);
                        } else {
                            pending_replies.insert(response);
                        }
                    }
                }
            }
        })
        .expect("hostx11-dispatch thread spawn")
}

pub(super) fn read_response(stream: &mut UnixStream) -> io::Result<HostResponse> {
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
    Ok(HostResponse {
        sequence,
        sequence_full: 0,
        bytes,
    })
}

/// Variant returned by the dispatcher's [`read_dispatch_message`]. The
/// merged main connection multiplexes replies/errors and asynchronous
/// events on the same stream — splitting them at the read boundary
/// keeps the dispatcher's branching shallow.
pub(super) enum HostMessage {
    Response(HostResponse),
    Event(HostEvent),
    /// Event class we don't care about (KeymapNotify, etc.) — drop
    /// silently. The dispatcher loops back to `read_dispatch_message`.
    Skip,
}

/// Phase 6.3 Step 4: replacement for `read_response` on the dispatcher
/// side. Reads the next 32-byte X11 header, decodes events to
/// `HostMessage::Event` (so the dispatcher can fan out to the sink and
/// per-client subscribers), and returns reply/error bodies as
/// `HostMessage::Response`.
///
/// `read_response` is preserved unchanged for the synchronous init
/// path (`read_until_response`) where any event arriving mid-init is
/// still dropped — the dispatcher hasn't been spawned yet, no sink is
/// listening, and `init_render` / `init_xkb` only care about replies.
pub(super) fn read_dispatch_message(stream: &mut UnixStream) -> io::Result<HostMessage> {
    let mut header = [0u8; 32];
    stream.read_exact(&mut header)?;
    match header[0] {
        0 => {
            // Error — fixed 32 bytes.
            let sequence = u16::from_le_bytes([header[2], header[3]]);
            let mut bytes = Vec::with_capacity(32);
            bytes.extend_from_slice(&header);
            Ok(HostMessage::Response(HostResponse {
                sequence,
                sequence_full: 0,
                bytes,
            }))
        }
        1 => {
            // Reply — header[4..8] is extra-length in 4-byte units.
            let sequence = u16::from_le_bytes([header[2], header[3]]);
            let extra =
                u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize * 4;
            let mut bytes = Vec::with_capacity(32 + extra);
            bytes.extend_from_slice(&header);
            if extra > 0 {
                let mut tail = vec![0u8; extra];
                stream.read_exact(&mut tail)?;
                bytes.extend_from_slice(&tail);
            }
            Ok(HostMessage::Response(HostResponse {
                sequence,
                sequence_full: 0,
                bytes,
            }))
        }
        35 => {
            // GenericEvent: skip the extra payload to keep stream aligned.
            // We don't currently surface these to the sink — XInput2 lives
            // on the per-client kb fanout below, not the host pump.
            let extra =
                u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize * 4;
            if extra > 0 {
                let mut tail = vec![0u8; extra];
                stream.read_exact(&mut tail)?;
            }
            Ok(HostMessage::Skip)
        }
        _ => {
            // Plain X11 event (KeyPress=2, KeyRelease=3, etc.). Decoder
            // returns None for event classes we ignore.
            match decode_host_event(&header) {
                Some(event) => Ok(HostMessage::Event(event)),
                None => Ok(HostMessage::Skip),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HostClipState, HostError, HostFillState, HostResponse, HostX11Backend, PendingErrors,
        PendingReplies, SequenceMap, promote_seq_from_atomic, read_u16,
    };
    use crate::backend::OriginContext;
    use std::{
        collections::HashMap,
        os::unix::net::UnixStream,
        sync::{Arc, Mutex, atomic::AtomicU64},
    };
    use yserver_protocol::x11::ClientId;

    fn dummy_backend() -> HostX11Backend {
        let (stream, _peer) = UnixStream::pair().expect("unix stream pair");
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        HostX11Backend {
            stream,
            window_id: 1,
            gc_id: 2,
            current_foreground: 0,
            current_background: 0,
            current_clip: HostClipState::None,
            current_fill: HostFillState::Solid,
            current_function: None,
            current_plane_mask: None,
            current_line_width: None,
            current_line_style: None,
            current_cap_style: None,
            current_join_style: None,
            current_fill_rule: None,
            current_subwindow_mode: None,
            current_graphics_exposures: None,
            current_dash_offset: None,
            current_dashes: None,
            current_arc_mode: None,
            active_origin: None,
            sequence: 0xfffe,
            next_seq_full: 0x1_fffe,
            next_xid_counter: 3,
            render: None,
            xkb: None,
            shape_opcode: None,
            xfixes_opcode: None,
            composite_opcode: None,
            root_visual_xid: 0,
            argb_visual_xid: None,
            argb_colormap_xid: None,
            pending_replies: Arc::new(PendingReplies::new(65_536)),
            pending_errors: Arc::new(PendingErrors::new(65_536)),
            pending_origins: Arc::new(Mutex::new(SequenceMap::new(65_536))),
            seq_full_atomic: Arc::new(AtomicU64::new(0x1_fffe)),
            event_tx,
            event_rx: Some(event_rx),
            host_event_masks: HashMap::new(),
            sink_consumer: None,
            dispatcher: None,
            xid_map: Arc::new(Mutex::new(HashMap::new())),
            key_subscribers: Arc::new(Mutex::new(Vec::new())),
            depth_gcs: HashMap::new(),
        }
    }

    #[test]
    fn promote_sequence_handles_wrap_against_latest_full_counter() {
        let mut backend = dummy_backend();
        let (wire0, full0) = backend.issue_sequence();
        let (wire1, full1) = backend.issue_sequence();
        assert_eq!((wire0, full0), (0xfffe, 0x1_fffe));
        assert_eq!((wire1, full1), (0xffff, 0x1_ffff));
        assert_eq!(backend.promote_sequence(0xffff), 0x1_ffff);
        assert_eq!(backend.promote_sequence(0x0000), 0x1_0000);
        assert_eq!(backend.promote_sequence(0xfffd), 0x1_fffd);
    }

    #[test]
    fn issue_sequence_records_and_consumes_origin_by_full_sequence() {
        let mut backend = dummy_backend();
        let origin = OriginContext {
            client_id: ClientId(7),
            nested_seq: 9,
            opcode: 12,
        };
        backend.set_active_origin(Some(origin));
        let (_wire, full) = backend.issue_sequence();
        assert_eq!(backend.take_origin_for_sequence(full), Some(origin));
        assert_eq!(backend.take_origin_for_sequence(full), None);
    }

    #[test]
    fn async_error_logging_path_consumes_origin_without_buffering_reply() {
        let mut backend = dummy_backend();
        let origin = OriginContext {
            client_id: ClientId(3),
            nested_seq: 4,
            opcode: 5,
        };
        backend.set_active_origin(Some(origin));
        let (_wire, full) = backend.issue_sequence();
        let mut bytes = vec![0; 32];
        bytes[0] = 0;
        bytes[1] = 3;
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        bytes[11] = 42;
        backend.stash_or_log_response(HostResponse {
            sequence: 1,
            sequence_full: full,
            bytes,
        });
        assert_eq!(backend.pending_replies.len(), 0);
        assert!(backend.take_origin_for_sequence(full).is_none());
        assert_eq!(read_u16(&[1, 0]), 1);
    }

    #[test]
    fn wait_for_reply_decodes_buffered_host_error() {
        let mut backend = dummy_backend();
        let full = 0x1_1234;
        let mut bytes = vec![0; 32];
        bytes[0] = 0;
        bytes[1] = 8;
        bytes[4..8].copy_from_slice(&0x00ab_cdefu32.to_le_bytes());
        bytes[8..10].copy_from_slice(&0x1234u16.to_le_bytes());
        bytes[10] = 62;
        backend.pending_replies.insert(HostResponse {
            sequence: 0x1234,
            sequence_full: full,
            bytes,
        });

        let error = backend
            .wait_for_reply(full)
            .expect("wait_for_reply io result")
            .expect_err("host error result");

        assert_eq!(
            error,
            HostError {
                code: 8,
                sequence: 0x1234,
                sequence_full: full,
                major_opcode: 62,
                minor_opcode: 0x1234,
                bad_value: 0x00ab_cdef,
            }
        );
    }

    #[test]
    fn async_error_path_buffers_structured_error() {
        let mut backend = dummy_backend();
        let origin = OriginContext {
            client_id: ClientId(4),
            nested_seq: 5,
            opcode: 6,
        };
        backend.set_active_origin(Some(origin));
        let (_wire, full) = backend.issue_sequence();
        let mut bytes = vec![0; 32];
        bytes[0] = 0;
        bytes[1] = 11;
        bytes[4..8].copy_from_slice(&0xdead_beefu32.to_le_bytes());
        bytes[8..10].copy_from_slice(&0x0042u16.to_le_bytes());
        bytes[10] = 17;
        backend.stash_or_log_response(HostResponse {
            sequence: 0x0042,
            sequence_full: full,
            bytes,
        });
        assert_eq!(backend.pending_errors.len(), 1);
    }

    #[test]
    fn update_host_event_mask_tracks_registry() {
        let mut backend = dummy_backend();
        assert_eq!(backend.update_host_event_mask(0x1234, 0x4, true), 0x4);
        assert_eq!(backend.host_event_mask(0x1234), 0x4);
        assert_eq!(backend.update_host_event_mask(0x1234, 0x8000, true), 0x8004);
        assert_eq!(backend.host_event_mask(0x1234), 0x8004);
        assert_eq!(backend.update_host_event_mask(0x1234, 0x4, false), 0x8000);
        assert_eq!(backend.host_event_mask(0x1234), 0x8000);
        assert_eq!(backend.update_host_event_mask(0x1234, 0x8000, false), 0);
        assert_eq!(backend.host_event_mask(0x1234), 0);
    }

    /// Wait_for_reply blocks on the Condvar; a parallel "dispatcher"
    /// pushes the response into PendingReplies; the waiter wakes up
    /// and returns Ok with the reply bytes. Exercises the post-Phase-6.3
    /// pathway that no longer reads from the stream synchronously.
    #[test]
    fn wait_for_reply_blocks_on_condvar_until_dispatcher_signals() {
        use std::{thread, time::Duration};

        let backend = dummy_backend();
        let pending = Arc::clone(&backend.pending_replies);
        // Mark dispatcher as "running" so wait_for_reply takes the
        // Condvar pathway instead of falling back to stream reads.
        let mut backend = backend;
        backend.dispatcher = Some(thread::spawn(|| {})); // placeholder JoinHandle
        let target_full: u64 = 0x1_2345;

        // Simulated dispatcher: 50ms later, insert a successful reply.
        let pending_for_thread = Arc::clone(&pending);
        let producer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let mut bytes = vec![0u8; 32];
            bytes[0] = 1; // reply
            bytes[1] = 0xab; // payload byte at offset 1
            bytes[8..10].copy_from_slice(&0x2345u16.to_le_bytes());
            pending_for_thread.insert(HostResponse {
                sequence: 0x2345,
                sequence_full: target_full,
                bytes,
            });
        });

        let reply = backend
            .wait_for_reply(target_full)
            .expect("wait_for_reply io result")
            .expect("reply, not error");
        producer.join().expect("producer thread join");
        assert_eq!(reply[0], 1);
        assert_eq!(reply[1], 0xab);
    }

    /// Async-error path: dispatcher emits BackendEvent::HostError on
    /// the channel for an error whose origin we previously recorded.
    /// `set_event_sink` then drains the channel via the consumer
    /// thread, so the sink sees the error with the right origin.
    #[test]
    fn dispatcher_routes_async_error_to_sink_with_origin() {
        use std::{sync::mpsc, time::Duration};

        struct CaptureSink(mpsc::Sender<crate::backend::BackendEvent>);
        impl crate::backend::BackendEventSink for CaptureSink {
            fn handle_backend_event(&mut self, event: crate::backend::BackendEvent) {
                let _ = self.0.send(event);
            }
        }

        let mut backend = dummy_backend();
        let origin = OriginContext {
            client_id: ClientId(11),
            nested_seq: 22,
            opcode: 33,
        };
        backend.set_active_origin(Some(origin));
        let (_wire, full) = backend.issue_sequence();

        // Hand-craft an error event into the channel as if the
        // dispatcher had decoded it. Verifies the consumer thread's
        // wiring rather than the read-side framing — read-side framing
        // is exercised by the live smoke test.
        let mut error_bytes = vec![0u8; 32];
        error_bytes[0] = 0; // error
        error_bytes[1] = 7;
        error_bytes[4..8].copy_from_slice(&0xfeed_face_u32.to_le_bytes());
        error_bytes[8..10].copy_from_slice(&0xbeefu16.to_le_bytes());
        error_bytes[10] = 99;
        let error_resp = HostResponse {
            sequence: 0xbeef,
            sequence_full: full,
            bytes: error_bytes,
        };
        let host_error = HostError::from_response(&error_resp).expect("host error");

        // Hook up the sink before pushing the event.
        let (tx, rx) = mpsc::channel();
        crate::backend::Backend::set_event_sink(&mut backend, Some(Box::new(CaptureSink(tx))));

        backend.emit_backend_event(crate::backend::BackendEvent::HostError {
            origin: Some(origin),
            error: host_error.into_io_error("simulated"),
        });

        // The consumer thread fans the event out to our CaptureSink.
        let event = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("backend event delivered");
        match event {
            crate::backend::BackendEvent::HostError {
                origin: got_origin,
                error: _,
            } => {
                assert_eq!(got_origin, Some(origin));
            }
            other => panic!("expected HostError, got {other:?}"),
        }
    }

    /// Sequence promotion edge cases: ancient seq, recent wrap, and
    /// the boundary where the host's wire seq matches the just-issued
    /// sequence (which should resolve to the previous block, never the
    /// not-yet-issued one).
    #[test]
    fn promote_seq_from_atomic_handles_wrap_and_ancient_seqs() {
        // Most recently issued seq_full = 0x2_0000 → next = 0x2_0001.
        // A wire seq of 0x0000 must resolve to 0x2_0000 (the most
        // recent), not 0x2_0000 + 0 (which would be the next request).
        assert_eq!(promote_seq_from_atomic(0x2_0001, 0x0000), 0x2_0000);

        // Wire seq matching the *next* counter exactly (impossibly
        // late) should saturate one block back so we never claim a
        // not-yet-issued slot.
        assert_eq!(promote_seq_from_atomic(0x2_0001, 0x0001), 0x1_0001);

        // Slightly older wire — still in the previous block when
        // newer ones are also possible. The promotion picks the
        // youngest non-future candidate.
        assert_eq!(promote_seq_from_atomic(0x2_0010, 0x000f), 0x2_000f);
        assert_eq!(promote_seq_from_atomic(0x2_0010, 0x0011), 0x1_0011);

        // First wrap: next=0x1_0000, wire=0xffff → 0x0_ffff (last in
        // initial block).
        assert_eq!(promote_seq_from_atomic(0x1_0000, 0xffff), 0x0_ffff);

        // Saturation guard: zero-base, wire larger than next. Returns 0
        // because saturating_sub clamps. Acceptable — sliding window
        // catches it as "too ancient" and the response is dropped.
        assert_eq!(promote_seq_from_atomic(0, 0x0001), 0);
    }

    /// Phase 6.3 Step 4: dispatcher decodes KeyPress (event type 2)
    /// out of a 32-byte X11 event header into `HostEvent::Key` with
    /// `pressed=true`. KeyRelease (event type 3) decodes the same
    /// shape with `pressed=false`. This is the substitute for the
    /// pre-Step-4 per-client kb pump's read path — so the merged
    /// dispatcher can fan keys out to the registered subscribers.
    #[test]
    fn decode_host_event_translates_key_press_and_release() {
        use crate::host_x11::HostEvent;
        use crate::host_x11::pump::decode_host_event;

        let mut press = [0u8; 32];
        press[0] = 2; // KeyPress
        press[1] = 0x39; // keycode
        press[4..8].copy_from_slice(&0x0102_0304u32.to_le_bytes()); // time
        press[20..22].copy_from_slice(&100i16.to_le_bytes()); // root_x
        press[22..24].copy_from_slice(&200i16.to_le_bytes()); // root_y
        press[24..26].copy_from_slice(&50i16.to_le_bytes()); // event_x
        press[26..28].copy_from_slice(&60i16.to_le_bytes()); // event_y
        press[28..30].copy_from_slice(&0x0001u16.to_le_bytes()); // state

        match decode_host_event(&press).expect("KeyPress decodes") {
            HostEvent::Key(key) => {
                assert!(key.pressed, "KeyPress yields pressed=true");
                assert_eq!(key.keycode, 0x39);
                assert_eq!(key.time, 0x0102_0304);
                assert_eq!(key.root_x, 100);
                assert_eq!(key.root_y, 200);
                assert_eq!(key.event_x, 50);
                assert_eq!(key.event_y, 60);
                assert_eq!(key.state, 0x0001);
            }
            other => panic!("expected HostEvent::Key, got {other:?}"),
        }

        let mut release = press;
        release[0] = 3; // KeyRelease
        match decode_host_event(&release).expect("KeyRelease decodes") {
            HostEvent::Key(key) => {
                assert!(!key.pressed, "KeyRelease yields pressed=false");
                assert_eq!(key.keycode, 0x39);
            }
            other => panic!("expected HostEvent::Key, got {other:?}"),
        }
    }

    /// Phase 6.3 Step 4: dispatcher decodes a synthetic-flag-stripped
    /// event type — bit 7 on event[0] marks events sent via
    /// SendEvent. Decoding must mask it off before classifying.
    #[test]
    fn decode_host_event_strips_synthetic_flag() {
        use crate::host_x11::HostEvent;
        use crate::host_x11::pump::decode_host_event;

        let mut synthetic_press = [0u8; 32];
        synthetic_press[0] = 2 | 0x80; // KeyPress with synthetic bit set
        synthetic_press[1] = 0x39;
        let event = decode_host_event(&synthetic_press).expect("synthetic key decodes");
        assert!(matches!(event, HostEvent::Key(_)));
    }

    /// Phase 6.3 Step 4: events the dispatcher doesn't surface
    /// (e.g. KeymapNotify = 11, MappingNotify = 34) decode to None,
    /// so the dispatcher loop just continues.
    #[test]
    fn decode_host_event_drops_uninteresting_event_types() {
        use crate::host_x11::pump::decode_host_event;

        for unhandled in [11u8, 13, 14, 15, 16, 18, 19, 20, 21, 23, 34] {
            let mut header = [0u8; 32];
            header[0] = unhandled;
            assert!(
                decode_host_event(&header).is_none(),
                "event type {unhandled} should decode to None",
            );
        }
    }

    /// Phase 6.3 Step 4: `add_key_subscriber` appends a Sender; the
    /// dispatcher's send-loop later fans events to every entry. We
    /// don't have a live dispatcher in this unit test, so we verify
    /// the registration shape directly: a freshly-added Sender lives
    /// in the shared list and a `send` from a simulated dispatcher
    /// arrives on the matching Receiver.
    #[test]
    fn add_key_subscriber_appends_sender_visible_to_shared_list() {
        use crate::host_x11::HostKeyEvent;
        let mut backend = dummy_backend();
        let (tx, rx) = crossbeam_channel::unbounded::<HostKeyEvent>();
        backend.add_key_subscriber(tx);
        // List has one entry, sending through it lands on rx.
        let subs = backend
            .key_subscribers
            .lock()
            .expect("key subscribers mutex poisoned");
        assert_eq!(subs.len(), 1, "one subscriber registered");
        let event = HostKeyEvent {
            pressed: true,
            keycode: 0x39,
            time: 1,
            root_x: 0,
            root_y: 0,
            event_x: 0,
            event_y: 0,
            state: 0,
        };
        for s in subs.iter() {
            s.send(event).expect("send to live subscriber");
        }
        drop(subs);
        let received = rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .expect("subscriber receives event");
        assert_eq!(received.keycode, 0x39);
    }

    /// Phase 6.3 Step 4: register_top_level inserts host_xid →
    /// nested_id into the shared xid_map. The wire-side write is
    /// exercised in the live integration smoke (xterm under wmaker);
    /// here we just confirm the registry shape that the sink relies
    /// on through `Backend::xid_map()`.
    #[test]
    fn register_top_level_populates_shared_xid_map() {
        use yserver_protocol::x11::ResourceId;
        let mut backend = dummy_backend();
        let nested_id = ResourceId(0x100);
        // The wire side flushes through the dummy stream's peer; the
        // peer side just discards. We only assert the in-memory map.
        let _ = backend.register_top_level(nested_id, 0xabcd_1234);
        let map = HostX11Backend::xid_map(&backend);
        assert_eq!(
            map.lock().unwrap().get(&0xabcd_1234).copied(),
            Some(nested_id),
        );
    }
}
