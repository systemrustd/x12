//! Deferred PRESENT completion queue (Stage 5 Task 6.1).
//!
//! Owns per-entry state for the v2 backend's `enqueue_present_completion`
//! and `drain_completed_present_events` trait impls. Internal types
//! never escape the `yserver` crate; the trait surface exchanges
//! the public `CompletedPresentEvent` only.
//!
//! Spec: `docs/superpowers/specs/2026-05-23-deferred-present-completion-design.md`.

use std::{os::fd::OwnedFd, sync::Arc};

use yserver_core::backend::{CompletedPresentEvent, SyncobjHandle, XshmfenceHandle};

use crate::kms::v2::platform::FenceTicket;

/// One queued PRESENT awaiting cow_batch retirement. The drain
/// fires the wake signal via `wake_pin` + returns the `event`
/// payload to the main loop.
pub(crate) struct PendingPresentEntry {
    /// Cow_batch ticket the just-appended copy_area participates in.
    /// `None` when PRESENT arrived with no cow_batch in flight (the
    /// no-GPU-work fast path) — drain treats this as immediately
    /// ready. Avoids needing a synthetic "pre-signaled sentinel"
    /// FenceTicket constructor.
    pub(crate) ticket: Option<FenceTicket>,
    /// Lifetime pin on the underlying wake primitive. Survives a
    /// mid-flight `XFixesDestroyFence` / `FreeSyncobj`.
    pub(crate) wake_pin: PinnedWake,
    /// sync_file FD exported from `ticket.fence` via
    /// `vkGetFenceFdKHR(SYNC_FD)`. `None` when the fence was
    /// already signaled at enqueue time (vkGetFenceFdKHR returned
    /// -1; the wakeup_eventfd path is used instead).
    pub(crate) fence_fd: Option<OwnedFd>,
    /// Public-facing event payload, returned by `drain_*` to the
    /// main loop.
    pub(crate) event: CompletedPresentEvent,
}

/// Wake-target lifetime pin variants. The drain dispatches signal
/// via the held `Arc` regardless of whether the X11 resource id is
/// still in the registry.
pub(crate) enum PinnedWake {
    Pixmap(Arc<dyn XshmfenceHandle>),
    PixmapSynced {
        handle: Arc<dyn SyncobjHandle>,
        value: u64,
    },
    /// Client passed no wake object (idle_fence_xid == 0 or
    /// release_syncobj == 0). Drain skips the signal step; X11 event
    /// emission still happens.
    None,
}

#[cfg(test)]
mod tests {
    use super::*;
    use yserver_core::backend::PresentWake;
    use yserver_protocol::x11::ClientId;

    /// Smoke test that the types compile + can be constructed.
    /// Real semantics tested in `KmsBackendV2` integration tests.
    #[test]
    fn pinned_wake_none_constructs() {
        let pin = PinnedWake::None;
        match pin {
            PinnedWake::None => {}
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn completed_present_event_carries_payload() {
        let event = CompletedPresentEvent {
            client_id: ClientId(7),
            serial: 42,
            host_xid: 0x100001,
            dst_host_xid: 0xE00001,
            options: 0,
            wake: PresentWake::Pixmap {
                idle_fence_xid: 0xCC,
            },
        };
        assert_eq!(event.serial, 42);
    }
}
