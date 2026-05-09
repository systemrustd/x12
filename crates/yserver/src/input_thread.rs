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

use std::{
    io,
    os::fd::BorrowedFd,
    time::{SystemTime, UNIX_EPOCH},
};

use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};
use yserver_core::{
    core_loop::{CoreSender, HostInputEvent, Message},
    host_x11::HostKeyEvent,
};

use crate::input::{InputEvent, SendContext};

// Linux evdev keycodes (raw, before the X11 +8 translation). Used by
// the Ctrl-Alt-Backspace zap detector: the Linux scancodes are the
// unambiguous source of truth — the X side may rewrite the keymap or
// have a grabbing client consuming modifiers.
const LINUX_KEY_BACKSPACE: u32 = 14;
const LINUX_KEY_LEFTCTRL: u32 = 29;
const LINUX_KEY_LEFTALT: u32 = 56;
const LINUX_KEY_RIGHTCTRL: u32 = 97;
const LINUX_KEY_RIGHTALT: u32 = 100;

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
        }
    }

    /// Update tracked modifier state for `ev` and return `true` iff
    /// this event is a Backspace key press while both Ctrl and Alt
    /// are held — the zap shortcut.
    ///
    /// Modifier release events update state but never fire the zap;
    /// non-key events are ignored.
    fn check_zap(&mut self, ev: &InputEvent) -> bool {
        match *ev {
            InputEvent::KeyPress { keycode } => match keycode {
                LINUX_KEY_LEFTCTRL | LINUX_KEY_RIGHTCTRL => {
                    self.ctrl_pressed = true;
                    false
                }
                LINUX_KEY_LEFTALT | LINUX_KEY_RIGHTALT => {
                    self.alt_pressed = true;
                    false
                }
                LINUX_KEY_BACKSPACE => self.ctrl_pressed && self.alt_pressed,
                _ => false,
            },
            InputEvent::KeyRelease { keycode } => {
                match keycode {
                    LINUX_KEY_LEFTCTRL | LINUX_KEY_RIGHTCTRL => self.ctrl_pressed = false,
                    LINUX_KEY_LEFTALT | LINUX_KEY_RIGHTALT => self.alt_pressed = false,
                    _ => {}
                }
                false
            }
            _ => false,
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
        }
    }
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
    for raw in events {
        if state.check_zap(&raw) {
            // Drop any pending motion + the Backspace event itself —
            // the server is shutting down, no client should see them.
            *pending_motion = None;
            log::warn!("yserver: Ctrl-Alt-Backspace pressed — requesting shutdown (zap)");
            sender.send(Message::Shutdown)?;
            return Ok(());
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
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
}
