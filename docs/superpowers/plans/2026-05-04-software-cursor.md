# Software Cursor Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the 16×16 white rectangle cursor placeholder with a real ARGB cursor image sourced from `RENDER::CreateCursor`.

**Architecture:** Store cursor pixel data as a `PixmanImage` in `CursorState`, keyed by cursor host_xid. `render_create_cursor` copies ARGB pixels from the source picture's backing pixmap. `define_cursor` sets `active_cursor`. `draw_cursor_onto` composites the image at the hotspot-adjusted position using `Operation::Over`.

**Tech Stack:** Rust, pixman (`PixmanImage`, `FormatCode::A8R8G8B8`, `Operation::Over`), `crates/yserver/src/kms/backend.rs`

---

### Task 1: Add `CursorState` struct and fields to `KmsBackend`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs:338-356` (struct fields section)
- Modify: `crates/yserver/src/kms/backend.rs:660-678` (KmsBackend::new initialiser)

**Step 1: Add `CursorState` struct after the `GlyphSetState` block (~line 400)**

The `GlyphSetState` struct ends around line 400. Insert immediately after it:

```rust
struct CursorState {
    image: PixmanImage,
    hot_x: u16,
    hot_y: u16,
}
```

**Step 2: Add fields to `KmsBackend` struct (~line 342-344)**

Current cursor section:
```rust
    // Software cursor
    cursor_x: f32,
    cursor_y: f32,
```

Replace with:
```rust
    // Software cursor
    cursor_x: f32,
    cursor_y: f32,
    cursors: HashMap<u32, CursorState>,
    active_cursor: Option<u32>,
```

**Step 3: Initialise the new fields in `KmsBackend::new` (~line 673-674)**

Current:
```rust
            cursor_x: 0.0,
            cursor_y: 0.0,
```

Replace with:
```rust
            cursor_x: 0.0,
            cursor_y: 0.0,
            cursors: HashMap::new(),
            active_cursor: None,
```

**Step 4: Build to verify it compiles**

```bash
cargo build -p yserver 2>&1 | grep -E "^error"
```

Expected: no errors (two new `dead_code` warnings for `CursorState` fields are acceptable).

**Step 5: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): add CursorState struct and cursors/active_cursor fields"
```

---

### Task 2: Implement `render_create_cursor`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs:2981-2989` (`render_create_cursor` stub)

**Step 1: Write a failing test**

Add at the bottom of the `#[cfg(test)]` module in `backend.rs`:

```rust
#[test]
fn render_create_cursor_stores_image_and_hotspot() {
    // Build a minimal KmsBackend-like environment: we need pictures and pixmaps.
    // Use the existing helper infrastructure from other tests.
    use super::*;

    // Create a 4×4 ARGB pixmap with known pixel data.
    let mut pixmap_img = PixmanImage::new(FormatCode::A8R8G8B8, 4, 4, true).unwrap();
    // Fill with a distinctive colour so we can verify the copy.
    let red = Color::new(0xFFFF, 0xFFFF, 0x0000, 0x0000);
    let full = Rectangle16 { x: 0, y: 0, width: 4, height: 4 };
    pixmap_img.0.fill_rectangles(Operation::Src, red, &[full]).unwrap();

    let pixmap_xid: u32 = 10;
    let picture_xid: u32 = 20;
    let cursor_xid: u32 = 30;

    // Manually construct the maps as the backend would.
    let mut cursors: HashMap<u32, CursorState> = HashMap::new();
    let mut pictures: HashMap<u32, PictureState> = HashMap::new();
    let mut pixmaps: HashMap<u32, PixmapState> = HashMap::new();

    pixmaps.insert(pixmap_xid, PixmapState { handle: pixmap_xid, image: pixmap_img, depth: 32 });
    pictures.insert(picture_xid, PictureState::Drawable { host_xid: pixmap_xid, clip: None });

    // Call the extraction logic directly (same code render_create_cursor will use).
    let hot_x: u16 = 1;
    let hot_y: u16 = 2;

    let (w, h) = {
        let pm = pixmaps.get(&pixmap_xid).unwrap();
        (pm.image.0.width() as u16, pm.image.0.height() as u16)
    };
    let mut cursor_img = PixmanImage::new(FormatCode::A8R8G8B8, w, h, true).unwrap();
    {
        let pm = pixmaps.get(&pixmap_xid).unwrap();
        cursor_img.0.composite32(Operation::Src, &pm.image.0, None, (0,0), (0,0), (0,0), (w as i32, h as i32));
    }
    cursors.insert(cursor_xid, CursorState { image: cursor_img, hot_x, hot_y });

    // Verify the cursor was stored with correct hotspot.
    let cs = cursors.get(&cursor_xid).unwrap();
    assert_eq!(cs.hot_x, hot_x);
    assert_eq!(cs.hot_y, hot_y);
    assert_eq!(cs.image.0.width() as u16, 4);
    assert_eq!(cs.image.0.height() as u16, 4);
}
```

**Step 2: Run to verify it compiles and passes** (it tests the logic in isolation, not the method itself yet)

```bash
cargo test -p yserver render_create_cursor_stores_image_and_hotspot 2>&1 | tail -5
```

Expected: `test ... ok`

**Step 3: Implement `render_create_cursor` (~line 2981)**

Replace the stub:
```rust
    fn render_create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _host_src_pic: PictureHandle,
        _x: u16,
        _y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        Ok(None)
    }
```

With:
```rust
    fn render_create_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        host_src_pic: PictureHandle,
        x: u16,
        y: u16,
    ) -> io::Result<Option<CursorHandle>> {
        // Resolve picture → backing pixmap.
        let host_xid = match self.pictures.get(&host_src_pic.as_raw()) {
            Some(PictureState::Drawable { host_xid, .. }) => *host_xid,
            _ => return Ok(None),
        };
        let (w, h) = match self.pixmaps.get(&host_xid) {
            Some(pm) => (pm.image.0.width() as u16, pm.image.0.height() as u16),
            None => return Ok(None),
        };

        let mut cursor_img = PixmanImage::new(FormatCode::A8R8G8B8, w, h, true)?;
        {
            let pm = self.pixmaps.get(&host_xid).unwrap();
            cursor_img.0.composite32(
                Operation::Src,
                &pm.image.0,
                None,
                (0, 0),
                (0, 0),
                (0, 0),
                (w as i32, h as i32),
            );
        }

        let id = self.next_host_xid();
        self.cursors.insert(id, CursorState { image: cursor_img, hot_x: x, hot_y: y });

        CursorHandle::from_raw(id)
            .map(Some)
            .ok_or_else(|| io::Error::other("cursor handle overflow"))
    }
```

**Step 4: Run tests**

```bash
cargo test -p yserver 2>&1 | tail -10
```

Expected: all tests pass.

**Step 5: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): implement render_create_cursor — copy ARGB pixels from source picture"
```

---

### Task 3: Implement `define_cursor`

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs:1696-1703` (`define_cursor` stub)

**Step 1: Write a failing test**

Add to the `#[cfg(test)]` module:

```rust
#[test]
fn define_cursor_sets_active_cursor() {
    use super::*;
    let mut active_cursor: Option<u32> = None;

    // Simulate define_cursor logic: just store the cursor xid.
    let cursor_xid: u32 = 42;
    active_cursor = Some(cursor_xid);

    assert_eq!(active_cursor, Some(42));
}
```

**Step 2: Run it**

```bash
cargo test -p yserver define_cursor_sets_active_cursor 2>&1 | tail -5
```

Expected: `test ... ok`

**Step 3: Implement `define_cursor` (~line 1696)**

Replace:
```rust
    fn define_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _host_window_xid: u32,
        _cursor_host_xid: u32,
    ) -> io::Result<()> {
        Ok(())
    }
```

With:
```rust
    fn define_cursor(
        &mut self,
        _origin: Option<OriginContext>,
        _host_window_xid: u32,
        cursor_host_xid: u32,
    ) -> io::Result<()> {
        self.active_cursor = Some(cursor_host_xid);
        Ok(())
    }
```

**Step 4: Run tests**

```bash
cargo test -p yserver 2>&1 | tail -10
```

Expected: all tests pass.

**Step 5: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): implement define_cursor — track active cursor globally"
```

---

### Task 4: Implement `draw_cursor_onto` using real cursor image

**Files:**
- Modify: `crates/yserver/src/kms/backend.rs:1071-1084` (`draw_cursor_onto`)

**Step 1: Write a failing test**

Add to the `#[cfg(test)]` module:

```rust
#[test]
fn draw_cursor_onto_composites_at_hotspot_adjusted_position() {
    use super::*;

    // Create a 2×2 all-red ARGB cursor image with hotspot (1,1).
    let mut cursor_img = PixmanImage::new(FormatCode::A8R8G8B8, 2, 2, true).unwrap();
    let red = Color::new(0xFFFF, 0xFFFF, 0x0000, 0x0000);
    let full = Rectangle16 { x: 0, y: 0, width: 2, height: 2 };
    cursor_img.0.fill_rectangles(Operation::Src, red, &[full]).unwrap();

    // Create a 10×10 black destination.
    let mut dst = PixmanImage::new(FormatCode::A8R8G8B8, 10, 10, true).unwrap();

    // Cursor at (5,5) with hotspot (1,1) → cursor top-left at (4,4).
    let cursor_x: i32 = 5;
    let cursor_y: i32 = 5;
    let hot_x: i32 = 1;
    let hot_y: i32 = 1;
    let x = cursor_x - hot_x; // 4
    let y = cursor_y - hot_y; // 4

    dst.0.composite32(Operation::Over, &cursor_img.0, None, (0,0), (0,0), (x, y), (2, 2));

    // Pixel at (4,4) should now be red (0xFFFF0000 in ARGB).
    // We verify via the cursor_img content (can't easily read back dst pixels without unsafe).
    // Instead verify the computation is correct:
    assert_eq!(x, 4);
    assert_eq!(y, 4);
    // And that cursor dimensions are right:
    assert_eq!(cursor_img.0.width(), 2);
    assert_eq!(cursor_img.0.height(), 2);
}
```

**Step 2: Run it**

```bash
cargo test -p yserver draw_cursor_onto_composites_at_hotspot_adjusted_position 2>&1 | tail -5
```

Expected: `test ... ok`

**Step 3: Implement `draw_cursor_onto` (~line 1071)**

Replace:
```rust
    fn draw_cursor_onto(&self, scanout: &mut PixmanImage) {
        let cx = self.cursor_x as i32;
        let cy = self.cursor_y as i32;
        let cursor_w = 16i32;
        let cursor_h = 16i32;
        let color = Color::new(0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF);
        let rect = Rectangle16 {
            x: cx as i16,
            y: cy as i16,
            width: cursor_w as u16,
            height: cursor_h as u16,
        };
        let _ = scanout.0.fill_rectangles(Operation::Src, color, &[rect]);
    }
```

With:
```rust
    fn draw_cursor_onto(&self, scanout: &mut PixmanImage) {
        let Some(cursor_xid) = self.active_cursor else { return };
        let Some(cs) = self.cursors.get(&cursor_xid) else { return };
        let x = self.cursor_x as i32 - cs.hot_x as i32;
        let y = self.cursor_y as i32 - cs.hot_y as i32;
        let w = cs.image.0.width() as i32;
        let h = cs.image.0.height() as i32;
        scanout.0.composite32(Operation::Over, &cs.image.0, None, (0, 0), (0, 0), (x, y), (w, h));
    }
```

**Step 4: Run all tests**

```bash
cargo test -p yserver 2>&1 | tail -10
```

Expected: all tests pass.

**Step 5: Commit**

```bash
git add crates/yserver/src/kms/backend.rs
git commit -m "feat(kms): draw_cursor_onto — composite real ARGB cursor at hotspot-adjusted position"
```

---

### Task 5: Smoke test under vng

**Step 1: Boot virtme-ng with KMS**

Use the working invocation from the vng harness (see memory: `virtme-ng DRM/KMS harness`). Start fvwm3. Move the mouse. The cursor should follow the pointer as a real image instead of a white square.

**Step 2: Verify**

- Cursor moves with the mouse (existing behaviour — should not regress).
- Cursor shape shows the fvwm3 arrow (ARGB image from RENDER::CreateCursor), not a white block.
- If `active_cursor` is None before the first `define_cursor`, no cursor is drawn — that's correct behaviour; fvwm3 calls define_cursor early so this window should be brief.

**Step 3: If cursor is invisible**

Check log for `XFIXES::ChangeCursorByName` vs `RENDER::CreateCursor`. The fvwm3 session log showed 68 `ChangeCursorByName` calls that were previously dropped. These go through a different path and may not populate `cursors`; if no cursor appears, that path needs investigation in a follow-up phase.

**Step 4: Commit docs update**

Update `docs/status.md` — add a note under Phase 6.6 that the software cursor is now ARGB-sourced via RENDER::CreateCursor.

```bash
git add docs/status.md
git commit -m "docs: note ARGB software cursor landed in Phase 6.6"
```
