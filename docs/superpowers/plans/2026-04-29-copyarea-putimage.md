# CopyArea + PutImage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement Phase 1 `CopyArea` (opcode 62) and `PutImage` (opcode 72), including host-backed pixmaps, so `xterm` scrolling and image-backed text/cursor paths work in ynest.

**Architecture:** Add protocol parsers for `CopyArea` and `PutImage`; extend pixmaps with optional host xids; add a `HostDrawableTarget` resolver that covers both windows and pixmaps; add host `CreatePixmap` / `FreePixmap` / `CopyArea` / `PutImage` forwarding methods; then wire opcodes 53, 54, 62, and 72 in `nested.rs`. Preserve the existing Phase 1 route rule: only forward top-level windows and zero-offset child windows; unsupported child-offset routing remains a successful no-op.

**Tech Stack:** Rust 2024, std `Mutex`/`Arc`, existing local X11 wire helpers.

**Spec:** [`docs/superpowers/specs/2026-04-29-copyarea-putimage-design.md`](../specs/2026-04-29-copyarea-putimage-design.md).

**Project conventions:**

```sh
RUSTC_WRAPPER= cargo fmt --all -- --check
RUSTC_WRAPPER= cargo check --workspace
RUSTC_WRAPPER= cargo test --workspace
```

Run the full set before considering the plan complete. Manual smoke tests are required because the important behavior is visible terminal scrolling.

---

## File Structure

| Path | Status | Purpose |
|---|---|---|
| `crates/yserver-protocol/src/x11/mod.rs` | modify | Add `CopyAreaRequest`, `ImageFormat`, `PutImageRequest`, parser helpers, missing core error constants, and parser unit tests |
| `crates/yserver-core/src/resources.rs` | modify | Add `Pixmap.host_xid`, `HostDrawableTarget`, `host_drawable_target`, pixmap host-xid mutation/removal helpers, and resolver tests |
| `crates/yserver-core/src/host_x11.rs` | modify | Add host pixmap lifecycle and request forwarding for `CopyArea` / `PutImage` |
| `crates/yserver-core/src/nested.rs` | modify | Wire host-backed `CreatePixmap` / `FreePixmap`, opcode 62, opcode 72, validation, and route handling |
| `docs/status.md` | modify | Mark the punch-list item complete after implementation and smoke tests pass |

The implementation is four commits, sequenced so each commit compiles:

1. **Protocol parsers + errors** — pure protocol additions and tests.
2. **Resource drawable resolution** — host-backed pixmap metadata and resolver tests.
3. **HostX11 forwarding methods** — low-level host requests, no opcode behavior change yet.
4. **Opcode wiring + smoke** — integrate `CreatePixmap`, `FreePixmap`, `CopyArea`, `PutImage`, then update status.

---

## Commit 1 — Protocol Parsers + Error Constants

### Task 1.1: Add missing core error constants

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs` (`pub mod error`)

- [ ] **Step 1: Extend `x11::error`**

Add the constants needed by real drawing validation:

```rust
pub const BAD_DRAWABLE: u8 = 9;
pub const BAD_GC: u8 = 13;
```

Keep existing numeric constants unchanged.

- [ ] **Step 2: Run a quick check**

Run:

```sh
RUSTC_WRAPPER= cargo check -p yserver-protocol
```

Expected: compiles.

### Task 1.2: Add `CopyAreaRequest`

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs`

- [ ] **Step 1: Add the request struct near existing drawing request structs**

Place it close to `ClearAreaRequest`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CopyAreaRequest {
    pub src: ResourceId,
    pub dst: ResourceId,
    pub gc: ResourceId,
    pub src_x: i16,
    pub src_y: i16,
    pub dst_x: i16,
    pub dst_y: i16,
    pub width: u16,
    pub height: u16,
}
```

- [ ] **Step 2: Add the parser near `clear_area_request`**

```rust
pub fn copy_area_request(body: &[u8]) -> Option<CopyAreaRequest> {
    Some(CopyAreaRequest {
        src: ResourceId(read_u32_le(body.get(0..4)?)),
        dst: ResourceId(read_u32_le(body.get(4..8)?)),
        gc: ResourceId(read_u32_le(body.get(8..12)?)),
        src_x: read_i16_le(body.get(12..14)?),
        src_y: read_i16_le(body.get(14..16)?),
        dst_x: read_i16_le(body.get(16..18)?),
        dst_y: read_i16_le(body.get(18..20)?),
        width: read_u16_le(body.get(20..22)?),
        height: read_u16_le(body.get(22..24)?),
    })
}
```

- [ ] **Step 3: Add parser tests**

Add tests in the existing `#[cfg(test)] mod tests` for:

- all fields parse correctly from a 24-byte body.
- short bodies return `None`.

Run:

```sh
RUSTC_WRAPPER= cargo test -p yserver-protocol copy_area
```

Expected: parser tests pass.

### Task 1.3: Add `PutImageRequest`

**Files:**
- Modify: `crates/yserver-protocol/src/x11/mod.rs`

- [ ] **Step 1: Add format and request types**

Place these near `CopyAreaRequest`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImageFormat {
    XyBitmap,
    XyPixmap,
    ZPixmap,
    Unknown(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PutImageRequest<'a> {
    pub format: ImageFormat,
    pub drawable: ResourceId,
    pub gc: ResourceId,
    pub width: u16,
    pub height: u16,
    pub dst_x: i16,
    pub dst_y: i16,
    pub left_pad: u8,
    pub depth: u8,
    pub data: &'a [u8],
}
```

- [ ] **Step 2: Add a conversion helper**

```rust
fn image_format(value: u8) -> ImageFormat {
    match value {
        0 => ImageFormat::XyBitmap,
        1 => ImageFormat::XyPixmap,
        2 => ImageFormat::ZPixmap,
        other => ImageFormat::Unknown(other),
    }
}
```

- [ ] **Step 3: Add the parser near drawing parsers**

```rust
pub fn put_image_request(format: u8, body: &[u8]) -> Option<PutImageRequest<'_>> {
    Some(PutImageRequest {
        format: image_format(format),
        drawable: ResourceId(read_u32_le(body.get(0..4)?)),
        gc: ResourceId(read_u32_le(body.get(4..8)?)),
        width: read_u16_le(body.get(8..10)?),
        height: read_u16_le(body.get(10..12)?),
        dst_x: read_i16_le(body.get(12..14)?),
        dst_y: read_i16_le(body.get(14..16)?),
        left_pad: *body.get(16)?,
        depth: *body.get(17)?,
        data: body.get(20..)?,
    })
}
```

- [ ] **Step 4: Add parser tests**

Cover:

- `ZPixmap` parses all scalar fields and preserves the data slice.
- format byte `0` maps to `XyBitmap`.
- format byte `1` maps to `XyPixmap`.
- unknown format maps to `Unknown(value)`.
- short bodies return `None`.

Run:

```sh
RUSTC_WRAPPER= cargo test -p yserver-protocol put_image
```

Expected: parser tests pass.

- [ ] **Step 5: Verify commit**

Run:

```sh
RUSTC_WRAPPER= cargo fmt --all -- --check
RUSTC_WRAPPER= cargo check --workspace
```

Expected: both pass.

---

## Commit 2 — Host-Backed Pixmap Metadata + Drawable Resolver

### Task 2.1: Extend `Pixmap`

**Files:**
- Modify: `crates/yserver-core/src/resources.rs`

- [ ] **Step 1: Add `host_xid`**

Change `Pixmap`:

```rust
pub struct Pixmap {
    pub id: ResourceId,
    pub drawable: ResourceId,
    pub width: u16,
    pub height: u16,
    pub depth: u8,
    pub host_xid: Option<u32>,
    pub owner: ClientId,
}
```

- [ ] **Step 2: Initialize in `ResourceTable::create_pixmap`**

Set:

```rust
host_xid: None,
```

The host xid is assigned after successful host creation in the opcode handler.

### Task 2.2: Add pixmap host-xid helpers

**Files:**
- Modify: `crates/yserver-core/src/resources.rs`

- [ ] **Step 1: Add setter**

```rust
pub fn set_pixmap_host_xid(&mut self, id: ResourceId, host_xid: u32) {
    if let Some(pixmap) = self.pixmaps.get_mut(&id.0) {
        pixmap.host_xid = Some(host_xid);
    }
}
```

- [ ] **Step 2: Add remover that returns the removed pixmap**

Change `free_pixmap` from returning `()` to:

```rust
pub fn free_pixmap(&mut self, id: ResourceId) -> Option<Pixmap> {
    self.pixmaps.remove(&id.0)
}
```

Update existing call sites to ignore the return value until Commit 4:

```rust
let _ = s.resources.free_pixmap(pixmap);
```

### Task 2.3: Add `HostDrawableTarget`

**Files:**
- Modify: `crates/yserver-core/src/resources.rs`

- [ ] **Step 1: Add enum near `TopLevelTarget`**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostDrawableTarget {
    Window {
        nested: ResourceId,
        top_level: ResourceId,
        host_xid: u32,
        x_offset: i16,
        y_offset: i16,
        depth: u8,
    },
    Pixmap {
        nested: ResourceId,
        host_xid: u32,
        width: u16,
        height: u16,
        depth: u8,
    },
}
```

- [ ] **Step 2: Add convenience methods**

`depth()` is **required** (used by CopyArea and PutImage handlers in
Commit 4). `has_zero_window_offset` is optional — `routed_host_xid`
pattern-matches directly and does not need it.

```rust
impl HostDrawableTarget {
    pub fn depth(self) -> u8 {
        match self {
            Self::Window { depth, .. } | Self::Pixmap { depth, .. } => depth,
        }
    }
}
```

Keep it small and obvious.

### Task 2.4: Add `host_drawable_target`

**Files:**
- Modify: `crates/yserver-core/src/resources.rs`

- [ ] **Step 1: Implement resolver**

```rust
#[must_use]
pub fn host_drawable_target(&self, id: ResourceId) -> Option<HostDrawableTarget> {
    if let Some(window) = self.windows.get(&id.0) {
        let target = self.top_level_host_target(id)?;
        return Some(HostDrawableTarget::Window {
            nested: id,
            top_level: target.top_level,
            host_xid: target.host_xid,
            x_offset: target.x_offset,
            y_offset: target.y_offset,
            depth: window.depth,
        });
    }

    let pixmap = self.pixmaps.get(&id.0)?;
    Some(HostDrawableTarget::Pixmap {
        nested: id,
        host_xid: pixmap.host_xid?,
        width: pixmap.width,
        height: pixmap.height,
        depth: pixmap.depth,
    })
}
```

- [ ] **Step 2: Add resolver tests**

Add tests for:

- top-level window resolves with depth and host xid.
- child window resolves with accumulated offsets.
- host-backed pixmap resolves.
- pixmap without `host_xid` returns `None`.
- unknown drawable returns `None`.

Run:

```sh
RUSTC_WRAPPER= cargo test -p yserver-core host_drawable_target
```

Expected: tests pass.

### Task 2.5: Verify commit

- [ ] **Step 1: Run formatting and workspace check**

```sh
RUSTC_WRAPPER= cargo fmt --all -- --check
RUSTC_WRAPPER= cargo check --workspace
```

Expected: both pass.

---

## Commit 3 — HostX11 Pixmap + Image Forwarding

### Task 3.1: Add host pixmap lifecycle

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs`

- [ ] **Step 1: Add `create_pixmap`**

```rust
pub fn create_pixmap(
    &mut self,
    host_xid: u32,
    depth: u8,
    width: u16,
    height: u16,
) -> io::Result<()> {
    self.sequence = self.sequence.wrapping_add(1);
    let mut out = Vec::new();
    out.push(53);
    out.push(depth);
    write_u16(&mut out, 4);
    write_u32(&mut out, host_xid);
    write_u32(&mut out, self.window_id);
    write_u16(&mut out, width);
    write_u16(&mut out, height);
    self.stream.write_all(&out)?;
    self.stream.flush()
}
```

- [ ] **Step 2: Add `free_pixmap`**

```rust
pub fn free_pixmap(&mut self, host_xid: u32) -> io::Result<()> {
    self.sequence = self.sequence.wrapping_add(1);
    let mut out = Vec::new();
    out.push(54);
    out.push(0);
    write_u16(&mut out, 2);
    write_u32(&mut out, host_xid);
    self.stream.write_all(&out)?;
    self.stream.flush()
}
```

### Task 3.2: Add `copy_area`

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs`

- [ ] **Step 1: Add method**

```rust
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
    out.push(62);
    out.push(0);
    write_u16(&mut out, 7);
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
```

### Task 3.3: Add `put_image`

**Files:**
- Modify: `crates/yserver-core/src/host_x11.rs`

- [ ] **Step 1: Add method**

```rust
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
    let padded_len = padded_len(data.len());
    let length_units = 6 + u16::try_from(padded_len / 4)
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "image is too large"))?;
    self.sequence = self.sequence.wrapping_add(1);
    let mut out = Vec::new();
    out.push(72);
    out.push(2); // ZPixmap
    write_u16(&mut out, length_units);
    write_u32(&mut out, host_xid);
    write_u32(&mut out, self.gc_id);
    write_u16(&mut out, width);
    write_u16(&mut out, height);
    write_i16(&mut out, dst_x);
    write_i16(&mut out, dst_y);
    out.push(0); // left-pad
    out.push(depth);
    write_u16(&mut out, 0);
    out.extend_from_slice(data);
    out.resize(24 + padded_len, 0);
    self.stream.write_all(&out)?;
    self.stream.flush()
}
```

Use the existing private `padded_len` helper if present; otherwise add a private helper matching existing padding conventions in this file.

### Task 3.4: Verify commit

- [ ] **Step 1: Run formatting and check**

```sh
RUSTC_WRAPPER= cargo fmt --all -- --check
RUSTC_WRAPPER= cargo check --workspace
```

Expected: both pass.

---

## Commit 4 — Wire Opcodes 53, 54, 62, 72

### Task 4.1: Add local helpers in `nested.rs`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs`

- [ ] **Step 1: Import `HostDrawableTarget`**

Extend the resources import:

```rust
resources::{HostDrawableTarget, MapState, Pixmap, ROOT_COLORMAP, ROOT_VISUAL, ROOT_WINDOW, Window},
```

- [ ] **Step 2: Add supported-depth helper near other small helpers**

```rust
fn supported_pixmap_depth(depth: u8) -> bool {
    matches!(depth, 24 | 32)
}
```

If manual testing shows depth-1 pixmaps are needed before `PutImage XYBitmap`, extend to `matches!(depth, 1 | 24 | 32)`.

- [ ] **Step 3: Add image byte-length helper**

```rust
fn zpixmap_expected_len(width: u16, height: u16, depth: u8) -> Option<usize> {
    let bits_per_pixel = match depth {
        24 | 32 => 32usize,
        _ => return None,
    };
    let stride_bits = usize::from(width).checked_mul(bits_per_pixel)?;
    let stride_bytes = stride_bits.div_ceil(32).checked_mul(4)?;
    stride_bytes.checked_mul(usize::from(height))
}
```

- [ ] **Step 4: Add route helper for Phase 1**

```rust
fn routed_host_xid(target: HostDrawableTarget) -> Option<u32> {
    match target {
        HostDrawableTarget::Window { host_xid, x_offset: 0, y_offset: 0, .. } => Some(host_xid),
        HostDrawableTarget::Pixmap { host_xid, .. } => Some(host_xid),
        HostDrawableTarget::Window { .. } => None,
    }
}
```

### Task 4.2: Wire host-backed `CreatePixmap`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` (opcode 53)

- [ ] **Step 1: Preserve existing ID validation**

Keep the current `BadIDChoice` validation exactly where it is.

- [ ] **Step 2: Validate source drawable and requested depth**

After ID validation:

```rust
let drawable_exists = {
    let s = lock_server(server)?;
    s.resources.window(request.drawable).is_some() || s.resources.pixmap(request.drawable).is_some()
};
if !drawable_exists {
    return emit_x11_error(writer, sequence, x11::error::BAD_DRAWABLE, request.drawable.0, 53);
}
if !supported_pixmap_depth(request.depth) {
    return emit_x11_error(writer, sequence, x11::error::BAD_VALUE, u32::from(request.depth), 53);
}
```

- [ ] **Step 3: Create host pixmap before storing host xid**

```rust
let host_xid = if let Some(host) = host
    && let Ok(mut host) = host.lock()
{
    let xid = host.allocate_xid();
    match host.create_pixmap(xid, request.depth, request.width, request.height) {
        Ok(()) => Some(xid),
        Err(err) => {
            warn!("client {} host CreatePixmap failed: {err}", client_id.0);
            None
        }
    }
} else {
    None
};
```

- [ ] **Step 4: Store pixmap and optional host xid**

```rust
{
    let mut s = lock_server(server)?;
    s.resources.create_pixmap(client_id, request);
    if let Some(host_xid) = host_xid {
        s.resources.set_pixmap_host_xid(request.pixmap, host_xid);
    }
}
```

### Task 4.3: Wire host-backed `FreePixmap`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` (opcode 54)

- [ ] **Step 1: Capture removed pixmap**

```rust
let removed = {
    let mut s = lock_server(server)?;
    s.resources.free_pixmap(pixmap)
};
```

- [ ] **Step 2: Free host pixmap**

```rust
if let Some(host_xid) = removed.and_then(|pixmap| pixmap.host_xid)
    && let Some(host) = host
    && let Ok(mut host) = host.lock()
{
    host.free_pixmap(host_xid)?;
}
```

### Task 4.4: Wire `CopyArea`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` (opcode 62)

- [ ] **Step 1: Replace the stub**

```rust
62 => {
    if let Some(request) = x11::copy_area_request(body) {
        if request.width == 0 || request.height == 0 {
            return log_void(client_id, sequence, "CopyArea");
        }

        let (gc_exists, src_exists, dst_exists, src, dst) = {
            let s = lock_server(server)?;
            (
                s.resources.gc(request.gc).is_some(),
                s.resources.window(request.src).is_some()
                    || s.resources.pixmap(request.src).is_some(),
                s.resources.window(request.dst).is_some()
                    || s.resources.pixmap(request.dst).is_some(),
                s.resources.host_drawable_target(request.src),
                s.resources.host_drawable_target(request.dst),
            )
        };
        if !gc_exists {
            return emit_x11_error(writer, sequence, x11::error::BAD_GC, request.gc.0, 62);
        }
        if !src_exists {
            return emit_x11_error(writer, sequence, x11::error::BAD_DRAWABLE, request.src.0, 62);
        }
        if !dst_exists {
            return emit_x11_error(writer, sequence, x11::error::BAD_DRAWABLE, request.dst.0, 62);
        }
        // src/dst exist but have no host backing (no host_xid set yet, or
        // non-zero child offset) — silently drop, same as other Phase 1 no-ops.
        if let (Some(src), Some(dst)) = (src, dst) {
            if src.depth() != dst.depth() {
                return emit_x11_error(writer, sequence, x11::error::BAD_MATCH, request.dst.0, 62);
            }
            if let (Some(src_host_xid), Some(dst_host_xid)) =
                (routed_host_xid(src), routed_host_xid(dst))
                && let Some(host) = host
                && let Ok(mut host) = host.lock()
            {
                host.copy_area(
                    src_host_xid,
                    dst_host_xid,
                    request.src_x,
                    request.src_y,
                    request.dst_x,
                    request.dst_y,
                    request.width,
                    request.height,
                )?;
            }
        }
    }
    log_void(client_id, sequence, "CopyArea")
}
```

- [ ] **Step 2: Verify `HostDrawableTarget::depth()` was added in Task 2.3**

This was already implemented as a required method there.

### Task 4.5: Wire `PutImage`

**Files:**
- Modify: `crates/yserver-core/src/nested.rs` (opcode 72)

- [ ] **Step 1: Replace the stub**

```rust
72 => {
    if let Some(request) = x11::put_image_request(header.data, body) {
        if request.width == 0 || request.height == 0 {
            return log_void(client_id, sequence, "PutImage");
        }

        let (gc_exists, drawable_exists, target) = {
            let s = lock_server(server)?;
            (
                s.resources.gc(request.gc).is_some(),
                s.resources.window(request.drawable).is_some()
                    || s.resources.pixmap(request.drawable).is_some(),
                s.resources.host_drawable_target(request.drawable),
            )
        };
        if !gc_exists {
            return emit_x11_error(writer, sequence, x11::error::BAD_GC, request.gc.0, 72);
        }
        if !drawable_exists {
            return emit_x11_error(writer, sequence, x11::error::BAD_DRAWABLE, request.drawable.0, 72);
        }

        if request.format != x11::ImageFormat::ZPixmap {
            return emit_x11_error(writer, sequence, x11::error::BAD_VALUE, u32::from(header.data), 72);
        }
        if request.left_pad != 0 {
            return emit_x11_error(writer, sequence, x11::error::BAD_VALUE, u32::from(request.left_pad), 72);
        }

        // target is None when the drawable has no host backing yet — drop silently.
        if let Some(target) = target {
            if request.depth != target.depth() {
                return emit_x11_error(writer, sequence, x11::error::BAD_MATCH, u32::from(request.depth), 72);
            }
            let Some(expected_len) =
                zpixmap_expected_len(request.width, request.height, request.depth)
            else {
                return emit_x11_error(writer, sequence, x11::error::BAD_MATCH, u32::from(request.depth), 72);
            };
            if request.data.len() < expected_len {
                return emit_x11_error(writer, sequence, x11::error::BAD_LENGTH, u32::try_from(request.data.len()).unwrap_or(u32::MAX), 72);
            }

            if let Some(host_xid) = routed_host_xid(target)
                && let Some(host) = host
                && let Ok(mut host) = host.lock()
            {
                host.put_image(
                    host_xid,
                    request.depth,
                    request.width,
                    request.height,
                    request.dst_x,
                    request.dst_y,
                    &request.data[..expected_len],
                )?;
            }
        }
    }
    log_void(client_id, sequence, "PutImage")
}
```

- [ ] **Step 2: Check formatting**

The nested handler is already dense. Let `cargo fmt` normalize the `let-else` and chained `if let` layout.

### Task 4.6: Add focused tests where practical

**Files:**
- Modify: `crates/yserver-core/src/resources.rs`
- Modify: `crates/yserver-core/src/nested.rs` only if handler-level tests already exist for comparable opcode paths

- [ ] **Step 1: Prefer resource and parser tests over brittle handler tests**

The current server does not have a lightweight mock-host harness for opcode handlers. Do not build one only for this punch-list item.

- [ ] **Step 2: Add unit tests for `zpixmap_expected_len`**

Cover:

- `24, width=2, height=3` returns `24`.
- `32, width=2, height=3` returns `24`.
- unsupported depth returns `None`.
- overflow returns `None` if a practical case exists.

### Task 4.7: Verify and smoke test

- [ ] **Step 1: Run workspace checks**

```sh
RUSTC_WRAPPER= cargo fmt --all -- --check
RUSTC_WRAPPER= cargo check --workspace
RUSTC_WRAPPER= cargo test --workspace
```

Expected: all pass.

- [ ] **Step 2: Run ynest**

```sh
RUSTC_WRAPPER= cargo run --bin ynest -- 99
```

- [ ] **Step 3: Smoke existing Phase 1 clients**

```sh
DISPLAY=:99 xeyes
DISPLAY=:99 xclock
DISPLAY=:99 xev
```

Expected: no regressions.

- [ ] **Step 4: Smoke xterm scrolling**

```sh
DISPLAY=:99 xterm
```

Inside xterm, generate scrolling:

```sh
seq 1 200
```

Expected:

- terminal scrolls cleanly.
- no stale black rectangles after scrolling.
- prompt redraw and typing still work.

### Task 4.8: Update status

**Files:**
- Modify: `docs/status.md`

- [ ] **Step 1: Mark punch-list item done**

Change:

```md
- [ ] **`CopyArea` and `PutImage`.**
```

to:

```md
- [x] **`CopyArea` and `PutImage`.**
```

Add a short note that Phase 1 supports `ZPixmap` and host-backed pixmap/window copies, while `XYBitmap`, `XYPixmap`, and non-zero child-window coordinate translation remain follow-ups.

- [ ] **Step 2: Final status check**

```sh
git status --short
```

Expected changed files are limited to the files in this plan plus any mechanical formatting in touched modules.

---

## Known Follow-Ups

- `XYBitmap` / `XYPixmap` support if a Phase 1 client actually needs it.
- `GraphicsExpose` / `NoExpose` if a client waits for copy completion events.
- GC function and plane-mask tracking for non-default raster operations.
- Coordinate translation for non-zero-offset child windows.
- MIT-SHM image upload path in Phase 4.
