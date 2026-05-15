//! Sender-only libinput thread for the single-threaded core.
//!
//! The thread owns the `SendContext` and an `epoll` set wrapping the
//! libinput fd. Each batch of `crate::input::InputEvent`s gets mapped
//! to `HostInputEvent`s and pushed onto the core's message channel.
//! Consecutive `PointerMotion` events are coalesced — at most one
//! motion stays in flight to the core at any given moment, and the
//! latest position wins. Buttons and keys are never coalesced and
//! flush any pending motion immediately.
//!
//! Cursor accumulation lives on this thread (relative deltas + clamped
//! absolute mappings). The backend keeps its own cursor mirror updated
//! when it receives `HostInputEvent::PointerMotion`. A brief skew is
//! tolerable.
//!
//! Spec: `docs/superpowers/specs/2026-05-05-single-threaded-core-design.md`
//! Plan: `docs/superpowers/plans/2026-05-06-single-threaded-core.md` §E2.

use std::{io, os::fd::BorrowedFd};

use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use yserver_core::{
    core_loop::{
        CoreSender, HostInputEvent, Message, SYNTH_SCROLL_DOWN, SYNTH_SCROLL_LEFT,
        SYNTH_SCROLL_RIGHT, SYNTH_SCROLL_UP,
    },
    host_x11::HostKeyEvent,
};

use crate::input::{InputEvent, SendContext};

// Linux evdev keycodes (raw, before the X11 +8 translation). Used by
// the Ctrl-Alt-Backspace zap detector and the Ctrl-Alt-Enter dump-
// scanout hotkey: the Linux scancodes are the unambiguous source of
// truth — the X side may rewrite the keymap or have a grabbing client
// consuming modifiers.
const LINUX_KEY_ENTER: u32 = 28;
const LINUX_KEY_BACKSPACE: u32 = 14;
const LINUX_KEY_LEFTCTRL: u32 = 29;
const LINUX_KEY_LEFTALT: u32 = 56;
const LINUX_KEY_RIGHTCTRL: u32 = 97;
const LINUX_KEY_RIGHTALT: u32 = 100;

/// Server-internal hotkeys recognised on the libinput thread before
/// events are forwarded to X dispatch. The keypress that triggers the
/// hotkey is intentionally NOT forwarded to clients — zap drops it
/// because we're shutting down, and the scanout dump drops it to
/// avoid an Enter sneaking into a focused client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hotkey {
    /// Ctrl+Alt+Backspace — emergency shutdown.
    Zap,
    /// Ctrl+Alt+Enter — diagnostic scanout dump (same code path as
    /// SIGUSR1).
    DumpScanout,
}

/// Cursor accumulator + framebuffer dimensions held on the libinput
/// thread.
#[derive(Debug, Clone, Copy)]
pub struct LibinputThreadState {
    cursor_x: f64,
    cursor_y: f64,
    fb_w: u32,
    fb_h: u32,
    /// Modifier-key state for the Ctrl+Alt+Backspace "zap" emergency
    /// shutdown. Tracked on this thread (off the kernel evdev codes)
    /// rather than on the X side because a grabbing client or a
    /// remapped keymap could silently consume the modifier press —
    /// zap needs to fire even when the X dispatch is wedged, since
    /// that's the most likely reason the user is reaching for it.
    ctrl_pressed: bool,
    alt_pressed: bool,
    /// Sub-click scroll accumulators in v120 units. libinput's high-
    /// resolution wheel and finger/continuous scroll arrive as small
    /// v120 deltas that may not add up to a full 120-unit click in one
    /// event. We bank the remainder here and emit a button-4/5/6/7
    /// press+release pair each time the absolute accumulator crosses
    /// 120. Sign convention matches libinput: positive Y = scroll down,
    /// positive X = scroll right.
    scroll_accum_x_v120: i32,
    scroll_accum_y_v120: i32,
}

impl LibinputThreadState {
    #[must_use]
    pub fn new(fb_w: u32, fb_h: u32) -> Self {
        Self {
            cursor_x: f64::from(fb_w) / 2.0,
            cursor_y: f64::from(fb_h) / 2.0,
            fb_w,
            fb_h,
            ctrl_pressed: false,
            alt_pressed: false,
            scroll_accum_x_v120: 0,
            scroll_accum_y_v120: 0,
        }
    }

    /// Update tracked modifier state for `ev` and return the hotkey
    /// fired by this event, if any. Ctrl+Alt+Backspace fires
    /// [`Hotkey::Zap`]; Ctrl+Alt+Enter fires [`Hotkey::DumpScanout`].
    ///
    /// Modifier release events update state but never fire a hotkey;
    /// non-key events are ignored.
    fn check_hotkey(&mut self, ev: &InputEvent) -> Option<Hotkey> {
        match *ev {
            InputEvent::KeyPress { keycode } => match keycode {
                LINUX_KEY_LEFTCTRL | LINUX_KEY_RIGHTCTRL => {
                    self.ctrl_pressed = true;
                    None
                }
                LINUX_KEY_LEFTALT | LINUX_KEY_RIGHTALT => {
                    self.alt_pressed = true;
                    None
                }
                LINUX_KEY_BACKSPACE if self.ctrl_pressed && self.alt_pressed => Some(Hotkey::Zap),
                LINUX_KEY_ENTER if self.ctrl_pressed && self.alt_pressed => {
                    Some(Hotkey::DumpScanout)
                }
                _ => None,
            },
            InputEvent::KeyRelease { keycode } => {
                match keycode {
                    LINUX_KEY_LEFTCTRL | LINUX_KEY_RIGHTCTRL => self.ctrl_pressed = false,
                    LINUX_KEY_LEFTALT | LINUX_KEY_RIGHTALT => self.alt_pressed = false,
                    _ => {}
                }
                None
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn cursor(&self) -> (f64, f64) {
        (self.cursor_x, self.cursor_y)
    }

    /// Translate one libinput event into a `HostInputEvent`.
    ///
    /// `time_ms` lets tests pin the timestamp; production callers pass
    /// the wall clock.
    fn map(&mut self, ev: InputEvent, time_ms: u32) -> HostInputEvent {
        match ev {
            InputEvent::KeyPress { keycode } => HostInputEvent::Key(HostKeyEvent {
                pressed: true,
                keycode: ((keycode + 8) & 0xff) as u8,
                time: time_ms,
                root_x: self.cursor_x as i16,
                root_y: self.cursor_y as i16,
                event_x: self.cursor_x as i16,
                event_y: self.cursor_y as i16,
                state: 0,
            }),
            InputEvent::KeyRelease { keycode } => HostInputEvent::Key(HostKeyEvent {
                pressed: false,
                keycode: ((keycode + 8) & 0xff) as u8,
                time: time_ms,
                root_x: self.cursor_x as i16,
                root_y: self.cursor_y as i16,
                event_x: self.cursor_x as i16,
                event_y: self.cursor_y as i16,
                state: 0,
            }),
            InputEvent::PointerMotion { dx, dy } => {
                self.cursor_x =
                    (self.cursor_x + dx).clamp(0.0, f64::from(self.fb_w).max(1.0) - 1.0);
                self.cursor_y =
                    (self.cursor_y + dy).clamp(0.0, f64::from(self.fb_h).max(1.0) - 1.0);
                HostInputEvent::PointerMotion {
                    x: self.cursor_x as i32,
                    y: self.cursor_y as i32,
                    time: time_ms,
                }
            }
            InputEvent::PointerMotionAbsolute { x_norm, y_norm } => {
                self.cursor_x = x_norm.clamp(0.0, 1.0) * (f64::from(self.fb_w).max(1.0) - 1.0);
                self.cursor_y = y_norm.clamp(0.0, 1.0) * (f64::from(self.fb_h).max(1.0) - 1.0);
                HostInputEvent::PointerMotion {
                    x: self.cursor_x as i32,
                    y: self.cursor_y as i32,
                    time: time_ms,
                }
            }
            InputEvent::Button { code, pressed } => HostInputEvent::PointerButton {
                button: u16::try_from(code).unwrap_or(u16::MAX),
                pressed,
                time: time_ms,
            },
            // PointerScroll is fanned out separately via `drain_scroll`
            // because it can map to N (≥ 0) press+release pairs depending
            // on accumulated v120. Reaching here means a caller forgot
            // to route it; map to a no-op-ish placeholder.
            InputEvent::PointerScroll { .. } => HostInputEvent::PointerButton {
                button: u16::MAX,
                pressed: false,
                time: time_ms,
            },
        }
    }

    /// One v120 click step. libinput emits high-resolution wheel deltas
    /// in 120ths of a logical click; we accumulate fractional deltas and
    /// fire a button press+release each time |accum| crosses this.
    const V120_PER_CLICK: i32 = 120;

    /// Accumulate a scroll delta and emit press+release pairs for any
    /// completed clicks. `dy_v120 > 0` → scroll-down (button 5);
    /// `dy_v120 < 0` → scroll-up (button 4). Horizontal axis maps to
    /// button 6 (left) / 7 (right). Mixed-axis events emit Y clicks
    /// first then X clicks within a single call.
    fn drain_scroll(
        &mut self,
        dx_v120: i32,
        dy_v120: i32,
        time_ms: u32,
        out: &mut Vec<HostInputEvent>,
    ) {
        self.scroll_accum_x_v120 = self.scroll_accum_x_v120.saturating_add(dx_v120);
        self.scroll_accum_y_v120 = self.scroll_accum_y_v120.saturating_add(dy_v120);

        // Vertical first (more common; matches X11 button-4/5 priority).
        while self.scroll_accum_y_v120 >= Self::V120_PER_CLICK {
            self.scroll_accum_y_v120 -= Self::V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_DOWN, time_ms);
        }
        while self.scroll_accum_y_v120 <= -Self::V120_PER_CLICK {
            self.scroll_accum_y_v120 += Self::V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_UP, time_ms);
        }
        while self.scroll_accum_x_v120 >= Self::V120_PER_CLICK {
            self.scroll_accum_x_v120 -= Self::V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_RIGHT, time_ms);
        }
        while self.scroll_accum_x_v120 <= -Self::V120_PER_CLICK {
            self.scroll_accum_x_v120 += Self::V120_PER_CLICK;
            push_button_click(out, SYNTH_SCROLL_LEFT, time_ms);
        }
    }
}

fn push_button_click(out: &mut Vec<HostInputEvent>, button: u16, time_ms: u32) {
    out.push(HostInputEvent::PointerButton {
        button,
        pressed: true,
        time: time_ms,
    });
    out.push(HostInputEvent::PointerButton {
        button,
        pressed: false,
        time: time_ms,
    });
}

/// Process one batch of libinput events with motion coalescing.
///
/// Across the batch (and the carry-over from a previous batch via
/// `pending_motion`), at most one `PointerMotion` remains queued for
/// the core. Any non-motion event flushes the pending motion before
/// being sent. The caller drains `pending_motion` between batches if
/// it wants the core to see end-of-burst movement before the next
/// `epoll_wait`.
///
/// Per the plan (§E2), this is the function under test:
/// feeding `[Motion, Motion, Motion, Button, Motion, Motion, Motion]`
/// must produce three sender messages — `Motion(latest), Button,
/// Motion(latest)` — not seven.
pub fn process_batch(
    state: &mut LibinputThreadState,
    sender: &CoreSender,
    pending_motion: &mut Option<HostInputEvent>,
    events: impl IntoIterator<Item = InputEvent>,
    time_ms: u32,
) -> io::Result<()> {
    let mut scroll_buf: Vec<HostInputEvent> = Vec::new();
    for raw in events {
        match state.check_hotkey(&raw) {
            Some(Hotkey::Zap) => {
                // Drop any pending motion + the Backspace event itself —
                // the server is shutting down, no client should see them.
                *pending_motion = None;
                log::warn!("yserver: Ctrl-Alt-Backspace pressed — requesting shutdown (zap)");
                sender.send(Message::Shutdown)?;
                return Ok(());
            }
            Some(Hotkey::DumpScanout) => {
                // Flush any queued motion so the input stream stays
                // ordered, drop the Enter keypress itself, and ask the
                // core to dump the scanout (same code path as SIGUSR1).
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                log::info!("yserver: Ctrl-Alt-Enter pressed — dumping scanout");
                sender.send(Message::DumpScanout)?;
                continue;
            }
            None => {}
        }
        // Scroll fans out separately because one InputEvent may map to
        // zero or many press+release pairs depending on accumulated v120.
        if let InputEvent::PointerScroll { dx_v120, dy_v120 } = raw {
            scroll_buf.clear();
            state.drain_scroll(dx_v120, dy_v120, time_ms, &mut scroll_buf);
            if !scroll_buf.is_empty() {
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                for ev in scroll_buf.drain(..) {
                    sender.send(Message::HostInput(ev))?;
                }
            }
            continue;
        }
        let mapped = state.map(raw, time_ms);
        match mapped {
            HostInputEvent::PointerMotion { .. } => {
                *pending_motion = Some(mapped);
            }
            non_motion => {
                if let Some(m) = pending_motion.take() {
                    sender.send(Message::HostInput(m))?;
                }
                sender.send(Message::HostInput(non_motion))?;
            }
        }
    }
    Ok(())
}

/// Long-running libinput thread body. Owns `input_ctx`, drives an
/// `epoll` set on its fd, dispatches batches through [`process_batch`],
/// and flushes any leftover pending motion at the end of each batch so
/// the core never sees stale "latest motion" sitting in the channel.
///
/// Returns only on a fatal send error (channel closed = core gone).
pub fn run(input_ctx: SendContext, sender: CoreSender, fb_w: u32, fb_h: u32) -> io::Result<()> {
    let mut input_ctx = input_ctx;
    let mut state = LibinputThreadState::new(fb_w, fb_h);
    let mut pending_motion: Option<HostInputEvent> = None;

    let input_epoll = Epoll::new(EpollCreateFlags::empty())
        .map_err(|err| io::Error::other(format!("input thread epoll_create: {err}")))?;
    let fd = input_ctx.fd();
    // SAFETY: `input_ctx` outlives this borrow because both live for
    // the duration of `run`.
    let borrow = unsafe { BorrowedFd::borrow_raw(fd) };
    input_epoll
        .add(borrow, EpollEvent::new(EpollFlags::EPOLLIN, 0))
        .map_err(|err| io::Error::other(format!("input thread epoll_add: {err}")))?;

    // Drain udev's initial device enumeration before waiting on
    // epoll. udev_assign_seat queues DeviceAdded events synchronously
    // but dispatch() must be called once to consume them. Without this
    // first dispatch, the very first epoll_wait blocks until any input
    // arrives, and the seat enumeration is silently held back. This is
    // also where `libinput: device added` first lands in the log.
    {
        let initial = match input_ctx.dispatch() {
            Ok(evs) => evs,
            Err(err) => {
                log::warn!("input thread: initial libinput dispatch: {err}");
                Vec::new()
            }
        };
        if !initial.is_empty() {
            let time_ms = current_time_ms();
            process_batch(&mut state, &sender, &mut pending_motion, initial, time_ms)?;
            if let Some(m) = pending_motion.take() {
                sender.send(Message::HostInput(m))?;
            }
        }
    }

    let mut buf = [EpollEvent::empty(); 4];
    loop {
        match input_epoll.wait(&mut buf, EpollTimeout::NONE) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(err) => {
                log::warn!("input thread: epoll_wait: {err}");
                continue;
            }
        }

        let events = match input_ctx.dispatch() {
            Ok(evs) => evs,
            Err(err) => {
                log::warn!("input thread: libinput dispatch: {err}");
                continue;
            }
        };

        let time_ms = current_time_ms();
        process_batch(&mut state, &sender, &mut pending_motion, events, time_ms)?;
        // Flush any pending motion before the next epoll_wait so the
        // core sees end-of-burst movement promptly. A subsequent batch
        // whose first event is another motion just starts coalescing
        // fresh.
        if let Some(m) = pending_motion.take() {
            sender.send(Message::HostInput(m))?;
        }
    }
}

fn current_time_ms() -> u32 {
    crate::clock::server_time_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    use yserver_core::core_loop::channel;

    #[test]
    fn maps_relative_motion_to_clamped_absolute() {
        let mut s = LibinputThreadState::new(800, 600);
        // Center: (400, 300)
        assert_eq!(s.cursor(), (400.0, 300.0));
        let ev = s.map(
            InputEvent::PointerMotion {
                dx: 50.0,
                dy: -100.0,
            },
            0,
        );
        assert!(matches!(
            ev,
            HostInputEvent::PointerMotion { x: 450, y: 200, .. }
        ));
        // Walk past the right edge — clamps to fb_w-1.
        let _ = s.map(
            InputEvent::PointerMotion {
                dx: 1000.0,
                dy: 0.0,
            },
            0,
        );
        let (cx, _) = s.cursor();
        assert!((cx - 799.0).abs() < 0.5, "cursor_x = {cx}");
    }

    #[test]
    fn maps_absolute_motion_to_scanout_pixels() {
        let mut s = LibinputThreadState::new(800, 600);
        let ev = s.map(
            InputEvent::PointerMotionAbsolute {
                x_norm: 0.5,
                y_norm: 0.25,
            },
            42,
        );
        match ev {
            HostInputEvent::PointerMotion { x, y, time } => {
                assert_eq!(time, 42);
                // 0.5 * 799 ≈ 399.5 → 399 (truncation when cast to i32)
                assert!((x - 399).abs() <= 1, "x = {x}");
                // 0.25 * 599 ≈ 149.75 → 149
                assert!((y - 149).abs() <= 1, "y = {y}");
            }
            other => panic!("expected PointerMotion, got {other:?}"),
        }
    }

    #[test]
    fn maps_buttons_and_keys_without_state_mutation() {
        let mut s = LibinputThreadState::new(800, 600);
        let before = s.cursor();
        let btn = s.map(
            InputEvent::Button {
                code: 0x110,
                pressed: true,
            },
            7,
        );
        match btn {
            HostInputEvent::PointerButton {
                button,
                pressed,
                time,
            } => {
                assert_eq!(button, 0x110);
                assert!(pressed);
                assert_eq!(time, 7);
            }
            other => panic!("expected PointerButton, got {other:?}"),
        }
        let key = s.map(InputEvent::KeyPress { keycode: 30 }, 8);
        match key {
            HostInputEvent::Key(ev) => {
                assert!(ev.pressed);
                assert_eq!(ev.keycode, 38); // 30 + 8 (evdev → X11)
                assert_eq!(ev.time, 8);
            }
            other => panic!("expected Key, got {other:?}"),
        }
        assert_eq!(s.cursor(), before, "buttons/keys must not move the cursor");
    }

    /// Headline test from plan §E2: a batch of 5 motions + 1 button +
    /// 3 motions yields exactly three sender messages — last motion,
    /// button, last motion.
    #[test]
    fn process_batch_coalesces_consecutive_motions() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        let batch = vec![
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::Button {
                code: 1,
                pressed: true,
            },
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
            InputEvent::PointerMotion { dx: 1.0, dy: 1.0 },
        ];
        process_batch(&mut state, &sender, &mut pending, batch, 100).unwrap();
        // End-of-batch flush (matches the production loop in `run`).
        if let Some(m) = pending.take() {
            sender.send(Message::HostInput(m)).unwrap();
        }

        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert_eq!(
            collected.len(),
            3,
            "expected 3 messages (motion, button, motion); got {}: {collected:?}",
            collected.len()
        );
        match &collected[0] {
            Message::HostInput(HostInputEvent::PointerMotion { x: 405, y: 305, .. }) => {}
            other => panic!("first message: {other:?}"),
        }
        match &collected[1] {
            Message::HostInput(HostInputEvent::PointerButton {
                button: 1,
                pressed: true,
                ..
            }) => {}
            other => panic!("second message: {other:?}"),
        }
        match &collected[2] {
            Message::HostInput(HostInputEvent::PointerMotion { x: 408, y: 308, .. }) => {}
            other => panic!("third message: {other:?}"),
        }
        // Silence unused warning on `poll` — we just need its waker
        // alive for the channel to function.
        drop(poll);
    }

    #[test]
    fn process_batch_carries_motion_across_batches() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        // Batch A: motion only — left in `pending`, nothing sent yet.
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [InputEvent::PointerMotion { dx: 5.0, dy: 0.0 }],
            1,
        )
        .unwrap();
        assert!(pending.is_some());
        let immediate: Vec<Message> = rx.try_recv_all().collect();
        assert!(immediate.is_empty(), "no flush yet, got {immediate:?}");

        // Batch B: motion then button — only the latest combined
        // motion + button get sent.
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [
                InputEvent::PointerMotion { dx: 10.0, dy: 0.0 },
                InputEvent::Button {
                    code: 1,
                    pressed: true,
                },
            ],
            2,
        )
        .unwrap();
        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert_eq!(collected.len(), 2);
        match &collected[0] {
            Message::HostInput(HostInputEvent::PointerMotion { x: 415, y: 300, .. }) => {}
            other => panic!("first message: {other:?}"),
        }
        match &collected[1] {
            Message::HostInput(HostInputEvent::PointerButton {
                button: 1,
                pressed: true,
                ..
            }) => {}
            other => panic!("second message: {other:?}"),
        }
        drop(poll);
    }

    #[test]
    fn ctrl_alt_backspace_emits_shutdown_and_drops_keypress() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_LEFTCTRL,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_LEFTALT,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_BACKSPACE,
                },
                // Anything after the zap is dropped — the server is
                // already shutting down. This press must NOT reach
                // the core.
                InputEvent::KeyPress {
                    keycode: 30, /* a */
                },
            ],
            0,
        )
        .unwrap();

        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert!(
            collected.iter().any(|m| matches!(m, Message::Shutdown)),
            "expected Shutdown in {collected:?}",
        );
        assert!(
            !collected.iter().any(|m| matches!(
                m,
                Message::HostInput(HostInputEvent::Key(ev)) if ev.pressed && ev.keycode == 14 + 8
            )),
            "Backspace keypress must not be forwarded after zap, got {collected:?}",
        );
        // Modifier presses before the Backspace landed first; tolerate
        // those since they were valid client events at the time.
        drop(poll);
    }

    #[test]
    fn backspace_alone_does_not_zap() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [InputEvent::KeyPress {
                keycode: LINUX_KEY_BACKSPACE,
            }],
            0,
        )
        .unwrap();
        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert!(
            !collected.iter().any(|m| matches!(m, Message::Shutdown)),
            "Shutdown must not fire on lone Backspace, got {collected:?}",
        );
        drop(poll);
    }

    #[test]
    fn modifier_release_disarms_zap() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_LEFTCTRL,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_LEFTALT,
                },
                InputEvent::KeyRelease {
                    keycode: LINUX_KEY_LEFTCTRL,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_BACKSPACE,
                },
            ],
            0,
        )
        .unwrap();
        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert!(
            !collected.iter().any(|m| matches!(m, Message::Shutdown)),
            "Shutdown must not fire after Ctrl release, got {collected:?}",
        );
        drop(poll);
    }

    #[test]
    fn right_modifiers_also_arm_zap() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_RIGHTCTRL,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_RIGHTALT,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_BACKSPACE,
                },
            ],
            0,
        )
        .unwrap();
        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert!(
            collected.iter().any(|m| matches!(m, Message::Shutdown)),
            "right Ctrl + right Alt + Backspace must zap, got {collected:?}",
        );
        drop(poll);
    }

    #[test]
    fn ctrl_alt_enter_emits_dump_scanout_and_drops_keypress() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_LEFTCTRL,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_LEFTALT,
                },
                InputEvent::KeyPress {
                    keycode: LINUX_KEY_ENTER,
                },
            ],
            0,
        )
        .unwrap();

        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert!(
            collected.iter().any(|m| matches!(m, Message::DumpScanout)),
            "expected DumpScanout in {collected:?}",
        );
        assert!(
            !collected.iter().any(|m| matches!(
                m,
                Message::HostInput(HostInputEvent::Key(ev)) if ev.pressed && ev.keycode == 28 + 8
            )),
            "Enter keypress must not be forwarded after dump-scanout hotkey, got {collected:?}",
        );
        drop(poll);
    }

    #[test]
    fn enter_alone_does_not_dump_scanout() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [InputEvent::KeyPress {
                keycode: LINUX_KEY_ENTER,
            }],
            0,
        )
        .unwrap();
        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert!(
            !collected.iter().any(|m| matches!(m, Message::DumpScanout)),
            "DumpScanout must not fire on lone Enter, got {collected:?}",
        );
        drop(poll);
    }

    fn collect_button_codes(msgs: &[Message]) -> Vec<(u16, bool)> {
        msgs.iter()
            .filter_map(|m| match m {
                Message::HostInput(HostInputEvent::PointerButton {
                    button, pressed, ..
                }) => Some((*button, *pressed)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn scroll_one_click_down_emits_press_release_pair() {
        let (poll, sender, rx) = channel().expect("channel");
        let mut state = LibinputThreadState::new(800, 600);
        let mut pending: Option<HostInputEvent> = None;
        process_batch(
            &mut state,
            &sender,
            &mut pending,
            [InputEvent::PointerScroll {
                dx_v120: 0,
                dy_v120: 120,
            }],
            7,
        )
        .unwrap();
        let collected: Vec<Message> = rx.try_recv_all().collect();
        assert_eq!(
            collect_button_codes(&collected),
            vec![(SYNTH_SCROLL_DOWN, true), (SYNTH_SCROLL_DOWN, false)],
            "expected one scroll-down press+release pair, got {collected:?}"
        );
        drop(poll);
    }

    #[test]
    fn scroll_accumulates_subclick_v120() {
        let mut state = LibinputThreadState::new(800, 600);
        let mut out = Vec::new();
        // 60 + 30 + 40 = 130 → one click; remainder 10 banked.
        state.drain_scroll(0, 60, 0, &mut out);
        assert!(out.is_empty(), "60 < 120, no emission yet");
        state.drain_scroll(0, 30, 0, &mut out);
        assert!(out.is_empty(), "60 + 30 = 90 < 120");
        state.drain_scroll(0, 40, 0, &mut out);
        assert_eq!(
            out.len(),
            2,
            "60 + 30 + 40 = 130 should emit one press+release pair"
        );
        assert!(matches!(
            out[0],
            HostInputEvent::PointerButton {
                button: SYNTH_SCROLL_DOWN,
                pressed: true,
                ..
            }
        ));
    }

    #[test]
    fn scroll_negative_v120_emits_scroll_up() {
        let mut state = LibinputThreadState::new(800, 600);
        let mut out = Vec::new();
        state.drain_scroll(0, -120, 0, &mut out);
        assert_eq!(out.len(), 2);
        assert!(matches!(
            out[0],
            HostInputEvent::PointerButton {
                button: SYNTH_SCROLL_UP,
                pressed: true,
                ..
            }
        ));
    }

    #[test]
    fn scroll_multiple_clicks_in_one_event() {
        let mut state = LibinputThreadState::new(800, 600);
        let mut out = Vec::new();
        // 480 v120 = exactly 4 clicks down.
        state.drain_scroll(0, 480, 0, &mut out);
        assert_eq!(out.len(), 8, "4 clicks × (press + release)");
        for chunk in out.chunks_exact(2) {
            assert!(matches!(
                chunk[0],
                HostInputEvent::PointerButton {
                    button: SYNTH_SCROLL_DOWN,
                    pressed: true,
                    ..
                }
            ));
            assert!(matches!(
                chunk[1],
                HostInputEvent::PointerButton {
                    button: SYNTH_SCROLL_DOWN,
                    pressed: false,
                    ..
                }
            ));
        }
    }

    #[test]
    fn scroll_horizontal_emits_buttons_6_7() {
        let mut state = LibinputThreadState::new(800, 600);
        let mut out = Vec::new();
        state.drain_scroll(120, 0, 0, &mut out);
        assert!(matches!(
            out[0],
            HostInputEvent::PointerButton {
                button: SYNTH_SCROLL_RIGHT,
                pressed: true,
                ..
            }
        ));
        out.clear();
        state.drain_scroll(-120, 0, 0, &mut out);
        assert!(matches!(
            out[0],
            HostInputEvent::PointerButton {
                button: SYNTH_SCROLL_LEFT,
                pressed: true,
                ..
            }
        ));
    }
}
