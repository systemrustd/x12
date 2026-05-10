//! Messages flowing into the single-threaded core loop.
//!
//! See `docs/superpowers/plans/2026-05-06-single-threaded-core.md` Phase B.

use std::os::{fd::OwnedFd, unix::net::UnixStream};

use yserver_protocol::x11::{ClientByteOrder, ClientId, RequestHeader, SequenceNumber};

use crate::host_x11::HostKeyEvent;

/// All inbound messages multiplexed onto the core thread.
///
/// Reader threads, the libinput thread, the signalfd watcher, and setup
/// threads all turn their respective fds into `Message`s and send them
/// through the unbounded `crossbeam-channel`. The core's mio poller
/// owns directly-attached fds (listener, client writers, drm, libinput,
/// host-X11, signalfd) and never reads bytes off them via the channel.
#[derive(Debug)]
pub enum Message {
    /// Sent by a setup thread mid-handshake. The core allocates resource
    /// IDs and snapshots screen geometry, replies via `response_tx`, and
    /// never blocks.
    SetupAllocate {
        id: ClientId,
        response_tx: crossbeam_channel::Sender<SetupAllocateResponse>,
    },
    /// Setup thread finished writing `setup_success`; hands the
    /// (still-blocking) stream to the core for split / register / reader
    /// spawn (D4).
    ClientSetupComplete {
        id: ClientId,
        stream: UnixStream,
        resource_id_base: u32,
        resource_id_mask: u32,
        byte_order: ClientByteOrder,
    },
    /// One framed X11 request from a client reader thread.
    Request {
        id: ClientId,
        sequence: SequenceNumber,
        header: RequestHeader,
        body: Vec<u8>,
        attached_fd: Option<OwnedFd>,
    },
    /// Reader thread (or write-side disconnect detection) noticed the
    /// client socket is gone.
    ClientDisconnected {
        id: ClientId,
        reason: std::io::Error,
    },
    /// Host input event (KMS libinput producer, or host-X11 dispatch
    /// after F2).
    HostInput(HostInputEvent),
    /// DRM completion fd is readable; backend should drain page-flip
    /// completions and submit the next composite if needed.
    PageFlipReady,
    /// signalfd readable.
    Shutdown,
    /// SIGUSR1 received — the backend should dump the current scanout
    /// buffer to a file in cwd for offline inspection. Diagnostic-only;
    /// no-op for backends that don't drive their own composite.
    DumpScanout,
}

#[derive(Debug)]
pub enum HostInputEvent {
    PointerMotion {
        x: i32,
        y: i32,
        time: u32,
    },
    PointerButton {
        /// Linux input button code (`BTN_LEFT = 0x110`, `BTN_RIGHT = 0x111`,
        /// `BTN_MIDDLE = 0x112`, etc.). u16 because libinput codes are
        /// always < 0x200 and u8 would silently truncate `BTN_LEFT` to
        /// `0x10` — the KMS backend's `0x110 => 1` mapping then never
        /// matched and clicks were dropped.
        button: u16,
        pressed: bool,
        time: u32,
    },
    Key(HostKeyEvent),
}

/// Reply from the core to a setup thread's `SetupAllocate` request.
///
/// `resource_id_base == 0` signals the id allocator is exhausted; the
/// setup thread then writes `setup_failed` to its peer and exits.
#[derive(Debug)]
pub struct SetupAllocateResponse {
    pub resource_id_base: u32,
    pub resource_id_mask: u32,
    pub screen_width_px: u16,
    pub screen_height_px: u16,
    pub current_input_masks: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_variant_matches() {
        assert!(matches!(Message::Shutdown, Message::Shutdown));
    }
}
