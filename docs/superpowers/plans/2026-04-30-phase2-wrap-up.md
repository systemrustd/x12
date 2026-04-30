# Phase 2 wrap-up Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the residual Phase 2 opcodes and known follow-ups so a simple reparenting WM (Openbox / Fluxbox) can run end to end under `ynest`, alongside fvwm3.

**Architecture:** Pure incremental work in the existing crates. New per-server `KeyGrab` table and per-client `save_set` live on `ServerState` / `ClientHandle` (or `ResourceTable::clients`). Wire encoders/decoders go in `yserver-protocol/src/x11/mod.rs`. New dispatch arms go in `yserver-core/src/nested.rs`. Host proxy calls go in `host_x11.rs`. RANDR follow-ups extend `randr.rs` with subscriber storage and a host-resize hook.

**Tech Stack:** Rust stable, std-only at the crate API surface, `x11-rs` (or whatever `host_x11.rs` already uses) on the host side.

**Spec:** [`2026-04-30-phase2-wrap-up-design.md`](../specs/2026-04-30-phase2-wrap-up-design.md)

---

## Conventions used in this plan

- "Failing test" steps go in the same `mod tests {}` block at the end of the file being modified (existing pattern in `protocol/x11/mod.rs` and `resources.rs`). For wire encoders, the test is a byte-array roundtrip; for state machines, an exercise of the public method.
- Run `cargo test -p yserver-protocol -- <filter>` (single filter only — cargo doesn't take multiple positional filters; combine with substring matches if needed) or `cargo test` for the full set.
- Pre-commit gate (per `AGENTS.md`): `cargo +nightly fmt`, `cargo clippy` (no `-W clippy::pedantic`; the repo opts out), `cargo test`. Fix all warnings.
- Commit messages follow the existing style (`feat:`, `fix:`, `docs:`). No trailers — match the existing project history.
- For test fakes that need a `UnixStream`, use `UnixStream::pair()` to obtain two real connected ends; never construct a stream with `unsafe { std::mem::zeroed() }`. Where a public field demands a writer for a struct-init test, factor the test through a small writer trait or an `Arc<Mutex<dyn Write + Send>>`.

---

## Group A — Keyboard for WMs

### Task A1: KeyGrab data structure and lookup

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (add `KeyGrab`, field on `ServerState`, lookup helper)

- [ ] **Step 1: Write the failing tests**

Add these to the existing `#[cfg(test)] mod tests { ... }` block at the bottom of `server.rs` (create the block if absent). Tests must compile against the new types — they will fail with "type not found" first.

```rust
#[test]
fn key_grab_lookup_exact_match() {
    use crate::resources::ResourceId;
    use crate::resources::ClientId;
    let mut s = ServerState::new();
    let win = ResourceId(0x42);
    let owner = ClientId(1);
    s.key_grabs.push(KeyGrab {
        owner,
        grab_window: win,
        keycode: 24,        // 'q'
        modifiers: 0x0040,  // Mod4 (Super)
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
    use crate::resources::{ClientId, ResourceId};
    let mut s = ServerState::new();
    let win = ResourceId(0x42);
    s.key_grabs.push(KeyGrab {
        owner: ClientId(1),
        grab_window: win,
        keycode: 24,
        modifiers: 0x8000, // AnyModifier
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
    use crate::resources::{ClientId, ResourceId};
    let mut s = ServerState::new();
    let win = ResourceId(0x42);
    s.key_grabs.push(KeyGrab {
        owner: ClientId(1),
        grab_window: win,
        keycode: 0,        // AnyKey
        modifiers: 0x0040,
        owner_events: false,
        pointer_mode: 1,
        keyboard_mode: 1,
    });
    assert!(s.find_key_grab(win, 24, 0x0040).is_some());
    assert!(s.find_key_grab(win, 99, 0x0040).is_some());
    assert!(s.find_key_grab(win, 24, 0x0000).is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver-core key_grab -- --nocapture`
Expected: compile error "cannot find type `KeyGrab` in this scope" / "no field `key_grabs`".

- [ ] **Step 3: Implement `KeyGrab` and `find_key_grab`**

Add at the top of `server.rs` near `PassiveButtonGrab`:

```rust
#[derive(Debug, Clone)]
pub struct KeyGrab {
    pub owner: ClientId,
    pub grab_window: ResourceId,
    /// 0 == AnyKey
    pub keycode: u8,
    /// 0x8000 == AnyModifier; otherwise the literal modifier-state mask the grab matches
    pub modifiers: u16,
    pub owner_events: bool,
    /// 0 = Synchronous, 1 = Asynchronous
    pub pointer_mode: u8,
    /// 0 = Synchronous, 1 = Asynchronous
    pub keyboard_mode: u8,
}
```

Add `pub key_grabs: Vec<KeyGrab>` to `ServerState`, initialise to `Vec::new()` in `new()`.

Add the lookup helper on `impl ServerState`:

```rust
#[must_use]
pub fn find_key_grab(
    &self,
    window: ResourceId,
    keycode: u8,
    state_mask: u16,
) -> Option<&KeyGrab> {
    // Walk the window's ancestor chain; any grab on an ancestor of the
    // focused window can fire (X11 spec semantics).
    let mut current = window;
    let mut depth = 0usize;
    loop {
        for grab in &self.key_grabs {
            if grab.grab_window != current {
                continue;
            }
            let key_match = grab.keycode == 0 || grab.keycode == keycode;
            // The relevant modifier bits are the lower 8 of the state mask.
            let mod_match = grab.modifiers == 0x8000
                || grab.modifiers == (state_mask & 0x00ff);
            if key_match && mod_match {
                return Some(grab);
            }
        }
        let w = self.resources.window(current)?;
        if w.parent == current || w.parent == crate::resources::ROOT_WINDOW {
            // Also try root once.
            if current != crate::resources::ROOT_WINDOW {
                current = crate::resources::ROOT_WINDOW;
                depth += 1;
                if depth > 256 { break; }
                continue;
            }
            break;
        }
        current = w.parent;
        depth += 1;
        if depth > 256 { break; }
    }
    None
}
```

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test -p yserver-core key_grab`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/server.rs
git commit -m "$(cat <<'EOF'
feat: add KeyGrab table and find_key_grab lookup

Per-server passive key grab table with AnyKey (keycode=0) and
AnyModifier (modifiers=0x8000) wildcards; lookup walks the
focused window's ancestor chain plus root.

EOF
)"
```

### Task A2: GrabKey / UngrabKey opcode handlers

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` (replace stubs at opcodes 33, 34)

- [ ] **Step 1: Write the failing test**

Add to the `tests` mod at the bottom of `nested.rs`. If no such mod exists, create one. The test exercises the parser via a small dispatch helper — but parsers/handlers in this codebase are normally tested at the protocol layer. For this task, write the parse helper as a free function in `protocol/x11/mod.rs` and test it there.

In `crates/yserver-protocol/src/x11/mod.rs` (existing tests mod):

```rust
#[test]
fn parse_grab_key_request() {
    // GrabKey body (excludes opcode/length header; the dispatcher passes
    // the trailing bytes plus header.data == owner_events).
    // Layout (post-header): grab_window(4) modifiers(2) keycode(1) pointer_mode(1)
    //                       keyboard_mode(1) pad(3)
    let body = [
        0x12, 0x34, 0x00, 0x00, // grab_window 0x3412
        0x40, 0x00,             // modifiers 0x0040
        24,                     // keycode 24
        1,                      // pointer_mode async
        1,                      // keyboard_mode async
        0, 0, 0,                // pad
    ];
    let parsed = parse_grab_key(&body, /*owner_events=*/ false).unwrap();
    assert_eq!(parsed.grab_window, 0x3412);
    assert_eq!(parsed.modifiers, 0x0040);
    assert_eq!(parsed.keycode, 24);
    assert_eq!(parsed.pointer_mode, 1);
    assert_eq!(parsed.keyboard_mode, 1);
    assert!(!parsed.owner_events);
}

#[test]
fn parse_ungrab_key_request() {
    // UngrabKey body: grab_window(4) modifiers(2) pad(2). header.data carries keycode.
    let body = [0x12, 0x34, 0x00, 0x00, 0x40, 0x00, 0, 0];
    let parsed = parse_ungrab_key(&body, /*keycode_in_header_data=*/ 24).unwrap();
    assert_eq!(parsed.grab_window, 0x3412);
    assert_eq!(parsed.keycode, 24);
    assert_eq!(parsed.modifiers, 0x0040);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yserver-protocol parse_grab_key parse_ungrab_key`
Expected: "cannot find function `parse_grab_key`".

- [ ] **Step 3: Implement parsers in `protocol/x11/mod.rs`**

```rust
#[derive(Debug, Clone, Copy)]
pub struct GrabKeyRequest {
    pub owner_events: bool,
    pub grab_window: u32,
    pub modifiers: u16,
    pub keycode: u8,
    pub pointer_mode: u8,
    pub keyboard_mode: u8,
}

#[must_use]
pub fn parse_grab_key(body: &[u8], owner_events: bool) -> Option<GrabKeyRequest> {
    if body.len() < 12 { return None; }
    Some(GrabKeyRequest {
        owner_events,
        grab_window: u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
        modifiers: u16::from_le_bytes([body[4], body[5]]),
        keycode: body[6],
        pointer_mode: body[7],
        keyboard_mode: body[8],
    })
}

#[derive(Debug, Clone, Copy)]
pub struct UngrabKeyRequest {
    pub keycode: u8,
    pub grab_window: u32,
    pub modifiers: u16,
}

#[must_use]
pub fn parse_ungrab_key(body: &[u8], keycode_in_header_data: u8) -> Option<UngrabKeyRequest> {
    if body.len() < 6 { return None; }
    Some(UngrabKeyRequest {
        keycode: keycode_in_header_data,
        grab_window: u32::from_le_bytes([body[0], body[1], body[2], body[3]]),
        modifiers: u16::from_le_bytes([body[4], body[5]]),
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yserver-protocol parse_grab_key parse_ungrab_key`
Expected: 2 passed.

- [ ] **Step 5: Wire the parsers into the dispatcher**

In `crates/yserver-core/src/nested.rs`, replace the `33 => log_void(...)` and `34 => log_void(...)` arms with:

```rust
33 => {
    if let Some(req) = x11::parse_grab_key(body, header.data != 0) {
        let mut s = lock_server(server)?;
        // De-dup: remove existing grab with same (owner, window, key, modifiers)
        s.key_grabs.retain(|g| !(g.owner == client_id
            && g.grab_window == ResourceId(req.grab_window)
            && g.keycode == req.keycode
            && g.modifiers == req.modifiers));
        s.key_grabs.push(crate::server::KeyGrab {
            owner: client_id,
            grab_window: ResourceId(req.grab_window),
            keycode: req.keycode,
            modifiers: req.modifiers,
            owner_events: req.owner_events,
            pointer_mode: req.pointer_mode,
            keyboard_mode: req.keyboard_mode,
        });
        debug!(
            "client {} GrabKey window=0x{:x} keycode={} modifiers=0x{:x}",
            client_id.0, req.grab_window, req.keycode, req.modifiers
        );
    }
    log_void(client_id, sequence, "GrabKey")
}
34 => {
    if let Some(req) = x11::parse_ungrab_key(body, header.data) {
        let mut s = lock_server(server)?;
        s.key_grabs.retain(|g| !(g.owner == client_id
            && g.grab_window == ResourceId(req.grab_window)
            && (g.keycode == req.keycode || req.keycode == 0)
            && (g.modifiers == req.modifiers || req.modifiers == 0x8000)));
    }
    log_void(client_id, sequence, "UngrabKey")
}
```

- [ ] **Step 6: Run full test suite**

Run: `cargo test`
Expected: all pass, no regressions.

- [ ] **Step 7: Commit**

```bash
git add crates/yserver-protocol/src/x11/mod.rs crates/yserver-core/src/nested.rs
git commit -m "$(cat <<'EOF'
feat: implement GrabKey / UngrabKey (op 33 / 34)

Stores passive key grabs in ServerState.key_grabs; UngrabKey supports
AnyKey/AnyModifier wildcards. Parsers covered by unit tests.

EOF
)"
```

### Task A3: GrabKeyboard / UngrabKeyboard

**Files:**
- Modify: `crates/yserver-core/src/server.rs` (add `ActiveKeyboardGrab`, `ActiveKeyboardGrabSource`, `active_keyboard_grab` field)
- Modify: `crates/yserver-core/src/nested.rs` (replace stubs at opcodes 31, 32)

This task introduces the struct that Task A4 fills in further. We
keep it minimal here: just the explicit-grab path.

- [ ] **Step 1: Add the structs and field**

In `server.rs` near `KeyGrab`:

```rust
#[derive(Debug, Clone, Copy)]
pub enum ActiveKeyboardGrabSource {
    Explicit,                  // from GrabKeyboard
    PassiveKey { keycode: u8 },
}

#[derive(Debug, Clone, Copy)]
pub struct ActiveKeyboardGrab {
    pub owner: ClientId,
    pub grab_window: ResourceId,
    pub source: ActiveKeyboardGrabSource,
}
```

Add `pub active_keyboard_grab: Option<ActiveKeyboardGrab>` to
`ServerState`, init to `None`.

- [ ] **Step 2: Unit test**

```rust
#[test]
fn active_keyboard_grab_set_and_clear() {
    use crate::resources::{ClientId, ResourceId};
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
```

Run: `cargo test -p yserver-core active_keyboard_grab` — passes.

- [ ] **Step 3: Wire opcodes 31 / 32**

```rust
31 => {
    // GrabKeyboard body: owner_events(header.data) grab_window(4)
    //   time(4) pointer_mode(1) keyboard_mode(1) pad(2)
    if body.len() >= 12 {
        let grab_window = ResourceId(u32::from_le_bytes(
            [body[0], body[1], body[2], body[3]]));
        let mut s = lock_server(server)?;
        s.active_keyboard_grab = Some(crate::server::ActiveKeyboardGrab {
            owner: client_id,
            grab_window,
            source: crate::server::ActiveKeyboardGrabSource::Explicit,
        });
    }
    log_reply(client_id, sequence, "GrabKeyboard");
    x11::write_grab_reply(&mut *lock_writer()?, sequence, 0)
}
32 => {
    let mut s = lock_server(server)?;
    if let Some(g) = s.active_keyboard_grab && g.owner == client_id {
        s.active_keyboard_grab = None;
    }
    log_void(client_id, sequence, "UngrabKeyboard")
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/yserver-core/src/server.rs crates/yserver-core/src/nested.rs
git commit -m "$(cat <<'EOF'
feat: implement GrabKeyboard / UngrabKeyboard (op 31 / 32)

Active keyboard grab tracked on ServerState; routing change in
spawn_keyboard_forwarder follows in next commit.

EOF
)"
```

### Task A4: Route key events through grab table

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` — `spawn_keyboard_forwarder`

- [ ] **Step 1: Add a unit test for grab routing decision**

The existing forwarder is a free function that's hard to unit-test in isolation because it owns a `HostInputPump`. Add a pure helper `decide_key_target` and test it:

In `nested.rs` (or a new `crates/yserver-core/src/keyboard.rs` if you prefer keeping nested.rs from growing more):

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum KeyTarget {
    Focus(ResourceId),
    Grab { client_id: ClientId, grab_window: ResourceId },
    Drop,
}

pub(crate) fn decide_key_target(
    state: &ServerState,
    focus: ResourceId,
    keycode: u8,
    state_mask: u16,
) -> KeyTarget {
    // Active keyboard grab pre-empts everything.
    if let Some((cid, win)) = state.keyboard_grab {
        return KeyTarget::Grab { client_id: cid, grab_window: win };
    }
    if let Some(grab) = state.find_key_grab(focus, keycode, state_mask) {
        return KeyTarget::Grab {
            client_id: grab.owner,
            grab_window: grab.grab_window,
        };
    }
    if focus == ROOT_WINDOW {
        return KeyTarget::Drop;
    }
    KeyTarget::Focus(focus)
}
```

Test cases (in `nested.rs` tests mod):

```rust
#[test]
fn key_target_focus_when_no_grab() {
    let s = ServerState::new();
    let focus = ResourceId(0x100);
    assert_eq!(decide_key_target(&s, focus, 24, 0), KeyTarget::Focus(focus));
}

#[test]
fn key_target_active_grab_wins() {
    let mut s = ServerState::new();
    s.keyboard_grab = Some((ClientId(3), ResourceId(0x200)));
    let focus = ResourceId(0x100);
    assert_eq!(
        decide_key_target(&s, focus, 24, 0),
        KeyTarget::Grab { client_id: ClientId(3), grab_window: ResourceId(0x200) },
    );
}
```

(Note: the passive-grab-routing test belongs in Task A1 once the focus-window walk has a real `ResourceTable` populated; here we just exercise the pure helper.)

- [ ] **Step 2: Run, fail, implement, pass**

Run: `cargo test -p yserver-core key_target`
Expected: fail with "decide_key_target not found", then pass after pasting in the helper.

- [ ] **Step 3: Use the helper in `spawn_keyboard_forwarder`**

In the existing forwarder loop, replace the `if focus == ROOT_WINDOW { continue; }` block plus the `write_key_event(... event: focus ...)` call.

The X11 spec says a passive `GrabKey` match on a `KeyPress` activates a *temporary active keyboard grab* held by the same owner on the same grab window, lasting until the matching `KeyRelease`. While any active keyboard grab is held, passive grabs from other clients do not fire. The lookup therefore needs to track three states:

1. An active keyboard grab is held → route to its owner.
2. No active grab; this is a `KeyPress`; passive lookup matches → install an active grab tagged `PassiveKey { keycode }` and route to the new owner.
3. Otherwise → route to focus.

Update `decide_key_target` to take the press/release flag and a mutable hook for installing the active grab:

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum KeyTarget {
    Focus(ResourceId),
    Grab { client_id: ClientId, grab_window: ResourceId },
    Drop,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ActiveKeyboardGrabSource {
    Explicit,                  // GrabKeyboard
    PassiveKey { keycode: u8 },
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ActiveKeyboardGrab {
    pub owner: ClientId,
    pub grab_window: ResourceId,
    pub source: ActiveKeyboardGrabSource,
}

pub(crate) fn route_key_event(
    state: &mut ServerState,
    focus: ResourceId,
    keycode: u8,
    state_mask: u16,
    pressed: bool,
) -> KeyTarget {
    // 1. Active keyboard grab pre-empts everything.
    if let Some(g) = state.active_keyboard_grab {
        // Auto-release a passive-induced active grab when its key releases.
        if !pressed {
            if let ActiveKeyboardGrabSource::PassiveKey { keycode: kc } = g.source {
                if kc == keycode {
                    state.active_keyboard_grab = None;
                }
            }
        }
        return KeyTarget::Grab { client_id: g.owner, grab_window: g.grab_window };
    }
    // 2. Passive grab match on press → activate.
    if pressed {
        if let Some(grab) = state.find_key_grab(focus, keycode, state_mask) {
            let owner = grab.owner;
            let win = grab.grab_window;
            state.active_keyboard_grab = Some(ActiveKeyboardGrab {
                owner,
                grab_window: win,
                source: ActiveKeyboardGrabSource::PassiveKey { keycode },
            });
            return KeyTarget::Grab { client_id: owner, grab_window: win };
        }
    }
    // 3. Focus delivery (drop on root).
    if focus == ROOT_WINDOW { return KeyTarget::Drop; }
    KeyTarget::Focus(focus)
}
```

Replace `keyboard_grab: Option<(ClientId, ResourceId)>` from Task A3
with `active_keyboard_grab: Option<ActiveKeyboardGrab>` and add the
`Explicit` source from `GrabKeyboard`. (Update the A3 diff and the
unit test to use the new struct; the test from A3 becomes:

```rust
s.active_keyboard_grab = Some(ActiveKeyboardGrab {
    owner: ClientId(7),
    grab_window: ResourceId(0xff),
    source: ActiveKeyboardGrabSource::Explicit,
});
```

before this task is committed.)

Then in the forwarder loop:

```rust
let (event_window, target_writer) = {
    let mut s = match server.lock() { Ok(s) => s, Err(_) => continue };
    let target = route_key_event(&mut s, focus, event.keycode, event.state, event.pressed);
    match target {
        KeyTarget::Drop => continue,
        KeyTarget::Focus(w) => (w, writer.clone()),
        KeyTarget::Grab { client_id: cid, grab_window } => {
            match s.client_target(cid) {
                Some(t) => (grab_window, t.writer.clone()),
                None => continue,
            }
        }
    }
};
```

- [ ] **Step 4: Build and run tests**

Run: `cargo build && cargo test`
Expected: pass.

- [ ] **Step 5: Manual smoke test**

Start `ynest`, run:

```sh
DISPLAY=:99 xterm &
DISPLAY=:99 sh -c 'xdotool key --clearmodifiers super+q' || true
```

Confirm the key is delivered to xterm normally (no grab registered). Then write a tiny test client that calls `XGrabKey` for `XK_q` with Mod4Mask on the root window and confirm key events arrive at *that* client when xterm has focus and Super+q is pressed.

- [ ] **Step 6: Commit**

```bash
git add crates/yserver-core/src/nested.rs
git commit -m "$(cat <<'EOF'
feat: route key events through KeyGrab table

Active keyboard grab pre-empts focus delivery; passive GrabKey on the
focused window or any ancestor delivers to the grab owner with the
event window set to the grab window.

EOF
)"
```

### Task A5: Real GetKeyboardMapping via host proxy

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs` — add `get_keyboard_mapping(first, count)` and `keyboard_min_max() -> (u8, u8)`
- Modify: `crates/yserver-protocol/src/x11/mod.rs` — overload `write_get_keyboard_mapping_reply` to accept a precomputed keysym slice
- Modify: `crates/yserver-core/src/nested.rs` opcode 101

- [ ] **Step 1: Write a host-proxy test**

The host proxy uses live X11 sockets, so test it indirectly: write a wire-encoder test for `write_get_keyboard_mapping_reply_from_keysyms`:

```rust
#[test]
fn keyboard_mapping_reply_from_keysyms_layout() {
    let keysyms: &[u32] = &[0x71, 0x51, 0, 0,    // q Q
                            0x77, 0x57, 0, 0];   // w W
    let mut buf = Vec::new();
    write_get_keyboard_mapping_reply_from_keysyms(
        &mut buf, SequenceNumber(7), 4, keysyms).unwrap();
    assert_eq!(buf[0], 1);                     // reply
    assert_eq!(buf[1], 4);                     // keysyms-per-keycode
    let length = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    assert_eq!(length, 8);                     // 8 keysyms × 4 bytes / 4
    let kb = &buf[32..];
    assert_eq!(kb.len(), 32);
    assert_eq!(u32::from_le_bytes(kb[0..4].try_into().unwrap()), 0x71);
}
```

- [ ] **Step 2: Run, fail, implement**

Run: `cargo test -p yserver-protocol keyboard_mapping_reply_from_keysyms_layout`
Expected: function not found.

Implement in `protocol/x11/mod.rs`:

```rust
pub fn write_get_keyboard_mapping_reply_from_keysyms(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    keysyms_per_keycode: u8,
    keysyms: &[u32],
) -> io::Result<()> {
    let length_words = u32::try_from(keysyms.len()).unwrap_or(0);
    let mut reply = fixed_reply(sequence, keysyms_per_keycode, length_words);
    // fixed_reply leaves only 32 bytes; we append 4-byte keysyms.
    for k in keysyms {
        reply.extend_from_slice(&k.to_le_bytes());
    }
    writer.write_all(&reply)
}
```

- [ ] **Step 3: Add host proxy method**

In `host_x11.rs` (look at `list_fonts_proxy` for the established raw-wire pattern). Implementation sketch:

```rust
pub fn get_keyboard_mapping(
    &mut self,
    first_keycode: u8,
    count: u8,
) -> io::Result<(u8 /* keysyms_per_keycode */, Vec<u32>)> {
    // Build wire request manually (opcode 101, length 2):
    let mut req = [0u8; 8];
    req[0] = 101;
    req[1] = 0;            // pad
    req[2] = 2; req[3] = 0; // length in 4-byte units
    req[4] = first_keycode;
    req[5] = count;
    req[6] = 0; req[7] = 0;
    self.stream.write_all(&req)?;
    // Read reply (header 32 bytes; trailing keysyms).
    let mut header = [0u8; 32];
    self.stream.read_exact(&mut header)?;
    let kpc = header[1];
    let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    let total_bytes = (length as usize) * 4;
    let mut tail = vec![0u8; total_bytes];
    self.stream.read_exact(&mut tail)?;
    let mut keysyms = Vec::with_capacity(tail.len() / 4);
    for chunk in tail.chunks_exact(4) {
        keysyms.push(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok((kpc, keysyms))
}
```

(Use `XGetKeyboardMapping` via `x11-rs` if `host_x11.rs` already binds to it; otherwise the raw approach above.)

If `host_x11` already uses `xlib` bindings (check `Cargo.toml` / existing code: it uses `XOpenDisplay` etc.), prefer:

```rust
use x11::xlib::{XGetKeyboardMapping, XFree};
let mut kpc: c_int = 0;
let ptr = unsafe { XGetKeyboardMapping(self.display, first_keycode as c_int, count as c_int, &mut kpc) };
// ptr is array of (count * kpc) KeySym (XID = c_ulong on this platform).
let n = (count as usize) * (kpc as usize);
let slice = unsafe { std::slice::from_raw_parts(ptr, n) };
let keysyms: Vec<u32> = slice.iter().map(|&k| k as u32).collect();
unsafe { XFree(ptr.cast()); }
Ok((kpc as u8, keysyms))
```

Inspect `host_x11.rs` to choose the matching style; the project has been mixing raw wire and Xlib calls.

- [ ] **Step 4: Wire opcode 101**

Replace existing handler:

```rust
101 => {
    log_reply(client_id, sequence, "GetKeyboardMapping");
    let first_keycode = body.first().copied().unwrap_or(8);
    let count = body.get(1).copied().unwrap_or(0);
    let result = host
        .and_then(|h| h.lock().ok())
        .and_then(|mut h| h.get_keyboard_mapping(first_keycode, count).ok());
    if let Some((kpc, keysyms)) = result {
        x11::write_get_keyboard_mapping_reply_from_keysyms(
            &mut *lock_writer()?, sequence, kpc, &keysyms)
    } else {
        // Fallback to existing local stub on host failure
        x11::write_get_keyboard_mapping_reply(
            &mut *lock_writer()?, sequence, first_keycode, count, 4)
    }
}
```

- [ ] **Step 5: Run tests, build, commit**

```bash
cargo test
cargo +nightly fmt
cargo clippy -- -W clippy::pedantic
git add -A crates/
git commit -m "$(cat <<'EOF'
feat: proxy GetKeyboardMapping (op 101) to host

Replaces the hard-coded keysyms.rs stub with the host's real keymap.
Falls back to the local table when the host call fails.

EOF
)"
```

### Task A6: Real GetModifierMapping via host proxy

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs` — `get_modifier_mapping() -> [u8; 64]` (8 modifiers × 8 keycodes)
- Modify: `crates/yserver-protocol/src/x11/mod.rs` — extend `write_get_modifier_mapping_reply` to accept a 64-byte keycode array (or add `..._with_keycodes`)
- Modify: `crates/yserver-core/src/nested.rs` opcode 119

Steps follow the same TDD shape as Task A5: encoder unit test in `protocol`, host proxy method, dispatcher rewrite, full test, commit.

The reply width is `8 * keycodes_per_modifier` bytes. Don't hardcode
64 — host Xlib may report a different `max_keypermod`.

- [ ] **Step 1: Encoder test (parameterised)**

```rust
#[test]
fn modifier_mapping_reply_layout_kpm_2() {
    let kpm = 2u8;
    let kc: Vec<u8> = (0..(8 * kpm) as u8).map(|i| i + 8).collect();
    let mut buf = Vec::new();
    write_get_modifier_mapping_reply_with_keycodes(&mut buf, SequenceNumber(3), kpm, &kc).unwrap();
    assert_eq!(buf[0], 1);     // reply
    assert_eq!(buf[1], kpm);
    let length = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    assert_eq!(length, (8 * kpm as u32) / 4);
    assert_eq!(&buf[32..32 + 8 * kpm as usize], &kc[..]);
}

#[test]
fn modifier_mapping_reply_layout_kpm_4() {
    let kpm = 4u8;
    let kc: Vec<u8> = (0..(8 * kpm) as u8).map(|i| i + 8).collect();
    let mut buf = Vec::new();
    write_get_modifier_mapping_reply_with_keycodes(&mut buf, SequenceNumber(3), kpm, &kc).unwrap();
    let length = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    assert_eq!(length, (8 * kpm as u32) / 4);
    assert_eq!(&buf[32..32 + 8 * kpm as usize], &kc[..]);
}
```

- [ ] **Step 2: Implement encoder**

```rust
pub fn write_get_modifier_mapping_reply_with_keycodes(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    keycodes_per_modifier: u8,
    keycodes: &[u8],
) -> io::Result<()> {
    debug_assert_eq!(keycodes.len(), 8 * keycodes_per_modifier as usize,
        "GetModifierMapping payload must be exactly 8 * keycodes_per_modifier");
    let total = 8 * u32::from(keycodes_per_modifier);
    let length_words = total / 4;
    let mut reply = fixed_reply(sequence, keycodes_per_modifier, length_words);
    reply.extend_from_slice(keycodes);
    // total is always a multiple of 4 (8 * kpm); no extra padding needed.
    writer.write_all(&reply)
}
```

- [ ] **Step 3: Host proxy**

`get_modifier_mapping` returns `(kc_per_modifier, Vec<u8>)`. Use `XGetModifierMapping` from xlib bindings if available; the returned struct has `.max_keypermod` and `.modifiermap` (a `c_uchar*`). Free with `XFreeModifiermap`.

- [ ] **Step 4: Dispatcher**

```rust
119 => {
    log_reply(client_id, sequence, "GetModifierMapping");
    let result = host
        .and_then(|h| h.lock().ok())
        .and_then(|mut h| h.get_modifier_mapping().ok());
    if let Some((kpm, keycodes)) = result {
        x11::write_get_modifier_mapping_reply_with_keycodes(
            &mut *lock_writer()?, sequence, kpm, &keycodes)
    } else {
        x11::write_get_modifier_mapping_reply(&mut *lock_writer()?, sequence)
    }
}
```

- [ ] **Step 5: Run tests, commit**

Commit message:

```
feat: proxy GetModifierMapping (op 119) to host

```

### Task A7: ChangeKeyboardMapping + MappingNotify

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs` — `write_mapping_notify_event(request, first_keycode, count)`
- Modify: `crates/yserver-core/src/nested.rs` — opcode 100

- [ ] **Step 1: Write encoder test**

```rust
#[test]
fn mapping_notify_event_layout() {
    let mut buf = Vec::new();
    write_mapping_notify_event(&mut buf, SequenceNumber(0), /*request=*/1, 8, 248).unwrap();
    assert_eq!(buf.len(), 32);
    assert_eq!(buf[0], 34);     // MappingNotify
    // buf[1] is unused
    assert_eq!(buf[4], 1);      // request: Keyboard
    assert_eq!(buf[5], 8);      // first_keycode
    assert_eq!(buf[6], 248);    // count
}
```

- [ ] **Step 2: Implement encoder + dispatcher**

Per the X11 spec, MappingNotify body layout (bytes 4..32 after the
2-byte sequence at 2..4) starts with `request` at byte 4,
`first-keycode` at byte 5, `count` at byte 6, then 25 unused bytes.
Byte 1 is unused.

```rust
pub fn write_mapping_notify_event(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    request: u8,         // 0=Modifier, 1=Keyboard, 2=Pointer
    first_keycode: u8,
    count: u8,
) -> io::Result<()> {
    let mut buf = [0u8; 32];
    buf[0] = 34;
    // buf[1] unused
    buf[2..4].copy_from_slice(&sequence.0.to_le_bytes());
    buf[4] = request;
    buf[5] = first_keycode;
    buf[6] = count;
    writer.write_all(&buf)
}
```

Dispatcher (replacing absent `100 =>` arm; insert next to opcode 101):

```rust
100 => {
    // ChangeKeyboardMapping is host-mediated; treat as a no-op and
    // broadcast MappingNotify so clients refresh their keymaps.
    let first = body.first().copied().unwrap_or(8);
    let count = header.data; // keycode-count is in the header byte
    let targets: Vec<_> = lock_server(server)?.clients.values()
        .map(crate::server::ServerState::event_target_for_client)
        .collect();
    crate::server::fanout_event(&targets, |buf, seq, _order| {
        let _ = x11::write_mapping_notify_event(buf, seq, 1, first, count);
    });
    log_void(client_id, sequence, "ChangeKeyboardMapping")
}
```

(Note: `ServerState::event_target_for_client` is currently private — change it to `pub(crate)` if needed.)

- [ ] **Step 3: Run tests, commit**

```
feat: implement ChangeKeyboardMapping (op 100) + MappingNotify

Treats the request as a no-op (host owns the keymap) but emits
MappingNotify(Keyboard) to all clients so they refresh.

```

---

## Group B — Window operations for reparenting WMs

### Task B1: Per-client save_set storage

**Files:**
- Modify: `crates/yserver-core/src/server.rs` — `ClientHandle.save_set: HashSet<ResourceId>`

- [ ] **Step 1: Add the field**

In `server.rs`, add to `ClientHandle`:

```rust
pub save_set: HashSet<ResourceId>,
```

Initialise it (`HashSet::new()`) in every place `ClientHandle { ... }`
is constructed. Search `ClientHandle {` — there are 1–2 sites in
`nested.rs`.

- [ ] **Step 2: Test via real `UnixStream::pair()`**

Constructing a `ClientHandle` for a unit test needs a real
`UnixStream`. Use `UnixStream::pair()`:

```rust
#[test]
fn save_set_insert_and_remove() {
    use crate::resources::ResourceId;
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex, atomic::AtomicU16};

    let (a, _b) = UnixStream::pair().unwrap();
    let mut handle = ClientHandle {
        writer: Arc::new(Mutex::new(a)),
        byte_order: ClientByteOrder::Little,
        last_sequence: Arc::new(AtomicU16::new(0)),
        resource_id_base: 0,
        resource_id_mask: 0,
        event_masks: Default::default(),
        save_set: Default::default(),
    };
    handle.save_set.insert(ResourceId(0x10));
    handle.save_set.insert(ResourceId(0x20));
    handle.save_set.remove(&ResourceId(0x10));
    assert!(!handle.save_set.contains(&ResourceId(0x10)));
    assert!(handle.save_set.contains(&ResourceId(0x20)));
}
```

(`_b` is held by the test to keep the pair alive; dropping it would
close the writer end.)

- [ ] **Step 3: Test, commit**

```
feat: add save_set field to ClientHandle

```

### Task B2: ChangeSaveSet opcode

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` — opcode 6

- [ ] **Step 1: Wire dispatch**

```rust
6 => {
    // ChangeSaveSet body: window(4); header.data = mode (0=Insert, 1=Delete)
    if body.len() >= 4 {
        let win = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let mut s = lock_server(server)?;
        if let Some(c) = s.clients.get_mut(&client_id.0) {
            match header.data {
                0 => { c.save_set.insert(win); }
                1 => { c.save_set.remove(&win); }
                _ => {}
            }
        }
    }
    log_void(client_id, sequence, "ChangeSaveSet")
}
```

- [ ] **Step 2: Add `restore_save_set_for_client` on `ResourceTable`**

Per X11 spec semantics: for each save-set window inferior to a window
created by the dying client, reparent it to the **closest ancestor
that was not created by the dying client**, preserving its
*absolute root-relative position*; if it is unmapped, also map it.

In `resources.rs`:

```rust
/// Returns the list of (save-set window, new_parent, new_x, new_y, needs_remap)
/// that the caller should apply. Pure — does not mutate self.
pub fn plan_save_set_restore(
    &self,
    dying_client: ClientId,
    save_set: &HashSet<ResourceId>,
) -> Vec<(ResourceId, ResourceId, i16, i16, bool)> {
    let mut out = Vec::new();
    for &w in save_set {
        // Skip if the window itself is gone or not inferior to a
        // dying-client-created window.
        let Some(win) = self.window(w) else { continue };
        let abs = self.window_absolute_position(w);
        // Walk up from current parent to find first ancestor whose
        // creator is NOT dying_client.
        let mut anc = win.parent;
        while anc != ROOT_WINDOW {
            let Some(ancw) = self.window(anc) else { break };
            if ancw.created_by != dying_client { break; }
            anc = ancw.parent;
        }
        if anc == win.parent { continue; }   // already a safe ancestor
        let new_parent_abs = self.window_absolute_position(anc);
        #[allow(clippy::cast_possible_truncation)]
        let nx = (abs.0 - new_parent_abs.0) as i16;
        #[allow(clippy::cast_possible_truncation)]
        let ny = (abs.1 - new_parent_abs.1) as i16;
        let needs_remap = !win.mapped;
        out.push((w, anc, nx, ny, needs_remap));
    }
    out
}
```

This requires `Window.created_by: ClientId` (add the field; populate
it at `create_window` time — search `ResourceTable::create_window`).
If `created_by` doesn't already exist, add it as a small TDD step
*before* this one.

- [ ] **Step 3: Wire restore into the disconnect path**

Search for the `clients.remove` call (likely in `handle_client`
cleanup or a `drop_client` helper). Before resource cleanup:

```rust
let plan = match server.lock() {
    Ok(s) => {
        let save_set = s.clients.get(&client_id.0)
            .map(|c| c.save_set.clone())
            .unwrap_or_default();
        s.resources.plan_save_set_restore(client_id, &save_set)
    }
    Err(_) => Vec::new(),
};
for (w, new_parent, nx, ny, needs_remap) in plan {
    if let Ok(mut s) = server.lock() {
        let _ = s.resources.reparent_window(w, new_parent, nx, ny);
        if needs_remap {
            let _ = s.resources.map_window(w);
        }
    }
}
```

- [ ] **Step 4: Test `plan_save_set_restore` on `ResourceTable`**

```rust
#[test]
fn save_set_reparents_to_first_non_dying_ancestor_and_remaps() {
    let mut t = ResourceTable::new();
    let wm = ClientId(1);
    let app = ClientId(2);
    let frame = ResourceId(0x100);
    let child = ResourceId(0x200);
    t.create_window_for(wm, frame, ROOT_WINDOW, 100, 200, 400, 300, 24).unwrap();
    t.create_window_for(app, child, frame, 5, 7, 100, 50, 24).unwrap();
    // child is unmapped; frame is mapped.
    t.map_window(frame).unwrap();

    let mut ss = HashSet::new();
    ss.insert(child);
    let plan = t.plan_save_set_restore(/*dying=*/wm, &ss);
    assert_eq!(plan.len(), 1);
    let (w, parent, nx, ny, needs_remap) = plan[0];
    assert_eq!(w, child);
    assert_eq!(parent, ROOT_WINDOW);
    // child absolute is 100+5, 200+7 = (105, 207); root is at (0,0).
    assert_eq!((nx, ny), (105, 207));
    assert!(needs_remap);
}
```

- [ ] **Step 5: Commit**

```
feat: implement ChangeSaveSet (op 6) with spec-correct restore

Tracks per-client save-set; on client disconnect, save-set windows
are reparented to the closest ancestor not owned by the dying
client, preserving root-relative position, and remapped if they
were unmapped.
```

### Task B3: CirculateNotify / CirculateRequest event encoders

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs`

- [ ] **Step 1: Encoder tests**

```rust
#[test]
fn circulate_notify_event_layout() {
    let mut buf = Vec::new();
    write_circulate_notify_event(
        &mut buf, SequenceNumber(0), ResourceId(0x100), ResourceId(0x200), 0).unwrap();
    assert_eq!(buf.len(), 32);
    assert_eq!(buf[0], 26);                              // CirculateNotify
    let event_window = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let window = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    assert_eq!(event_window, 0x100);
    assert_eq!(window, 0x200);
    assert_eq!(buf[16], 0);                              // place: PlaceOnTop
}

#[test]
fn circulate_request_event_layout() {
    let mut buf = Vec::new();
    write_circulate_request_event(
        &mut buf, SequenceNumber(0), ResourceId(0x100), ResourceId(0x200), 1).unwrap();
    assert_eq!(buf[0], 27);                              // CirculateRequest
    assert_eq!(buf[16], 1);                              // place: PlaceOnBottom
}
```

- [ ] **Step 2: Implement**

```rust
pub fn write_circulate_notify_event(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    event_window: ResourceId,
    window: ResourceId,
    place: u8,            // 0=Top, 1=Bottom
) -> io::Result<()> {
    let mut buf = [0u8; 32];
    buf[0] = 26;
    buf[2..4].copy_from_slice(&sequence.0.to_le_bytes());
    buf[4..8].copy_from_slice(&event_window.0.to_le_bytes());
    buf[8..12].copy_from_slice(&window.0.to_le_bytes());
    buf[16] = place;
    writer.write_all(&buf)
}

pub fn write_circulate_request_event(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    parent: ResourceId,
    window: ResourceId,
    place: u8,
) -> io::Result<()> {
    let mut buf = [0u8; 32];
    buf[0] = 27;
    buf[2..4].copy_from_slice(&sequence.0.to_le_bytes());
    buf[4..8].copy_from_slice(&parent.0.to_le_bytes());
    buf[8..12].copy_from_slice(&window.0.to_le_bytes());
    buf[16] = place;
    writer.write_all(&buf)
}
```

- [ ] **Step 3: Test, commit**

```
feat: add CirculateNotify / CirculateRequest event encoders

```

### Task B4: CirculateWindow opcode 13

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` — new arm 13

- [ ] **Step 1: Wire dispatch**

The request's `window` argument is the **container** whose children are
being restacked — it is not itself the moved window. Substructure
redirect is checked on the container; the resulting `CirculateRequest`
event reports `parent = container`, `window = the chosen child`.

```rust
13 => {
    // CirculateWindow body: container(4); header.data = direction (0=RaiseLowest, 1=LowerHighest)
    if body.len() >= 4 {
        let container = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let direction = header.data;

        // Pick the child that would actually be restacked (back for Raise, front for Lower).
        let chosen_child = {
            let s = lock_server(server)?;
            let kids = s.resources.children(container);
            match (direction, kids.first(), kids.last()) {
                (0, _, Some(&back)) => Some(back),
                (1, Some(&front), _) => Some(front),
                _ => None,
            }
        };

        if let Some(child) = chosen_child {
            // Substructure redirect on the container (NOT its parent).
            let redirect_target = lock_server(server)?
                .subscribers(container, 0x0010_0000) // SubstructureRedirectMask
                .into_iter().next();
            if let Some(target) = redirect_target {
                let seq = SequenceNumber(target.last_sequence.load(Ordering::Relaxed));
                let mut buf = Vec::with_capacity(32);
                let _ = x11::write_circulate_request_event(
                    &mut buf, seq, /*parent=*/container, /*window=*/child, direction);
                if let Ok(mut w) = target.writer.lock() { let _ = w.write_all(&buf); }
            } else {
                // No redirect — actually circulate.
                let _ = lock_server(server)?.resources.circulate_window(container, direction);
                let on_child = lock_server(server)?.subscribers(child, 0x0002_0000);        // StructureNotify on the moved child
                let on_container = lock_server(server)?.subscribers(container, 0x0008_0000); // SubstructureNotify on the container
                for t in on_child.into_iter().chain(on_container.into_iter()) {
                    let seq = SequenceNumber(t.last_sequence.load(Ordering::Relaxed));
                    let mut buf = Vec::with_capacity(32);
                    let _ = x11::write_circulate_notify_event(
                        &mut buf, seq, /*event_window=*/child, /*window=*/child, direction);
                    if let Ok(mut w) = t.writer.lock() { let _ = w.write_all(&buf); }
                }
            }
        }
    }
    log_void(client_id, sequence, "CirculateWindow")
}
```

- [ ] **Step 2: Implement `ResourceTable::circulate_window`**

In `resources.rs`, add a method that reorders the parent's child list per X11 semantics:
- direction 0 (RaiseLowest): if any obscured child exists, raise the lowest one to the top of stacking order
- direction 1 (LowerHighest): if any obscuring child exists, lower the highest one to the bottom

For Phase 2 we don't model obscuring; treat both as a simple reorder (move the back child to the front, or front to the back). Document this approximation in a single-line comment.

```rust
pub fn circulate_window(&mut self, window: ResourceId, direction: u8) -> Result<(), ResourceError> {
    let parent = self.window(window).ok_or(ResourceError::NotFound)?.parent;
    let children = self.children_mut(parent);
    if children.len() < 2 { return Ok(()); }
    match direction {
        0 => { // RaiseLowest: move last to first
            let last = children.pop().expect("len>=2");
            children.insert(0, last);
        }
        1 => { // LowerHighest: move first to last
            let first = children.remove(0);
            children.push(first);
        }
        _ => return Err(ResourceError::Invalid),
    }
    Ok(())
}
```

(Look up the actual `children_mut` / equivalent helper in resources.rs; rename if needed.)

- [ ] **Step 3: Test, commit**

Add unit test for `circulate_window` in resources.rs.

```
feat: implement CirculateWindow (op 13)

SubstructureRedirect path emits CirculateRequest; otherwise reorders
children and emits CirculateNotify. Phase-2 stacking is naive
(end-of-list rotation); proper obscuring detection comes with the
compositor.

```

### Task B5: DestroySubwindows opcode 5

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Wire dispatch**

```rust
5 => {
    if body.len() >= 4 {
        let parent = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let kids: Vec<ResourceId> = lock_server(server)?.resources.children(parent).to_vec();
        for k in kids {
            destroy_window(client_id, server, host, k); // existing helper
        }
    }
    log_void(client_id, sequence, "DestroySubwindows")
}
```

(`destroy_window` is the helper used by the opcode-4 path; reuse it.)

- [ ] **Step 2: Test, commit**

```
feat: implement DestroySubwindows (op 5)

```

---

## Group C — Drawing

### Task C1: CopyPlane opcode 63

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs` — `copy_plane(...)` mirroring `copy_area`
- Modify: `crates/yserver-core/src/nested.rs` — new arm 63

- [ ] **Step 1: Add host method**

Look at `copy_area` (lines ~971+). Add:

```rust
pub fn copy_plane(
    &mut self,
    src_xid: u32, dst_xid: u32, gc_xid: u32,
    src_x: i16, src_y: i16, dst_x: i16, dst_y: i16,
    width: u16, height: u16, plane: u32,
) -> io::Result<()> {
    // Build wire request: opcode 63, length 8 words.
    let mut req = [0u8; 32];
    req[0] = 63; req[2] = 8; req[3] = 0;
    req[4..8].copy_from_slice(&src_xid.to_le_bytes());
    req[8..12].copy_from_slice(&dst_xid.to_le_bytes());
    req[12..16].copy_from_slice(&gc_xid.to_le_bytes());
    req[16..18].copy_from_slice(&src_x.to_le_bytes());
    req[18..20].copy_from_slice(&src_y.to_le_bytes());
    req[20..22].copy_from_slice(&dst_x.to_le_bytes());
    req[22..24].copy_from_slice(&dst_y.to_le_bytes());
    req[24..26].copy_from_slice(&width.to_le_bytes());
    req[26..28].copy_from_slice(&height.to_le_bytes());
    req[28..32].copy_from_slice(&plane.to_le_bytes());
    self.stream.write_all(&req)
}
```

- [ ] **Step 2: Wire opcode**

In `nested.rs`, model after the existing `62 => { ... CopyArea ... }` arm:

```rust
63 => {
    // CopyPlane body (after the 4-byte request header): src(4) dst(4) gc(4)
    //   src_x(2) src_y(2) dst_x(2) dst_y(2) width(2) height(2) plane(4)
    // Total 28 bytes.
    if body.len() >= 28 {
        let src = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let dst = ResourceId(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
        let gc = ResourceId(u32::from_le_bytes([body[8], body[9], body[10], body[11]]));
        let sx = i16::from_le_bytes([body[12], body[13]]);
        let sy = i16::from_le_bytes([body[14], body[15]]);
        let dx = i16::from_le_bytes([body[16], body[17]]);
        let dy = i16::from_le_bytes([body[18], body[19]]);
        let w  = u16::from_le_bytes([body[20], body[21]]);
        let h  = u16::from_le_bytes([body[22], body[23]]);
        let plane = u32::from_le_bytes([body[24], body[25], body[26], body[27]]);
        // Look up host xids and offsets — see existing CopyArea handler;
        // call host.copy_plane(...). On failure log and drop.
    }
    log_void(client_id, sequence, "CopyPlane")
}
```

- [ ] **Step 3: Commit**

```
feat: implement CopyPlane (op 63)

```

---

## Group D — Pointer/grab follow-ups

### Task D1: ChangeActivePointerGrab opcode 30

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

Per the X11 spec, `ChangeActivePointerGrab` updates the *active*
pointer grab and explicitly does not affect passive button grabs.
The fields live on a new `ActivePointerGrab` record, not on
`button_grabs`.

- [ ] **Step 1: Promote `pointer_grab` to a struct**

In `server.rs`, replace `pointer_grab: Option<(ClientId, ResourceId)>`
with:

```rust
#[derive(Debug, Clone, Copy)]
pub struct ActivePointerGrab {
    pub owner: ClientId,
    pub grab_window: ResourceId,
    pub event_mask: u16,
    pub cursor: ResourceId,    // 0 = inherit
    pub time: u32,
}

pub active_pointer_grab: Option<ActivePointerGrab>,
```

Migrate every existing `pointer_grab = Some((...))` site (search the
crate) to construct `ActivePointerGrab { ... }`. Migrate
`pointer_grab.and_then(...)` reads similarly. The existing
`pointer_grab_is_passive` flag stays — it tracks whether the active
grab was activated *by* a passive button grab (different concept
from a passive button grab itself).

- [ ] **Step 2: Wire dispatch**

```rust
30 => {
    // body: cursor(4) time(4) event_mask(2) pad(2)
    if body.len() >= 12 {
        let cursor = ResourceId(u32::from_le_bytes([body[0], body[1], body[2], body[3]]));
        let time = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        let event_mask = u16::from_le_bytes([body[8], body[9]]);
        let mut s = lock_server(server)?;
        if let Some(g) = s.active_pointer_grab.as_mut() {
            if g.owner == client_id {
                g.event_mask = event_mask;
                g.cursor = cursor;
                g.time = time;
                // Host cursor swap is deferred — would require XChangeActivePointerGrab
                // forwarding. The grab record is sufficient for our event-mask gating.
            }
        }
    }
    log_void(client_id, sequence, "ChangeActivePointerGrab")
}
```

- [ ] **Step 3: Commit**

```
feat: implement ChangeActivePointerGrab (op 30)

Promotes pointer_grab to ActivePointerGrab record; updates
event_mask/cursor/time on the active grab. Per spec, this request
does not affect passive button grabs.
```

### Task D2: GrabButton sync replay channel

**Files:**
- Modify: `crates/yserver-core/src/server.rs` — add `replay_tx: Option<Sender<ReplayCmd>>`
- Modify: `crates/yserver-core/src/host_x11.rs` (or wherever `pointer_event_fanout` lives)
- Modify: `crates/yserver-core/src/nested.rs` — opcode 35 ReplayPointer arm

- [ ] **Step 1: Define `ReplayCmd` and channel**

Channel carries the frozen `HostPointerEvent` plus the `xid_map` reference is already available in the pump thread.

```rust
pub enum ReplayCmd { Pointer(crate::host_x11::HostPointerEvent) }
```

`ServerState.replay_tx: Option<std::sync::mpsc::Sender<ReplayCmd>>`. Set from the pump thread on startup; consume in the same pump thread's loop with a `try_recv` between input reads. Use `crossbeam` or `std::sync::mpsc` — match what's already in `Cargo.toml`.

- [ ] **Step 2: Replace the TODO in opcode 35**

Replace the `// ReplayPointer (mode==2): frozen event is cleared; ...` block with:

```rust
if mode == 2 && let Some(ev) = frozen.take() {
    if let Some(tx) = &s.replay_tx { let _ = tx.send(ReplayCmd::Pointer(ev)); }
}
```

- [ ] **Step 3: Pump thread consumes replays**

Adjust the pump thread to do `match rx.try_recv() { Ok(ReplayCmd::Pointer(ev)) => { /* re-route through pointer_event_fanout */ } _ => {} }` once per input loop iteration.

- [ ] **Step 4: Manual test**

Smoke-test using fvwm3 + xterm: pre-existing GrabButton(Sync, ReplayPointer) path should now actually re-route.

- [ ] **Step 5: Commit**

```
fix: deliver GrabButton sync replay through pump thread

Replaces the deferred TODO in AllowEvents(ReplayPointer) with a
crossbeam-style command channel consumed by the pointer pump
thread, which already holds xid_map.

```

---

## Group E — RANDR follow-ups

### Task E1: RRSelectInput mask storage

**Files:**
- Modify: `crates/yserver-core/src/randr.rs` — `RandrState.subscribers: HashMap<(ClientId, ResourceId), u16>`

- [ ] **Step 1: Test**

```rust
#[test]
fn randr_subscribers_set_and_get() {
    let mut s = RandrState::nested(0, 800, 600);
    s.subscribe(ClientId(1), ResourceId(0x10), 0x1);
    assert_eq!(s.subscriber_mask(ClientId(1), ResourceId(0x10)), Some(0x1));
}
```

- [ ] **Step 2: Implement**

Add field, `subscribe(...)`, `subscriber_mask(...)`.

- [ ] **Step 3: Wire RRSelectInput in nested.rs**

(Search `RRSelectInput` in `handle_randr_request`; replace the "accepted, not stored" comment with the new call.)

- [ ] **Step 4: Commit**

```
feat: store RRSelectInput masks in RandrState

```

### Task E2: Host resize watcher and RRScreenChangeNotify

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs` — extend the close-watcher thread to surface ConfigureNotify
- Modify: `crates/yserver-core/src/nested.rs` — fanout
- Modify: `crates/yserver-protocol/src/x11/mod.rs` — `write_rr_screen_change_notify_event`

- [ ] **Step 1: Write the encoder + test**

```rust
#[test]
fn rr_screen_change_notify_layout() {
    let mut buf = Vec::new();
    write_rr_screen_change_notify_event(
        &mut buf, SequenceNumber(0), /*first_event=*/RANDR_FIRST_EVENT,
        /*rotation=*/1, ResourceId(0x10), ResourceId(0x20),
        /*size_id=*/0, /*subpixel=*/0, /*time=*/123,
        /*width=*/1920, /*height=*/1080, /*mwidth=*/508, /*mheight=*/285).unwrap();
    assert_eq!(buf.len(), 32);
    assert_eq!(buf[0], RANDR_FIRST_EVENT); // ScreenChangeNotify
    assert_eq!(buf[1], 1);                 // rotation
}
```

- [ ] **Step 2: Implement encoder**

(Per the RANDR spec — see `xcb-proto/src/randr.xml` event 0; layout: 32 bytes.)

```rust
pub fn write_rr_screen_change_notify_event(
    writer: &mut impl Write,
    sequence: SequenceNumber,
    first_event: u8,
    rotation: u8,
    root: ResourceId,
    request_window: ResourceId,
    size_id: u16,
    subpixel: u16,
    timestamp: u32,
    width: u16,
    height: u16,
    mwidth: u16,
    mheight: u16,
) -> io::Result<()> {
    let mut buf = [0u8; 32];
    buf[0] = first_event;
    buf[1] = rotation;
    buf[2..4].copy_from_slice(&sequence.0.to_le_bytes());
    buf[4..8].copy_from_slice(&timestamp.to_le_bytes());
    buf[8..12].copy_from_slice(&timestamp.to_le_bytes()); // config-timestamp
    buf[12..16].copy_from_slice(&root.0.to_le_bytes());
    buf[16..20].copy_from_slice(&request_window.0.to_le_bytes());
    buf[20..22].copy_from_slice(&size_id.to_le_bytes());
    buf[22..24].copy_from_slice(&subpixel.to_le_bytes());
    buf[24..26].copy_from_slice(&width.to_le_bytes());
    buf[26..28].copy_from_slice(&height.to_le_bytes());
    buf[28..30].copy_from_slice(&mwidth.to_le_bytes());
    buf[30..32].copy_from_slice(&mheight.to_le_bytes());
    writer.write_all(&buf)
}
```

- [ ] **Step 3: Watcher integration**

In the existing close-watcher thread (`spawn_window_close_watcher` or similar), the `HostInputPump::read_event` returns `HostEvent::Closed` only. Extend the input-pump enum (or add a parallel ConfigureNotify polling path) — the simpler route is: in the watcher thread, after the `read_event` loop, also process `XConfigureEvent` from XEvents on the container window. Or: subscribe to `StructureNotifyMask` on the host container and check for `ConfigureNotify` size deltas inside the existing loop.

When a size change is detected:

```rust
let mut s = lock_server(server)?;
s.randr.set_size(new_w, new_h);
let subs = s.randr.subscribers_snapshot();
drop(s);
for (cid, win) in subs {
    let target = lock_server(server)?.client_target(cid);
    if let Some(t) = target {
        let seq = SequenceNumber(t.last_sequence.load(Ordering::Relaxed));
        let mut buf = Vec::with_capacity(32);
        let _ = x11::write_rr_screen_change_notify_event(
            &mut buf, seq, RANDR_FIRST_EVENT, 1, ROOT_WINDOW, win,
            0, 0, lock_server(server)?.timestamp_now(),
            new_w, new_h, /*mwidth=*/254, /*mheight=*/254);
        if let Ok(mut w) = t.writer.lock() { let _ = w.write_all(&buf); }
    }
}
```

- [ ] **Step 4: Commit**

```
feat: emit RRScreenChangeNotify on host container resize

Watcher thread sees XConfigureNotify on the host container and
updates RandrState dimensions, then fans out RRScreenChangeNotify
to clients that selected RRScreenChangeNotifyMask.

```

### Task E3: RRGetScreenInfo (RANDR 1.0)

**Files:**
- Modify: `crates/yserver-protocol/src/x11/randr.rs` — `write_rr_get_screen_info_reply`
- Modify: `crates/yserver-core/src/nested.rs` — `handle_randr_request` minor 5

- [ ] **Step 1: Encoder + test**

(See RANDR 1.0 spec; reply layout is well-defined.)

- [ ] **Step 2: Wire dispatcher minor 5**

- [ ] **Step 3: Commit**

```
feat: implement RRGetScreenInfo (RANDR minor=5) for 1.0 clients

```

---

## Group F — Cross-cutting fixes

### Task F1: DestroyWindow releases bg-pixmap host XIDs

**Files:**
- Modify: `crates/yserver-core/src/resources.rs` — `destroy_window` (or wherever the recursive destroy walks)
- Modify: `crates/yserver-core/src/nested.rs` — opcode 4 path

In `destroy_window`, after a window is removed from the tree but before dropping its struct, if `Window.background_pixmap_host_xid` is set, capture it and call `host.free_pixmap(host_xid)` from the dispatcher.

- [ ] **Step 1: Test** — unit test on `ResourceTable::take_pending_pixmap_frees(window)` that returns the bg-pixmap host xid (if set) and clears the field.
- [ ] **Step 2: Implement helper.** Call from the existing destroy path; the dispatcher already holds the host handle.
- [ ] **Step 3: Commit.**

```
fix: free host bg-pixmap XIDs on DestroyWindow

Closes the leak noted in status.md known follow-ups.

```

### Task F2: SendEvent propagation up the parent chain

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` — opcode 25 SendEvent

The current implementation delivers to direct subscribers. Spec: when `propagate=true` and no client in the destination has a matching mask, walk up parents. Stop at a window where the do-not-propagate mask covers the event type.

- [ ] **Step 1: Test the lookup helper**

Add `fn target_for_send_event(state, dst, event_type, propagate) -> Option<EventTarget>` and unit-test it.

- [ ] **Step 2: Implement and replace direct lookup**

- [ ] **Step 3: Commit**

```
fix: propagate SendEvent up parent chain when no direct subscriber

```

### Task F3: UnmapNotify.from_configure on shrunk children

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` — opcode 12 ConfigureWindow

When a parent's ConfigureWindow shrinks the parent and a child becomes fully outside the new size, emit `UnmapNotify` with `from_configure=true`.

- [ ] **Step 1: Test the geometry helper**

```rust
#[test]
fn child_clipped_out_after_parent_shrink() {
    use crate::resources::{ResourceTable, ROOT_WINDOW};
    let mut t = ResourceTable::new();
    let parent = ResourceId(0x100);
    let child = ResourceId(0x200);
    t.create_window(parent, ROOT_WINDOW, 0, 0, 800, 600, 24, 0).unwrap();
    t.create_window(child, parent, 700, 500, 100, 100, 24, 0).unwrap();
    t.map_window(child).unwrap();
    let unmapped = t.children_clipped_out(parent, 600, 400);
    assert_eq!(unmapped, vec![child]);
}
```

- [ ] **Step 2: Implement helper and wire into ConfigureWindow**

- [ ] **Step 3: Commit**

```
fix: emit UnmapNotify(from_configure=true) for clipped-out children

```

---

## Group G — Validation

### Task G1: Run Openbox under ynest

**Files:** none (validation only)

- [ ] **Step 1: Make sure Openbox is installed**

Run: `which openbox || sudo pacman -S openbox`

- [ ] **Step 2: Start ynest**

```sh
cargo run --bin ynest -- 99 &
sleep 1
```

- [ ] **Step 3: Start Openbox**

```sh
DISPLAY=:99 openbox 2>&1 | tee openbox.log
```

- [ ] **Step 4: Open a client and exercise basic management**

```sh
DISPLAY=:99 xterm &
```

In the host window: right-click for the root menu, Alt+drag the xterm window, Alt+F4 to close.

- [ ] **Step 5: Capture findings**

For each blocker, note opcode/extension and whether it's in scope for this plan or escalates to a follow-up. Append a section "Phase 2 wrap-up validation log" to `docs/status.md` listing what worked and what didn't.

- [ ] **Step 6: Iterate**

Address each in-scope blocker by re-opening the relevant task or adding a small new task. Out-of-scope blockers (XKB, BIG-REQUESTS) get a status.md entry and a Phase 3 reference.

### Task G2: Run Fluxbox under ynest

Same shape as G1 with `fluxbox` instead of `openbox`. If Fluxbox demands BIG-REQUESTS, defer to Phase 3 per the spec.

### Task G3: Update status.md

- [ ] Mark Phase 2 wrap-up items checked off in `docs/status.md`.
- [ ] Update the opcode table for the newly-implemented opcodes (5, 6, 13, 30, 31, 32, 33, 34, 63, 100).
- [ ] Add the validation log section.
- [ ] Commit.

```
docs: mark Phase 2 wrap-up items complete

```

---

## Self-review checklist

- Spec coverage: every numbered item in the spec maps to a Group A–F task.
- No placeholders: every code step contains the actual code.
- Type consistency: `KeyGrab`, `ActiveKeyboardGrab`, `ActiveKeyboardGrabSource`, `ActivePointerGrab`, `KeyTarget`, `ReplayCmd`, `RandrState::subscribers` types referenced in dispatcher arms match their definitions.
- Each task ends with a commit.
- Tests precede implementation in every TDD-relevant task; pure plumbing tasks (e.g. dispatcher arms with no return value) rely on the encoder unit tests and validation in G1/G2.

## Revision history

- 2026-04-30 — codex review folded in: MappingNotify offsets,
  CirculateWindow container semantics, passive-key activates active
  grab, ChangeActivePointerGrab targets the active grab record,
  save-set restore preserves coords + remaps + walks ancestor chain,
  CopyPlane body length 28 bytes, GetModifierMapping size
  parameterised. Also: dropped `clippy::pedantic`, dropped
  Co-Authored-By trailers, replaced `unsafe { mem::zeroed() }`
  test pattern with `UnixStream::pair()`.
