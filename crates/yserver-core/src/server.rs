use std::{
    collections::{HashMap, HashSet},
    io::Write,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU16, Ordering},
    },
    time::Instant,
};

use yserver_protocol::x11::{
    self, AtomId, ClientByteOrder, ClientId, ResourceId, SequenceNumber, shape, xfixes,
};

use crate::{randr::RandrState, resources::ResourceTable};

pub const FIRST_CLIENT_BASE: u32 = 0x0010_0000;
pub const PER_CLIENT_MASK: u32 = 0x000F_FFFF;

#[derive(Debug)]
pub struct IdAllocator {
    next_base: u32,
}

impl IdAllocator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_base: FIRST_CLIENT_BASE,
        }
    }

    /// Returns `(resource_id_base, resource_id_mask)` for a new client.
    /// Returns `None` when the next base would overflow `u32`.
    pub fn allocate(&mut self) -> Option<(u32, u32)> {
        let base = self.next_base;
        let next = base.checked_add(FIRST_CLIENT_BASE)?;
        self.next_base = next;
        Some((base, PER_CLIENT_MASK))
    }

    /// `id` is owned by the holder of `(base, mask)` iff `(id & !mask) == base`.
    #[must_use]
    pub fn validate_owned(id: u32, base: u32, mask: u32) -> bool {
        (id & !mask) == base
    }
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
pub struct AtomTable {
    by_name: HashMap<String, AtomId>,
    names: HashMap<u32, String>,
    next_id: u32,
}

impl AtomTable {
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_name: HashMap::new(),
            names: HashMap::new(),
            next_id: 69, // X11 predefined atoms are 1..=68; custom atoms start here
        }
    }

    pub fn intern(&mut self, name: &str, only_if_exists: bool) -> AtomId {
        if let Some(atom) = x11::well_known_atom(name) {
            return atom;
        }
        if let Some(atom) = self.by_name.get(name).copied() {
            return atom;
        }
        if only_if_exists {
            return AtomId(0);
        }
        let atom = AtomId(self.next_id);
        self.next_id += 1;
        self.by_name.insert(name.to_owned(), atom);
        self.names.insert(atom.0, name.to_owned());
        atom
    }

    #[must_use]
    pub fn name(&self, atom: AtomId) -> Option<&str> {
        x11::well_known_atom_name(atom).or_else(|| self.names.get(&atom.0).map(String::as_str))
    }

    #[must_use]
    pub fn exists(&self, atom: AtomId) -> bool {
        atom.0 != 0
            && (x11::well_known_atom_name(atom).is_some() || self.names.contains_key(&atom.0))
    }
}

#[derive(Debug, Clone)]
pub struct PassiveButtonGrab {
    pub owner: ClientId,
    pub grab_window: ResourceId,
    /// 0 = AnyButton
    pub button: u8,
    /// 0x8000 = AnyModifier
    pub modifiers: u16,
    pub event_mask: u32,
    pub pointer_mode: u8,
}

#[derive(Debug, Clone)]
pub struct KeyGrab {
    pub owner: ClientId,
    pub grab_window: ResourceId,
    /// 0 == AnyKey
    pub keycode: u8,
    /// 0x8000 == AnyModifier; otherwise the literal modifier-state mask
    pub modifiers: u16,
    pub owner_events: bool,
    /// 0 = Synchronous, 1 = Asynchronous
    pub pointer_mode: u8,
    /// 0 = Synchronous, 1 = Asynchronous
    pub keyboard_mode: u8,
}

#[derive(Debug, Clone, Copy)]
pub enum ActiveKeyboardGrabSource {
    /// from GrabKeyboard
    Explicit,
    /// activated by a passive GrabKey on the matching keycode press
    PassiveKey { keycode: u8 },
}

#[derive(Debug, Clone, Copy)]
pub struct ActiveKeyboardGrab {
    pub owner: ClientId,
    pub grab_window: ResourceId,
    pub source: ActiveKeyboardGrabSource,
}

#[derive(Debug, Clone, Copy)]
pub struct ActivePointerGrab {
    pub owner: ClientId,
    pub grab_window: ResourceId,
    pub event_mask: u16,
    /// 0 = inherit
    pub cursor: ResourceId,
    pub time: u32,
}

#[derive(Debug)]
pub struct ServerState {
    pub atoms: AtomTable,
    pub resources: ResourceTable,
    pub clients: HashMap<u32, ClientHandle>,
    pub id_allocator: IdAllocator,
    pub start_instant: Instant,
    pub randr: RandrState,
    /// RANDR event masks selected via RRSelectInput: (client, window) -> mask.
    pub randr_select_masks: HashMap<(u32, ResourceId), u16>,
    /// XKB SelectEvents masks: (client, device spec) -> selected event mask.
    pub xkb_select_event_masks: HashMap<(u32, u16), u16>,
    /// Selection ownership: maps selection atom → owning window (ResourceId).
    pub selections: HashMap<AtomId, ResourceId>,
    /// Active pointer grab: (grab owner, grab window). When set, all pointer
    /// events are redirected to the grab owner regardless of where the cursor is.
    pub pointer_grab: Option<(ClientId, ResourceId)>,
    /// Active pointer grab record (full state including event_mask/cursor/time).
    /// When set, mirrors `pointer_grab` and supersedes it for spec-correct
    /// `ChangeActivePointerGrab` semantics.
    pub active_pointer_grab: Option<ActivePointerGrab>,
    /// Registered passive button grabs.
    pub button_grabs: Vec<PassiveButtonGrab>,
    /// True when `pointer_grab` was activated by a passive button grab.
    pub pointer_grab_is_passive: bool,
    /// Frozen pointer event held by a sync passive grab.
    pub frozen_pointer_event: Option<crate::host_x11::HostPointerEvent>,
    /// Registered passive key grabs.
    pub key_grabs: Vec<KeyGrab>,
    /// Active keyboard grab (explicit or passive-induced).
    pub active_keyboard_grab: Option<ActiveKeyboardGrab>,
    /// XFIXES regions owned by clients.
    pub xfixes_regions: HashMap<u32, XFixesRegion>,
    /// XFIXES selection event masks: (client, window, selection atom) -> mask.
    pub xfixes_selection_masks: HashMap<(u32, ResourceId, AtomId), u32>,
    /// XFIXES cursor event masks: (client, window) -> mask.
    pub xfixes_cursor_masks: HashMap<(u32, ResourceId), u32>,
    /// SHAPE state per window. Missing entries mean the default window rectangle.
    pub shape_windows: HashMap<ResourceId, ShapeWindowState>,
    /// SHAPE select-input state: (client, window) -> enabled.
    pub shape_select_masks: HashMap<(u32, ResourceId), bool>,
    pub sync_counters: HashMap<u32, SyncCounter>,
    pub sync_alarms: HashMap<u32, SyncAlarm>,
    pub damage_objects: HashMap<u32, DamageObject>,
    pub composite_redirects: HashMap<(ResourceId, bool), u8>,
    pub present_event_selections: HashMap<u32, PresentEventSelection>,
    pub present_msc: HashMap<ResourceId, u64>,
    /// MIT-SHM segments — keyed by client-supplied `shmseg` ID.
    pub mit_shm_segments: HashMap<u32, MitShmSegment>,
}

impl ServerState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            atoms: AtomTable::new(),
            resources: ResourceTable::new(),
            clients: HashMap::new(),
            id_allocator: IdAllocator::new(),
            start_instant: Instant::now(),
            randr: RandrState::nested(0, 800, 600),
            randr_select_masks: HashMap::new(),
            xkb_select_event_masks: HashMap::new(),
            selections: HashMap::new(),
            pointer_grab: None,
            active_pointer_grab: None,
            button_grabs: Vec::new(),
            pointer_grab_is_passive: false,
            frozen_pointer_event: None,
            key_grabs: Vec::new(),
            active_keyboard_grab: None,
            xfixes_regions: HashMap::new(),
            xfixes_selection_masks: HashMap::new(),
            xfixes_cursor_masks: HashMap::new(),
            shape_windows: HashMap::new(),
            shape_select_masks: HashMap::new(),
            sync_counters: HashMap::new(),
            sync_alarms: HashMap::new(),
            damage_objects: HashMap::new(),
            composite_redirects: HashMap::new(),
            present_event_selections: HashMap::new(),
            present_msc: HashMap::new(),
            mit_shm_segments: HashMap::new(),
        }
    }

    #[must_use]
    pub fn timestamp_now(&self) -> u32 {
        // X11 timestamps are 32-bit milliseconds; truncation is intentional.
        let elapsed = self.start_instant.elapsed().as_millis();
        #[allow(clippy::cast_possible_truncation)]
        let ts = elapsed as u32;
        ts
    }
}

impl Default for ServerState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XFixesRegion {
    pub owner: ClientId,
    pub rects: Vec<xfixes::RegionRect>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ShapeWindowState {
    pub bounding: Option<Vec<xfixes::RegionRect>>,
    pub clip: Option<Vec<xfixes::RegionRect>>,
    pub input: Option<Vec<xfixes::RegionRect>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncCounter {
    pub owner: ClientId,
    pub value: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncAlarm {
    pub owner: ClientId,
    pub counter: u32,
    pub wait_value: i64,
    pub delta: i64,
    pub events: bool,
    pub state: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DamageObject {
    pub owner: ClientId,
    pub drawable: ResourceId,
    pub level: u8,
    pub rects: Vec<xfixes::RegionRect>,
    /// True when we've already emitted a `DamageNotify` for this Subtract
    /// cycle. The "cycle" begins after `DamageSubtract` clears the
    /// accumulated region. Levels 2 (BoundingBox) and 3 (NonEmpty) emit at
    /// most one event per cycle; resetting this flag is the cycle boundary.
    pub pending_notify_fired: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentEventSelection {
    pub owner: ClientId,
    pub window: ResourceId,
    pub event_mask: u32,
}

/// A shared memory segment attached via MIT-SHM. Owns the lifetime of both
/// the file descriptor and the kernel mapping; the `Drop` impl `munmap`s
/// and closes the fd in the right order.
#[derive(Debug)]
pub struct MitShmSegment {
    pub owner: ClientId,
    /// Length of the memory mapping, in bytes.
    pub size: usize,
    /// Whether the client requested a read-only attach. We honour this on
    /// `GetImage` (which writes back into the segment) by failing those
    /// requests with `BadAccess`.
    pub read_only: bool,
    /// Pointer to the start of the mapping. Always non-null while this
    /// `MitShmSegment` is alive.
    addr: *mut libc::c_void,
    /// Backing source — either an FD we close on Drop, or a SysV shmat
    /// mapping we shmdt on Drop.
    backing: MitShmBacking,
}

#[derive(Debug)]
enum MitShmBacking {
    /// `AttachFd`: file descriptor that backs `addr` via `mmap`.
    Fd(libc::c_int),
    /// Legacy `Attach`: SysV mapping. `addr` was returned by `shmat(2)`.
    Sysv,
}

// Safe to send across threads: the underlying memory is independent of any
// thread-local state, and we serialize access via `&mut ServerState`.
unsafe impl Send for MitShmSegment {}
unsafe impl Sync for MitShmSegment {}

impl MitShmSegment {
    /// Map an attached file descriptor. Caller must have verified the FD
    /// references a regular file or memfd; we `fstat` to learn the size.
    ///
    /// On success, takes ownership of `fd` and will close it on `Drop`.
    pub fn from_fd(owner: ClientId, fd: libc::c_int, read_only: bool) -> std::io::Result<Self> {
        // Stat to get size.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstat(fd, &mut stat) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }
        if stat.st_size <= 0 {
            unsafe { libc::close(fd) };
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "MIT-SHM AttachFd: zero-length segment",
            ));
        }
        let size = stat.st_size as usize;
        let prot = if read_only {
            libc::PROT_READ
        } else {
            libc::PROT_READ | libc::PROT_WRITE
        };
        let addr = unsafe { libc::mmap(std::ptr::null_mut(), size, prot, libc::MAP_SHARED, fd, 0) };
        if addr == libc::MAP_FAILED {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(err);
        }
        Ok(Self {
            owner,
            size,
            read_only,
            addr,
            backing: MitShmBacking::Fd(fd),
        })
    }

    /// Attach a SysV shared-memory segment created by the client via
    /// `shmget(2)`. Returns an error when the kernel rejects `shmat` (most
    /// commonly because the client and server live in different IPC
    /// namespaces).
    pub fn from_shmid(owner: ClientId, shmid: u32, read_only: bool) -> std::io::Result<Self> {
        let flags = if read_only { libc::SHM_RDONLY } else { 0 };
        // SAFETY: shmat is fine to call with arbitrary user-provided shmid;
        // it returns (void*)-1 on failure.
        let addr = unsafe { libc::shmat(shmid as libc::c_int, std::ptr::null(), flags) };
        if addr == (-1_isize as *mut libc::c_void) {
            return Err(std::io::Error::last_os_error());
        }
        // Query the segment size via shmctl(IPC_STAT).
        let mut info: libc::shmid_ds = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::shmctl(shmid as libc::c_int, libc::IPC_STAT, &raw mut info) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::shmdt(addr) };
            return Err(err);
        }
        let size = info.shm_segsz as usize;
        if size == 0 {
            unsafe { libc::shmdt(addr) };
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "MIT-SHM Attach: zero-length segment",
            ));
        }
        Ok(Self {
            owner,
            size,
            read_only,
            addr,
            backing: MitShmBacking::Sysv,
        })
    }

    /// View into the mapped memory. Lifetime is tied to `&self`, so callers
    /// may only borrow within their own request handler.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `addr` is non-null and valid for `size` bytes for the
        // lifetime of `self` (until Drop). Memory is shared across processes
        // but X server semantics expect us to read the snapshot from our
        // perspective synchronously inside a request handler.
        unsafe { std::slice::from_raw_parts(self.addr.cast::<u8>(), self.size) }
    }

    /// Mutable view into the mapped memory. Returns `None` for read-only
    /// segments — caller should reply `BadAccess`.
    pub fn as_mut_slice(&mut self) -> Option<&mut [u8]> {
        if self.read_only {
            return None;
        }
        // SAFETY: same as `as_slice`, plus the segment was mapped with
        // `PROT_WRITE` because `read_only == false`.
        Some(unsafe { std::slice::from_raw_parts_mut(self.addr.cast::<u8>(), self.size) })
    }
}

impl Drop for MitShmSegment {
    fn drop(&mut self) {
        unsafe {
            match self.backing {
                MitShmBacking::Fd(fd) => {
                    libc::munmap(self.addr, self.size);
                    libc::close(fd);
                }
                MitShmBacking::Sysv => {
                    libc::shmdt(self.addr);
                }
            }
        }
    }
}

impl Default for SyncAlarm {
    fn default() -> Self {
        Self {
            owner: ClientId(0),
            counter: 0,
            wait_value: 0,
            delta: 0,
            events: false,
            state: 0,
        }
    }
}

impl ShapeWindowState {
    pub fn rects_mut(&mut self, kind: u8) -> Option<&mut Option<Vec<xfixes::RegionRect>>> {
        match kind {
            shape::KIND_BOUNDING => Some(&mut self.bounding),
            shape::KIND_CLIP => Some(&mut self.clip),
            shape::KIND_INPUT => Some(&mut self.input),
            _ => None,
        }
    }

    #[must_use]
    pub fn rects(&self, kind: u8) -> Option<&Vec<xfixes::RegionRect>> {
        match kind {
            shape::KIND_BOUNDING => self.bounding.as_ref(),
            shape::KIND_CLIP => self.clip.as_ref(),
            shape::KIND_INPUT => self.input.as_ref(),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct ClientHandle {
    pub writer: Arc<Mutex<UnixStream>>,
    pub byte_order: ClientByteOrder,
    pub last_sequence: Arc<AtomicU16>,
    pub resource_id_base: u32,
    pub resource_id_mask: u32,
    pub event_masks: HashMap<ResourceId, u32>,
    /// Foreign windows the client wants kept alive after disconnect
    /// (X11 ChangeSaveSet semantics).
    pub save_set: HashSet<ResourceId>,
    pub big_requests_enabled: bool,
    /// XI2 event masks: (window_id, device_id) -> mask
    pub xi2_masks: HashMap<(ResourceId, u16), u32>,
}

/// Snapshot of a client's writer for cross-client event fanout.
#[derive(Clone)]
pub struct EventTarget {
    pub writer: Arc<Mutex<UnixStream>>,
    pub byte_order: ClientByteOrder,
    pub last_sequence: Arc<AtomicU16>,
}

impl ServerState {
    fn event_target_for_client(client: &ClientHandle) -> EventTarget {
        EventTarget {
            writer: client.writer.clone(),
            byte_order: client.byte_order,
            last_sequence: client.last_sequence.clone(),
        }
    }

    #[must_use]
    pub fn subscribers(&self, window: ResourceId, mask_bit: u32) -> Vec<EventTarget> {
        self.clients
            .values()
            .filter_map(|c| {
                let mask = c.event_masks.get(&window).copied().unwrap_or(0);
                if mask & mask_bit != 0 {
                    Some(Self::event_target_for_client(c))
                } else {
                    None
                }
            })
            .collect()
    }

    #[must_use]
    pub fn subscribers_intersecting(
        &self,
        window: ResourceId,
        event_mask: u32,
    ) -> Vec<EventTarget> {
        self.clients
            .values()
            .filter_map(|c| {
                let mask = c.event_masks.get(&window).copied().unwrap_or(0);
                if mask & event_mask != 0 {
                    Some(Self::event_target_for_client(c))
                } else {
                    None
                }
            })
            .collect()
    }

    #[must_use]
    pub fn client_target(&self, client_id: ClientId) -> Option<EventTarget> {
        self.clients
            .get(&client_id.0)
            .map(Self::event_target_for_client)
    }

    #[must_use]
    pub fn selection_owner_target(&self, selection: AtomId) -> Option<(ResourceId, EventTarget)> {
        let owner_window = *self.selections.get(&selection)?;
        let owner_client = self.resources.window_owner(owner_window)?;
        let target = self.client_target(owner_client)?;
        Some((owner_window, target))
    }

    pub fn drop_window_subscriptions(&mut self, windows: &[ResourceId]) {
        for client in self.clients.values_mut() {
            for w in windows {
                client.event_masks.remove(w);
            }
        }
    }

    pub fn find_passive_grab(
        &self,
        window: ResourceId,
        button: u8,
        state_mask: u16,
    ) -> Option<PassiveButtonGrab> {
        let mut current = window;
        let mut depth = 0usize;
        loop {
            for grab in &self.button_grabs {
                if grab.grab_window != current {
                    continue;
                }
                let button_match = grab.button == 0 || grab.button == button;
                let mod_match = grab.modifiers == 0x8000 || grab.modifiers == (state_mask & 0x00ff);
                if button_match && mod_match {
                    return Some(grab.clone());
                }
            }
            let w = self.resources.window(current)?;
            if w.parent == current || w.parent == crate::resources::ROOT_WINDOW {
                break;
            }
            current = w.parent;
            depth += 1;
            if depth > 256 {
                break;
            }
        }
        None
    }

    /// X11 passive `GrabKey` lookup.
    ///
    /// Phase-2 subset: matches grabs whose `grab_window` is the focused
    /// window, an ancestor of it, or root. The descendant-containing-pointer
    /// case is deferred until a real client needs it.
    #[must_use]
    pub fn find_key_grab(
        &self,
        window: ResourceId,
        keycode: u8,
        state_mask: u16,
    ) -> Option<&KeyGrab> {
        let mut current = window;
        let mut depth = 0usize;
        let mut tried_root = false;
        loop {
            for grab in &self.key_grabs {
                if grab.grab_window != current {
                    continue;
                }
                let key_match = grab.keycode == 0 || grab.keycode == keycode;
                let mod_match = grab.modifiers == 0x8000 || grab.modifiers == (state_mask & 0x00ff);
                if key_match && mod_match {
                    return Some(grab);
                }
            }
            if current == crate::resources::ROOT_WINDOW {
                tried_root = true;
                break;
            }
            let Some(w) = self.resources.window(current) else {
                break;
            };
            if w.parent == current {
                break;
            }
            current = w.parent;
            depth += 1;
            if depth > 256 {
                break;
            }
        }
        if !tried_root && current != crate::resources::ROOT_WINDOW {
            for grab in &self.key_grabs {
                if grab.grab_window != crate::resources::ROOT_WINDOW {
                    continue;
                }
                let key_match = grab.keycode == 0 || grab.keycode == keycode;
                let mod_match = grab.modifiers == 0x8000 || grab.modifiers == (state_mask & 0x00ff);
                if key_match && mod_match {
                    return Some(grab);
                }
            }
        }
        None
    }
}

/// Like `emit_window_event` but operates on a pre-snapshotted target list (use when the lock has
/// already been dropped — e.g. after `destroy_window`).
pub fn fanout_event(
    targets: &[EventTarget],
    encode: impl Fn(&mut Vec<u8>, SequenceNumber, ClientByteOrder),
) {
    for target in targets {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        encode(&mut buf, seq, target.byte_order);
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }
}

pub fn fanout_raw_event(targets: &[EventTarget], event: &[u8; 32]) {
    for target in targets {
        let seq = target.last_sequence.load(Ordering::Relaxed);
        let mut buf = *event;
        buf[2] = (seq & 0xff) as u8;
        buf[3] = ((seq >> 8) & 0xff) as u8;
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }
}

#[must_use]
pub(crate) fn xi2_mask_for_client(
    client: &ClientHandle,
    target: ResourceId,
    fallback: ResourceId,
    device_candidates: &[u16],
) -> u32 {
    for window in [target, fallback] {
        for deviceid in device_candidates {
            if let Some(mask) = client.xi2_masks.get(&(window, *deviceid)) {
                return *mask;
            }
        }
        if fallback == target {
            break;
        }
    }
    0
}

pub fn emit_window_event(
    state: &Mutex<ServerState>,
    window: ResourceId,
    mask_bit: u32,
    encode: impl Fn(&mut Vec<u8>, SequenceNumber, ClientByteOrder),
) {
    let targets = match state.lock() {
        Ok(g) => g.subscribers(window, mask_bit),
        Err(_) => return,
    };
    fanout_event(&targets, encode);
}

#[allow(clippy::too_many_lines)]
pub fn pointer_event_fanout(
    state: &Mutex<ServerState>,
    xid_map: &crate::host_x11::HostXidMap,
    event: crate::host_x11::HostPointerEvent,
) {
    use crate::host_x11::PointerEventKind;

    // Translate root_x/root_y from host-screen coordinates into ynest-root
    // coordinates. The host pump reports root_x/y relative to the host server's
    // root window, but our nested clients see the ynest container as their
    // root, so values must be relative to that. For events on a registered
    // top-level subwindow we have host event_x/y (relative to that subwindow)
    // and the top-level's known position in nested-root, so the translation is
    // straightforward. Without this translation, clients placing popups or
    // tooltips at root_x/root_y end up off-screen by the container's host
    // offset.
    let event = if let Some(top_level_id) = xid_map
        .lock()
        .ok()
        .and_then(|m| m.get(&event.host_xid).copied())
    {
        let translated = state.lock().ok().and_then(|g| {
            g.resources
                .window(top_level_id)
                .map(|w| (w.x + event.event_x, w.y + event.event_y))
        });
        if let Some((rx, ry)) = translated {
            crate::host_x11::HostPointerEvent {
                root_x: rx,
                root_y: ry,
                ..event
            }
        } else {
            event
        }
    } else {
        event
    };

    // Active pointer grab: redirect all button/motion events to grab owner.
    let grab_state = match state.lock() {
        Ok(g) => g.pointer_grab.and_then(|(client_id, grab_window)| {
            g.client_target(client_id).map(|t| (grab_window, t))
        }),
        Err(_) => return,
    };
    if let Some((grab_window, target)) = grab_state {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        match event.kind {
            PointerEventKind::ButtonPress => x11::encode_button_press_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: event.detail,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: grab_window,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x: event.root_x,
                    event_y: event.root_y,
                    state: event.state,
                },
            ),
            PointerEventKind::ButtonRelease => x11::encode_button_release_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: event.detail,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: grab_window,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x: event.root_x,
                    event_y: event.root_y,
                    state: event.state,
                },
            ),
            PointerEventKind::MotionNotify => x11::encode_motion_notify_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: 0,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: grab_window,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x: event.root_x,
                    event_y: event.root_y,
                    state: event.state,
                },
            ),
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify => return,
        }
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
        if event.kind == PointerEventKind::ButtonRelease
            && let Ok(mut s) = state.lock()
            && s.pointer_grab_is_passive
        {
            s.pointer_grab = None;
            s.pointer_grab_is_passive = false;
            s.frozen_pointer_event = None;
        }
        return;
    }

    // Passive button grab matching for ButtonPress events.
    if event.kind == PointerEventKind::ButtonPress {
        let top_level_id_opt = xid_map
            .lock()
            .ok()
            .and_then(|m| m.get(&event.host_xid).copied());
        let matched = top_level_id_opt.and_then(|top| {
            let s = state.lock().ok()?;
            let (hit_window, _, _) = s
                .resources
                .pointer_target_at(top, event.event_x, event.event_y)
                .unwrap_or((top, event.event_x, event.event_y));
            s.find_passive_grab(hit_window, event.detail, event.state)
        });
        if let Some(grab) = matched {
            let target_opt = match state.lock() {
                Ok(mut s) => {
                    let target = s.client_target(grab.owner);
                    if grab.pointer_mode == 0 {
                        s.frozen_pointer_event = Some(event);
                    }
                    s.pointer_grab = Some((grab.owner, grab.grab_window));
                    s.pointer_grab_is_passive = true;
                    target
                }
                Err(_) => return,
            };
            if let Some(target) = target_opt {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                x11::encode_button_press_event(
                    &mut buf,
                    target.byte_order,
                    x11::PointerEvent {
                        sequence: seq,
                        detail: event.detail,
                        time: event.time,
                        root: crate::resources::ROOT_WINDOW,
                        event: grab.grab_window,
                        root_x: event.root_x,
                        root_y: event.root_y,
                        event_x: event.event_x,
                        event_y: event.event_y,
                        state: event.state,
                    },
                );
                if let Ok(mut w) = target.writer.lock() {
                    let _ = w.write_all(&buf);
                }
            }
            return;
        }
    }

    let top_level_id = match xid_map.lock() {
        Ok(map) => match map.get(&event.host_xid).copied() {
            Some(id) => id,
            None => return,
        },
        Err(_) => return,
    };
    let mask_bit: u32 = match event.kind {
        PointerEventKind::ButtonPress => 0x0000_0004,
        PointerEventKind::ButtonRelease => 0x0000_0008,
        PointerEventKind::MotionNotify => {
            // PointerMotion (0x40), plus ButtonMotion (0x2000) and the
            // matching ButtonNMotion bit for each currently held button.
            // event.state bits 8..=12 are Button1..Button5.
            let mut bits: u32 = 0x0000_0040;
            let buttons_held = (event.state >> 8) & 0x1f;
            if buttons_held != 0 {
                bits |= 0x0000_2000;
                for n in 0..5 {
                    if buttons_held & (1 << n) != 0 {
                        bits |= 0x0000_0100 << n;
                    }
                }
            }
            bits
        }
        PointerEventKind::EnterNotify => 0x0000_0010,
        PointerEventKind::LeaveNotify => 0x0000_0020,
    };
    let xi2_evtype: u16 = match event.kind {
        PointerEventKind::ButtonPress => 4,
        PointerEventKind::ButtonRelease => 5,
        PointerEventKind::MotionNotify => 6,
        PointerEventKind::EnterNotify => 7,
        PointerEventKind::LeaveNotify => 8,
    };

    let (nested_id, event_x, event_y, core_targets, xi2_targets) = match state.lock() {
        Ok(g) => {
            let (target, event_x, event_y) = g
                .resources
                .pointer_target_at(top_level_id, event.event_x, event.event_y)
                .unwrap_or((top_level_id, event.event_x, event.event_y));

            let mut core_targets = g.subscribers(target, mask_bit);
            if core_targets.is_empty() && target != top_level_id {
                core_targets = g.subscribers(top_level_id, mask_bit);
            }

            let mut xi2_targets = Vec::new();
            if xi2_evtype != 0 {
                for c in g.clients.values() {
                    let mask = xi2_mask_for_client(c, target, top_level_id, &[2, 1, 0]);
                    if mask & (1 << xi2_evtype) != 0 {
                        xi2_targets.push(ServerState::event_target_for_client(c));
                    }
                }
            }

            (target, event_x, event_y, core_targets, xi2_targets)
        }
        Err(_) => return,
    };

    for target in core_targets {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        match event.kind {
            PointerEventKind::ButtonPress => x11::encode_button_press_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: event.detail,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x,
                    event_y,
                    state: event.state,
                },
            ),
            PointerEventKind::ButtonRelease => x11::encode_button_release_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: event.detail,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x,
                    event_y,
                    state: event.state,
                },
            ),
            PointerEventKind::MotionNotify => x11::encode_motion_notify_event(
                &mut buf,
                target.byte_order,
                x11::PointerEvent {
                    sequence: seq,
                    detail: 0,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x,
                    event_y,
                    state: event.state,
                },
            ),
            PointerEventKind::EnterNotify => x11::encode_enter_notify_event(
                &mut buf,
                target.byte_order,
                x11::CrossingEvent {
                    sequence: seq,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x,
                    event_y,
                    state: event.state,
                },
            ),
            PointerEventKind::LeaveNotify => x11::encode_leave_notify_event(
                &mut buf,
                target.byte_order,
                x11::CrossingEvent {
                    sequence: seq,
                    time: event.time,
                    root: crate::resources::ROOT_WINDOW,
                    event: nested_id,
                    root_x: event.root_x,
                    root_y: event.root_y,
                    event_x,
                    event_y,
                    state: event.state,
                },
            ),
        }
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }

    for target in xi2_targets {
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(84);
        if matches!(
            event.kind,
            PointerEventKind::EnterNotify | PointerEventKind::LeaveNotify
        ) {
            x11::encode_xi2_crossing_event(
                &mut buf,
                seq,
                137,
                xi2_evtype,
                2,
                event.time,
                crate::resources::ROOT_WINDOW,
                nested_id,
                event.root_x,
                event.root_y,
                event_x,
                event_y,
                event.state,
                0,
                0,
                2,
            );
        } else {
            x11::encode_xi2_device_event(
                &mut buf,
                seq,
                137, // XI2 major opcode
                xi2_evtype,
                2, // deviceid: Master Pointer
                event.time,
                crate::resources::ROOT_WINDOW,
                nested_id,
                event.root_x,
                event.root_y,
                event_x,
                event_y,
                event.state,
                u32::from(event.detail),
                2,
            );
        }
        if let Ok(mut w) = target.writer.lock() {
            let _ = w.write_all(&buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn first_client_base_is_above_root_resources() {
        let mut a = IdAllocator::new();
        let (base, mask) = a.allocate().expect("first allocate");
        assert_eq!(base, 0x0010_0000);
        assert_eq!(mask, 0x000F_FFFF);
    }

    #[test]
    fn allocate_increments_by_first_client_base() {
        let mut a = IdAllocator::new();
        let (b1, _) = a.allocate().unwrap();
        let (b2, _) = a.allocate().unwrap();
        assert_eq!(b2 - b1, FIRST_CLIENT_BASE);
    }

    #[test]
    fn validate_owned_accepts_ids_in_range() {
        let (base, mask) = (0x0020_0000, 0x000F_FFFF);
        assert!(IdAllocator::validate_owned(base, base, mask));
        assert!(IdAllocator::validate_owned(base | mask, base, mask));
        assert!(IdAllocator::validate_owned(base + 0x42, base, mask));
    }

    #[test]
    fn validate_owned_rejects_ids_outside_range() {
        let (base, mask) = (0x0020_0000, 0x000F_FFFF);
        assert!(!IdAllocator::validate_owned(0x0010_0000, base, mask));
        assert!(!IdAllocator::validate_owned(0x0030_0000, base, mask));
        assert!(!IdAllocator::validate_owned(0x0000_0100, base, mask));
    }

    proptest! {
        #[test]
        fn pairwise_non_overlap(n in 1usize..256) {
            let mut a = IdAllocator::new();
            let mut ranges = Vec::with_capacity(n);
            for _ in 0..n {
                ranges.push(a.allocate().expect("range"));
            }
            for (i, (b1, m1)) in ranges.iter().enumerate() {
                for (b2, m2) in ranges.iter().skip(i + 1) {
                    let lo1 = *b1;
                    let hi1 = b1 | m1;
                    let lo2 = *b2;
                    let hi2 = b2 | m2;
                    prop_assert!(hi1 < lo2 || hi2 < lo1, "overlap {:x}..={:x} vs {:x}..={:x}", lo1, hi1, lo2, hi2);
                }
            }
        }

        #[test]
        fn mask_covers_assigned_bits(n in 1usize..64) {
            let mut a = IdAllocator::new();
            for _ in 0..n {
                let (base, mask) = a.allocate().unwrap();
                prop_assert_eq!(base & mask, 0);
            }
        }

        #[test]
        fn allocated_bases_above_root_range(n in 1usize..64) {
            let mut a = IdAllocator::new();
            for _ in 0..n {
                let (base, _) = a.allocate().unwrap();
                prop_assert!(base >= 0x0010_0000);
            }
        }

        #[test]
        fn validate_round_trip(seed in 0u32..256, offset in 0u32..=PER_CLIENT_MASK) {
            let mut a = IdAllocator::new();
            for _ in 0..seed { a.allocate().unwrap(); }
            let (base, mask) = a.allocate().unwrap();
            let id = base + offset;
            prop_assert!(IdAllocator::validate_owned(id, base, mask));
            let other = base.wrapping_add(0x0010_0000).wrapping_add(offset);
            prop_assert!(!IdAllocator::validate_owned(other, base, mask));
        }
    }

    #[test]
    fn subscribers_returns_clients_with_bit_set() {
        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0x0040_0000)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
            },
        );
        state.clients.insert(
            2,
            ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0020_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0x0000_0001)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
            },
        );
        // PropertyChange = 0x0040_0000
        let subs = state.subscribers(ResourceId(0x100), 0x0040_0000);
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn subscribers_omits_other_windows() {
        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x200), 0xFFFF_FFFF)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
            },
        );
        let subs = state.subscribers(ResourceId(0x100), 0x0040_0000);
        assert!(subs.is_empty());
    }

    #[test]
    fn xi2_pointer_mask_matches_exact_and_wildcard_devices() {
        for deviceid in [2u16, 1, 0] {
            let client = ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::from([((ResourceId(0x100), deviceid), 1 << 4)]),
            };

            assert_eq!(
                xi2_mask_for_client(&client, ResourceId(0x100), ResourceId(0x100), &[2, 1, 0]),
                1 << 4
            );
        }
    }

    #[test]
    fn xi2_keyboard_mask_matches_exact_and_wildcard_devices() {
        for deviceid in [3u16, 1, 0] {
            let client = ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::from([((ResourceId(0x100), deviceid), 1 << 2)]),
            };

            assert_eq!(
                xi2_mask_for_client(&client, ResourceId(0x100), ResourceId(0x100), &[3, 1, 0]),
                1 << 2
            );
        }
    }

    #[test]
    fn subscribers_omits_disconnected_client() {
        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0x0040_0000)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
            },
        );
        assert_eq!(state.subscribers(ResourceId(0x100), 0x0040_0000).len(), 1);
        state.clients.remove(&1);
        assert!(state.subscribers(ResourceId(0x100), 0x0040_0000).is_empty());
    }

    #[test]
    fn subscribers_intersecting_matches_any_selected_bit() {
        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0b1010)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
            },
        );
        state.clients.insert(
            2,
            ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0020_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0b0100)]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
            },
        );

        assert_eq!(
            state
                .subscribers_intersecting(ResourceId(0x100), 0b0010)
                .len(),
            1
        );
        assert_eq!(
            state
                .subscribers_intersecting(ResourceId(0x100), 0b1100)
                .len(),
            2
        );
        assert!(
            state
                .subscribers_intersecting(ResourceId(0x100), 0b0001)
                .is_empty()
        );
    }

    #[test]
    fn client_target_returns_connected_client() {
        let mut state = ServerState::new();
        state.clients.insert(
            7,
            ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0x1234)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::new(),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
            },
        );

        assert!(state.client_target(ClientId(7)).is_some());
        assert!(state.client_target(ClientId(8)).is_none());
    }

    fn make_test_writer() -> Arc<Mutex<UnixStream>> {
        let (a, _b) = UnixStream::pair().expect("socketpair");
        Arc::new(Mutex::new(a))
    }

    #[test]
    fn unmap_notify_fanout_reaches_only_subscribed_clients() {
        use yserver_protocol::x11::{SequenceNumber, encode_unmap_notify_event};

        // Client A: StructureNotify on window 0x100.
        let (a_writer_local, _a_reader_remote) = UnixStream::pair().expect("socketpair");
        // Client B: KeyPress only on window 0x100 (NOT StructureNotify).
        let (b_writer_local, _b_reader_remote) = UnixStream::pair().expect("socketpair");

        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientHandle {
                writer: Arc::new(Mutex::new(a_writer_local)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0x0002_0000)]), // StructureNotify
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
            },
        );
        state.clients.insert(
            2,
            ClientHandle {
                writer: Arc::new(Mutex::new(b_writer_local)),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0020_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([(ResourceId(0x100), 0x0000_0001)]), // KeyPress
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
            },
        );

        let subs = state.subscribers(ResourceId(0x100), 0x0002_0000);
        assert_eq!(subs.len(), 1, "only client A should be subscribed");

        let target = &subs[0];
        let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        encode_unmap_notify_event(
            &mut buf,
            seq,
            target.byte_order,
            ResourceId(0x100),
            ResourceId(0x100),
            false,
        );
        assert_eq!(buf[0], 18, "wire byte 0 is UnmapNotify");
        assert_eq!(&buf[4..8], &0x100u32.to_le_bytes());
        assert_eq!(&buf[8..12], &0x100u32.to_le_bytes());
        assert_eq!(buf[12], 0, "from_configure = false");
    }

    #[test]
    fn drop_window_subscriptions_removes_entries_for_destroyed_windows() {
        let mut state = ServerState::new();
        state.clients.insert(
            1,
            ClientHandle {
                writer: make_test_writer(),
                byte_order: ClientByteOrder::LittleEndian,
                last_sequence: Arc::new(AtomicU16::new(0)),
                resource_id_base: 0x0010_0000,
                resource_id_mask: 0x000F_FFFF,
                event_masks: HashMap::from([
                    (ResourceId(0x100), 0x0040_0000),
                    (ResourceId(0x200), 0x0040_0000),
                ]),
                save_set: HashSet::new(),
                big_requests_enabled: false,
                xi2_masks: HashMap::new(),
            },
        );
        assert_eq!(state.subscribers(ResourceId(0x100), 0x0040_0000).len(), 1);
        state.drop_window_subscriptions(&[ResourceId(0x100)]);
        assert!(state.subscribers(ResourceId(0x100), 0x0040_0000).is_empty());
        // Surviving window's subscription stays.
        assert_eq!(state.subscribers(ResourceId(0x200), 0x0040_0000).len(), 1);
    }

    #[test]
    fn pointer_event_fanout_filters_by_mask() {
        use std::{collections::HashMap as StdHashMap, sync::Mutex as StdMutex};

        use crate::host_x11::{HostPointerEvent, PointerEventKind};

        // Client A: ButtonPress on window 0x0010_0002.
        let (a_writer_local, _a_reader_remote) = UnixStream::pair().expect("socketpair");
        // Client B: MotionNotify on window 0x0010_0002.
        let (b_writer_local, _b_reader_remote) = UnixStream::pair().expect("socketpair");
        // Client C: no pointer events at all.
        let (c_writer_local, _c_reader_remote) = UnixStream::pair().expect("socketpair");

        let state = StdMutex::new(ServerState::new());
        {
            let mut s = state.lock().unwrap();
            s.clients.insert(
                1,
                ClientHandle {
                    writer: Arc::new(Mutex::new(a_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0010_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(ResourceId(0x0010_0002), 0x0000_0004)]), // ButtonPress
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                },
            );
            s.clients.insert(
                2,
                ClientHandle {
                    writer: Arc::new(Mutex::new(b_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0020_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(ResourceId(0x0010_0002), 0x0000_0040)]), // PointerMotion
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                },
            );
            s.clients.insert(
                3,
                ClientHandle {
                    writer: Arc::new(Mutex::new(c_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0030_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::new(),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                },
            );
        }

        let mut map = StdHashMap::new();
        map.insert(0xCAFE_u32, ResourceId(0x0010_0002));
        let xid_map = Arc::new(StdMutex::new(map));

        pointer_event_fanout(
            &state,
            &xid_map,
            HostPointerEvent {
                kind: PointerEventKind::ButtonPress,
                host_xid: 0xCAFE,
                detail: 1,
                time: 0,
                root_x: 1,
                root_y: 2,
                event_x: 3,
                event_y: 4,
                state: 0,
            },
        );

        let s = state.lock().unwrap();
        assert_eq!(
            s.subscribers(ResourceId(0x0010_0002), 0x0000_0004).len(),
            1,
            "only client A selected ButtonPress"
        );
        assert_eq!(
            s.subscribers(ResourceId(0x0010_0002), 0x0000_0040).len(),
            1,
            "only client B selected MotionNotify"
        );
    }

    #[test]
    fn pointer_event_fanout_delivers_motion_under_button_motion_mask() {
        use std::{
            collections::HashMap as StdHashMap,
            io::{ErrorKind, Read},
            sync::Mutex as StdMutex,
        };

        use crate::host_x11::{HostPointerEvent, PointerEventKind};

        // Client A: subscribes to ButtonMotion (0x2000) only. Mirrors
        // wmaker's frame mask: it expects motion while a button is held.
        let (a_writer_local, mut a_reader_remote) = UnixStream::pair().expect("socketpair");
        // Client B: no motion mask at all — must not receive anything.
        let (b_writer_local, mut b_reader_remote) = UnixStream::pair().expect("socketpair");

        a_reader_remote.set_nonblocking(true).unwrap();
        b_reader_remote.set_nonblocking(true).unwrap();

        let state = StdMutex::new(ServerState::new());
        {
            let mut s = state.lock().unwrap();
            // Top-level window so pointer_target_at returns the same id.
            s.resources.create_window(
                ClientId(1),
                yserver_protocol::x11::CreateWindowRequest {
                    depth: 24,
                    window: ResourceId(0x0010_0002),
                    parent: crate::resources::ROOT_WINDOW,
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                    border_width: 0,
                    class: 1,
                    visual: crate::resources::ROOT_VISUAL,
                    background_pixel: None,
                    event_mask: None,
                    override_redirect: None,
                },
            );
            let _ = s.resources.map_window(ResourceId(0x0010_0002));
            s.clients.insert(
                1,
                ClientHandle {
                    writer: Arc::new(Mutex::new(a_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0010_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(ResourceId(0x0010_0002), 0x0000_2000)]),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                },
            );
            s.clients.insert(
                2,
                ClientHandle {
                    writer: Arc::new(Mutex::new(b_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0020_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::new(),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                },
            );
        }

        let mut map = StdHashMap::new();
        map.insert(0xCAFE_u32, ResourceId(0x0010_0002));
        let xid_map = Arc::new(StdMutex::new(map));

        // Motion with button 1 held (state bit 8 == 0x100).
        pointer_event_fanout(
            &state,
            &xid_map,
            HostPointerEvent {
                kind: PointerEventKind::MotionNotify,
                host_xid: 0xCAFE,
                detail: 0,
                time: 0,
                root_x: 5,
                root_y: 5,
                event_x: 5,
                event_y: 5,
                state: 0x0100,
            },
        );

        let mut buf = [0u8; 32];
        let a_read = a_reader_remote.read(&mut buf);
        assert!(
            matches!(a_read, Ok(32)),
            "client with ButtonMotion mask should receive 32-byte MotionNotify when a button is held; got {a_read:?}",
        );
        assert_eq!(buf[0], 6, "event type should be MotionNotify");

        let b_read = b_reader_remote.read(&mut buf);
        assert!(
            matches!(b_read, Err(ref e) if e.kind() == ErrorKind::WouldBlock),
            "client with no motion mask must not receive motion; got {b_read:?}",
        );

        // Motion without any button held: ButtonMotion subscriber must NOT receive.
        pointer_event_fanout(
            &state,
            &xid_map,
            HostPointerEvent {
                kind: PointerEventKind::MotionNotify,
                host_xid: 0xCAFE,
                detail: 0,
                time: 0,
                root_x: 5,
                root_y: 5,
                event_x: 5,
                event_y: 5,
                state: 0,
            },
        );
        let a_read2 = a_reader_remote.read(&mut buf);
        assert!(
            matches!(a_read2, Err(ref e) if e.kind() == ErrorKind::WouldBlock),
            "ButtonMotion-only subscriber must NOT receive motion when no button is held; got {a_read2:?}",
        );
    }

    #[test]
    fn pointer_event_fanout_drops_unknown_host_xid() {
        use std::{collections::HashMap as StdHashMap, sync::Mutex as StdMutex};

        use crate::host_x11::{HostPointerEvent, PointerEventKind};

        let (a_writer_local, _a_reader_remote) = UnixStream::pair().expect("socketpair");

        let state = StdMutex::new(ServerState::new());
        {
            let mut s = state.lock().unwrap();
            s.clients.insert(
                1,
                ClientHandle {
                    writer: Arc::new(Mutex::new(a_writer_local)),
                    byte_order: ClientByteOrder::LittleEndian,
                    last_sequence: Arc::new(AtomicU16::new(0)),
                    resource_id_base: 0x0010_0000,
                    resource_id_mask: 0x000F_FFFF,
                    event_masks: HashMap::from([(ResourceId(0x0010_0002), 0x0000_0004)]),
                    save_set: HashSet::new(),
                    big_requests_enabled: false,
                    xi2_masks: HashMap::new(),
                },
            );
        }

        let xid_map: crate::host_x11::HostXidMap = Arc::new(StdMutex::new(StdHashMap::new())); // empty

        pointer_event_fanout(
            &state,
            &xid_map,
            HostPointerEvent {
                kind: PointerEventKind::ButtonPress,
                host_xid: 0xCAFE, // not in map
                detail: 1,
                time: 0,
                root_x: 0,
                root_y: 0,
                event_x: 0,
                event_y: 0,
                state: 0,
            },
        );

        assert!(state.lock().unwrap().clients.contains_key(&1));
    }

    #[test]
    fn key_grab_lookup_exact_match() {
        let mut s = ServerState::new();
        let win = ResourceId(0x42);
        let owner = ClientId(1);
        s.key_grabs.push(KeyGrab {
            owner,
            grab_window: win,
            keycode: 24,
            modifiers: 0x0040,
            owner_events: false,
            pointer_mode: 1,
            keyboard_mode: 1,
        });
        let hit = s.find_key_grab(win, 24, 0x0040);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().owner, owner);
    }

    #[test]
    fn key_grab_lookup_any_modifier_wildcard() {
        let mut s = ServerState::new();
        let win = ResourceId(0x42);
        s.key_grabs.push(KeyGrab {
            owner: ClientId(1),
            grab_window: win,
            keycode: 24,
            modifiers: 0x8000,
            owner_events: false,
            pointer_mode: 1,
            keyboard_mode: 1,
        });
        assert!(s.find_key_grab(win, 24, 0x0040).is_some());
        assert!(s.find_key_grab(win, 24, 0x0000).is_some());
        assert!(s.find_key_grab(win, 25, 0x0040).is_none());
    }

    #[test]
    fn key_grab_lookup_any_keycode_wildcard() {
        let mut s = ServerState::new();
        let win = ResourceId(0x42);
        s.key_grabs.push(KeyGrab {
            owner: ClientId(1),
            grab_window: win,
            keycode: 0,
            modifiers: 0x0040,
            owner_events: false,
            pointer_mode: 1,
            keyboard_mode: 1,
        });
        assert!(s.find_key_grab(win, 24, 0x0040).is_some());
        assert!(s.find_key_grab(win, 99, 0x0040).is_some());
        assert!(s.find_key_grab(win, 24, 0x0000).is_none());
    }

    #[test]
    fn active_keyboard_grab_set_and_clear() {
        let mut s = ServerState::new();
        assert!(s.active_keyboard_grab.is_none());
        s.active_keyboard_grab = Some(ActiveKeyboardGrab {
            owner: ClientId(7),
            grab_window: ResourceId(0xff),
            source: ActiveKeyboardGrabSource::Explicit,
        });
        assert_eq!(s.active_keyboard_grab.unwrap().owner, ClientId(7));
        s.active_keyboard_grab = None;
        assert!(s.active_keyboard_grab.is_none());
    }
}
