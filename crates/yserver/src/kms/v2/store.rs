//! `DrawableStore` — drawable storage + lifetime + damage.
//!
//! Per rendering-model-v2 spec § "DrawableStore — drawable storage
//! + lifetime" and Stage 2 plan substage 2b. Owns every drawable's
//! storage handle, refcount, retirement-generation against I6a
//! [`FenceTicket`]s, image-layout state, and the **two damage
//! lists** per I5 (presentation damage with snapshot/ack
//! semantics + protocol damage for the DAMAGE extension).
//!
//! Stage 2b lands the structure + tests. KmsBackendV2 wires a
//! handful of allocation paths through; full wiring (every
//! allocation method on the Backend trait) arrives across
//! Stages 2c–2d as those substages need the metadata side of
//! the drawables they paint into.
//!
//! Storage allocation is **split** from the metadata layer:
//! `PlatformBackend` creates the Vk handles ([`Storage`]) and
//! hands them to `DrawableStore::allocate`. This keeps the
//! store's allocation path uniform across production (real
//! VkContext) and tests (synthesised null handles) — both go
//! through the same metadata bookkeeping.

#![allow(
    dead_code,
    reason = "DrawableStore primitives are consumed by Stages 2c–2e"
)]

use std::collections::HashMap;

use ash::vk;

use super::platform::{FenceTicket, PlatformBackend};

// ────────────────────────────────────────────────────────────────
// Identity + classification
// ────────────────────────────────────────────────────────────────

/// Opaque per-drawable handle. Stable across resize (the
/// `vk::Image` may change but the id doesn't).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct DrawableId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrawableKind {
    /// Single root storage covering the full virtual-screen
    /// extent. Always scene-participating.
    Root,
    /// One per InputOutput window (after `map_subwindow`).
    /// Scene-participation toggles with map state.
    Window,
    /// One per X11 Pixmap. Never scene-participating.
    Pixmap,
    /// One per server-allocated cursor (Cursor / GlyphCursor /
    /// RenderCreateCursor). Scene-participating when active.
    Cursor,
    /// COMPOSITE `NameWindowPixmap` / `AllocateRedirectedBacking`
    /// target. Per I4, never scene-participating in v2 Stage 2
    /// (Stage 4 connects them to the visible scene path).
    RedirectedBacking,
    // COW deferred to Stage 4.
}

// ────────────────────────────────────────────────────────────────
// Storage handles — the Vk side of a drawable.
// PlatformBackend creates these; DrawableStore borrows them.
// ────────────────────────────────────────────────────────────────

/// The Vk resources backing one drawable. Created via
/// [`PlatformBackend::allocate_drawable_storage`] (production)
/// or [`Storage::for_tests_null`] (unit tests that exercise
/// metadata bookkeeping without touching Vk).
pub(crate) struct Storage {
    pub(crate) image: vk::Image,
    pub(crate) memory: vk::DeviceMemory,
    pub(crate) image_view: vk::ImageView,
    pub(crate) extent: vk::Extent2D,
    pub(crate) format: vk::Format,
    /// Current layout tracked outside the Vk driver — see
    /// [`Drawable::record_layout_transition`] for the central
    /// mutation point. Single source of truth for what the
    /// next op's barrier `srcLayout` should be.
    pub(crate) current_layout: vk::ImageLayout,
    /// Set when the storage was made via `for_tests_null`. The
    /// `Drop` path skips destroying null handles.
    pub(crate) is_test_stub: bool,
}

impl Storage {
    /// Production constructor — Vk handles owned by `PlatformBackend::
    /// allocate_drawable_storage`. Initial layout is `UNDEFINED`;
    /// transitions tracked thereafter via
    /// [`Drawable::record_layout_transition`].
    pub(crate) fn new_server_owned(
        image: vk::Image,
        memory: vk::DeviceMemory,
        image_view: vk::ImageView,
        extent: vk::Extent2D,
        format: vk::Format,
    ) -> Self {
        Self {
            image,
            memory,
            image_view,
            extent,
            format,
            current_layout: vk::ImageLayout::UNDEFINED,
            is_test_stub: false,
        }
    }

    /// Test-only constructor with null Vk handles. Used by
    /// unit tests that exercise refcount / damage / snapshot
    /// logic without needing a live VkContext.
    #[doc(hidden)]
    pub(crate) fn for_tests_null(extent: vk::Extent2D, format: vk::Format) -> Self {
        Self {
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            image_view: vk::ImageView::null(),
            extent,
            format,
            current_layout: vk::ImageLayout::UNDEFINED,
            is_test_stub: true,
        }
    }

    /// Destroy the underlying Vk handles. Called by
    /// `DrawableStore` after the last consumer has released
    /// the drawable AND its `FenceTicket` is signaled. No-op
    /// for test stubs.
    fn destroy(&mut self, platform: &PlatformBackend) {
        if self.is_test_stub {
            return;
        }
        let Some(vk) = platform.vk.as_ref() else {
            // No VkContext — happens only in malformed test
            // fixtures. Log + leak.
            log::error!(
                "Storage::destroy: no VkContext available for image {:?}",
                self.image,
            );
            return;
        };
        unsafe {
            if self.image_view != vk::ImageView::null() {
                vk.device.destroy_image_view(self.image_view, None);
            }
            if self.image != vk::Image::null() {
                vk.device.destroy_image(self.image, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                vk.device.free_memory(self.memory, None);
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────
// RegionSet — minimal Vec<Rect2D> with union / subtract.
//
// Stage 2 regions are typically <= a handful of rects per
// drawable, so a Vec-backed set is fast enough. Full pixman-
// style region algebra arrives later if profiling shows it.
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub(crate) struct RegionSet {
    rects: Vec<vk::Rect2D>,
}

impl RegionSet {
    pub(crate) fn new() -> Self {
        Self { rects: Vec::new() }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    pub(crate) fn rects(&self) -> &[vk::Rect2D] {
        &self.rects
    }

    /// Add a rect. No coalescing in Stage 2; if a hot path
    /// shows overlap-induced over-paint, we add canonicalize
    /// here.
    pub(crate) fn add(&mut self, rect: vk::Rect2D) {
        if rect.extent.width == 0 || rect.extent.height == 0 {
            return;
        }
        self.rects.push(rect);
    }

    /// Union with another set. O(n) — for Stage 2's small
    /// region counts this is fine.
    pub(crate) fn union_with(&mut self, other: &RegionSet) {
        for &r in &other.rects {
            self.add(r);
        }
    }

    /// Subtract `other` from `self` rect-by-rect. The
    /// implementation is conservative (treats overlap as "kept")
    /// — only exact-match rects are removed. Stage 2's
    /// snapshot/ack flows always pass back a slice of `self`
    /// so this is sufficient. Full region-difference algebra
    /// lands later.
    pub(crate) fn subtract(&mut self, other: &RegionSet) {
        if other.rects.is_empty() {
            return;
        }
        self.rects.retain(|r| {
            !other
                .rects
                .iter()
                .any(|o| o.offset == r.offset && o.extent == r.extent)
        });
    }

    pub(crate) fn clear(&mut self) {
        self.rects.clear();
    }

    /// Bounding rect over every rect in the set. Returns `None`
    /// if the set is empty. Stage 2e uses this for the
    /// buffer-age repaint scissor — we don't yet split the
    /// scissor per-rect; Stage 5 may tighten if profiling shows
    /// over-paint matters.
    pub(crate) fn bounding_rect(&self) -> Option<vk::Rect2D> {
        let mut iter = self.rects.iter().copied();
        let first = iter.next()?;
        let mut x0 = first.offset.x;
        let mut y0 = first.offset.y;
        let mut x1 = first.offset.x.saturating_add_unsigned(first.extent.width);
        let mut y1 = first.offset.y.saturating_add_unsigned(first.extent.height);
        for r in iter {
            x0 = x0.min(r.offset.x);
            y0 = y0.min(r.offset.y);
            x1 = x1.max(r.offset.x.saturating_add_unsigned(r.extent.width));
            y1 = y1.max(r.offset.y.saturating_add_unsigned(r.extent.height));
        }
        Some(vk::Rect2D {
            offset: vk::Offset2D { x: x0, y: y0 },
            extent: vk::Extent2D {
                width: u32::try_from((x1 - x0).max(0)).unwrap_or(0),
                height: u32::try_from((y1 - y0).max(0)).unwrap_or(0),
            },
        })
    }

    /// Clone — used by snapshot/ack capture paths. The
    /// `derive(Clone)` already covers this; this method is just
    /// a name for readability at call sites.
    #[must_use]
    pub(crate) fn snapshot(&self) -> RegionSet {
        self.clone()
    }
}

// ────────────────────────────────────────────────────────────────
// DamageSnapshot — peek/ack token for the I5 snapshot/ack rule.
// ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct DamageSnapshot {
    pub(crate) id: DrawableId,
    pub(crate) epoch: u64,
    pub(crate) region: RegionSet,
}

// ────────────────────────────────────────────────────────────────
// Drawable — one entry in DrawableStore.
// ────────────────────────────────────────────────────────────────

pub(crate) struct Drawable {
    pub(crate) id: DrawableId,
    pub(crate) xid: u32,
    pub(crate) kind: DrawableKind,
    pub(crate) depth: u8,
    pub(crate) refcount: u32,
    pub(crate) scene_participating: bool,
    pub(crate) storage: Storage,

    /// I6a: latest render-completion ticket for which this
    /// drawable was a consumer (read or written) in flight. None
    /// = no GPU work has touched it since the last retirement.
    /// Coalesces — overwritten by the newest touch. Per cross-
    /// cutting §5 the underlying Arc keeps prior consumers
    /// alive via their own clones.
    pub(crate) last_render_ticket: Option<FenceTicket>,

    /// Presentation damage — region the scene needs to re-blit.
    /// Accumulates only when `scene_participating` is true.
    /// Drained via [`peek_presentation_damage`] +
    /// [`ack_presentation_damage`].
    pub(crate) presentation_damage: RegionSet,
    pub(crate) presentation_damage_epoch: u64,

    /// Protocol damage — region reported to DAMAGE-ext clients.
    /// Accumulates on every paint, regardless of scene
    /// participation. Drained via the DAMAGE-ext dispatcher.
    pub(crate) protocol_damage: RegionSet,
}

impl Drawable {
    /// Record an image-layout transition on `cb` with full
    /// producer/consumer access masks. Updates
    /// `storage.current_layout` so subsequent ops see the
    /// correct old-layout in their barrier.
    ///
    /// **Single source of truth** for what the current layout
    /// is. Reading or writing `current_layout` outside this
    /// method is a layered correctness hazard — see Risk 11
    /// in the Stage 2 plan.
    pub(crate) fn record_layout_transition(
        &mut self,
        vk: &crate::kms::vk::device::VkContext,
        cb: vk::CommandBuffer,
        target_layout: vk::ImageLayout,
        src_stage: vk::PipelineStageFlags2,
        src_access: vk::AccessFlags2,
        dst_stage: vk::PipelineStageFlags2,
        dst_access: vk::AccessFlags2,
    ) {
        if self.storage.is_test_stub {
            // Tests don't issue real Vk; just update the
            // tracker so logic-side assertions can verify.
            self.storage.current_layout = target_layout;
            return;
        }
        let barrier = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(src_stage)
            .src_access_mask(src_access)
            .dst_stage_mask(dst_stage)
            .dst_access_mask(dst_access)
            .old_layout(self.storage.current_layout)
            .new_layout(target_layout)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.storage.image)
            .subresource_range(
                vk::ImageSubresourceRange::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .level_count(1)
                    .layer_count(1),
            );
        let dep =
            vk::DependencyInfo::default().image_memory_barriers(std::slice::from_ref(&barrier));
        unsafe { vk.device.cmd_pipeline_barrier2(cb, &dep) };
        self.storage.current_layout = target_layout;
    }
}

// ────────────────────────────────────────────────────────────────
// Errors + retirement decision
// ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) enum AllocError {
    /// Two allocations collided on the same xid; caller is
    /// responsible for picking a fresh one (`KmsCore::next_host_xid`).
    XidInUse,
    /// Caller passed an unsupported `(depth, format)` combo.
    UnsupportedFormat,
    /// VkContext / pool allocation failure.
    Vk(vk::Result),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetireDecision {
    /// Refcount > 0 after decref; storage stays.
    StillReferenced,
    /// Refcount hit zero AND no fence ticket attached (or it
    /// was already signaled). Vk handles destroyed; entry
    /// removed from the map.
    Destroyed,
    /// Refcount hit zero but a fence ticket is still
    /// unsignaled. Entry parked in `pending_retire`; future
    /// `poll_pending_retire` calls will sweep it once the
    /// fence fires.
    PendingFence,
}

// ────────────────────────────────────────────────────────────────
// DrawableStore — the map + accessors.
// ────────────────────────────────────────────────────────────────

pub(crate) struct DrawableStore {
    next_id: u64,
    entries: HashMap<DrawableId, Drawable>,
    by_xid: HashMap<u32, DrawableId>,
    /// Drawables that hit refcount-zero but whose ticket isn't
    /// signaled yet. `poll_pending_retire` drains.
    pending_retire: Vec<DrawableId>,
}

impl DrawableStore {
    pub(crate) fn new() -> Self {
        Self {
            next_id: 1,
            entries: HashMap::new(),
            by_xid: HashMap::new(),
            pending_retire: Vec::new(),
        }
    }

    /// Allocate a fresh drawable. The caller has already built
    /// the `Storage` (via PlatformBackend in production, or
    /// `Storage::for_tests_null` in unit tests). Refcount
    /// starts at 1; layout is `UNDEFINED`; both damage lists
    /// empty.
    ///
    /// # Errors
    ///
    /// - `XidInUse` if `xid` already maps to a drawable.
    pub(crate) fn allocate(
        &mut self,
        xid: u32,
        kind: DrawableKind,
        depth: u8,
        scene_participating: bool,
        storage: Storage,
    ) -> Result<DrawableId, AllocError> {
        if self.by_xid.contains_key(&xid) {
            return Err(AllocError::XidInUse);
        }
        let id = DrawableId(self.next_id);
        self.next_id = self.next_id.checked_add(1).expect("DrawableId overflow");
        let drawable = Drawable {
            id,
            xid,
            kind,
            depth,
            refcount: 1,
            scene_participating,
            storage,
            last_render_ticket: None,
            presentation_damage: RegionSet::new(),
            presentation_damage_epoch: 0,
            protocol_damage: RegionSet::new(),
        };
        self.entries.insert(id, drawable);
        self.by_xid.insert(xid, id);
        Ok(id)
    }

    pub(crate) fn lookup(&self, xid: u32) -> Option<DrawableId> {
        self.by_xid.get(&xid).copied()
    }

    pub(crate) fn get(&self, id: DrawableId) -> Option<&Drawable> {
        self.entries.get(&id)
    }

    pub(crate) fn get_mut(&mut self, id: DrawableId) -> Option<&mut Drawable> {
        self.entries.get_mut(&id)
    }

    pub(crate) fn get_by_xid(&self, xid: u32) -> Option<&Drawable> {
        self.lookup(xid).and_then(|id| self.get(id))
    }

    pub(crate) fn get_by_xid_mut(&mut self, xid: u32) -> Option<&mut Drawable> {
        let id = self.lookup(xid)?;
        self.entries.get_mut(&id)
    }

    pub(crate) fn incref(&mut self, id: DrawableId) {
        if let Some(d) = self.entries.get_mut(&id) {
            d.refcount = d.refcount.saturating_add(1);
        }
    }

    /// Drop one reference. If refcount hits zero, decide
    /// retirement: synchronous-destroy if no fence is
    /// pending; otherwise park in `pending_retire`.
    pub(crate) fn decref(
        &mut self,
        platform: &mut PlatformBackend,
        id: DrawableId,
    ) -> RetireDecision {
        let Some(drawable) = self.entries.get_mut(&id) else {
            return RetireDecision::Destroyed;
        };
        if drawable.refcount > 1 {
            drawable.refcount -= 1;
            return RetireDecision::StillReferenced;
        }
        drawable.refcount = 0;
        let ticket_ready = match drawable.last_render_ticket.as_ref() {
            None => true,
            Some(t) => match platform.vk.as_ref() {
                Some(vk) => t.poll_signaled(vk),
                None => true, // no Vk (tests) — treat as signaled
            },
        };
        if ticket_ready {
            self.destroy_now(platform, id);
            RetireDecision::Destroyed
        } else {
            self.pending_retire.push(id);
            RetireDecision::PendingFence
        }
    }

    /// Internal: destroy storage and remove from maps.
    fn destroy_now(&mut self, platform: &mut PlatformBackend, id: DrawableId) {
        let Some(mut drawable) = self.entries.remove(&id) else {
            return;
        };
        self.by_xid.remove(&drawable.xid);
        drawable.storage.destroy(platform);
        // last_render_ticket drops here; its Rc inner refcount
        // ensures the underlying fence handle stays alive until
        // every consumer that cloned it has also dropped.
    }

    /// Flip scene-participation. When set to false, **clears
    /// unpresented presentation damage and bumps the epoch**
    /// (per codex round 1 point 5): an unmap means the scene
    /// shouldn't repaint from this storage; any in-flight
    /// snapshot that ack's against the new epoch will become
    /// a no-op subtract against an empty set. Protocol damage
    /// unaffected.
    pub(crate) fn set_scene_participating(&mut self, id: DrawableId, v: bool) {
        let Some(d) = self.entries.get_mut(&id) else {
            return;
        };
        let was = d.scene_participating;
        d.scene_participating = v;
        if was && !v {
            d.presentation_damage.clear();
            d.presentation_damage_epoch = d.presentation_damage_epoch.checked_add(1).unwrap_or(0);
        }
    }

    /// Accumulate damage. Presentation list only if
    /// `scene_participating`; protocol list always. Bumps
    /// presentation_damage_epoch when presentation_damage
    /// was actually mutated.
    pub(crate) fn damage(&mut self, id: DrawableId, rect: vk::Rect2D) {
        let Some(d) = self.entries.get_mut(&id) else {
            return;
        };
        d.protocol_damage.add(rect);
        if d.scene_participating {
            d.presentation_damage.add(rect);
            d.presentation_damage_epoch = d.presentation_damage_epoch.checked_add(1).unwrap_or(0);
        }
    }

    /// Snapshot for the SceneCompositor to ack later.
    /// Returns `None` if the drawable doesn't exist or isn't
    /// scene-participating (no presentation damage to peek).
    pub(crate) fn peek_presentation_damage(&self, id: DrawableId) -> Option<DamageSnapshot> {
        let d = self.entries.get(&id)?;
        if !d.scene_participating {
            return None;
        }
        Some(DamageSnapshot {
            id,
            epoch: d.presentation_damage_epoch,
            region: d.presentation_damage.clone(),
        })
    }

    /// Ack: if `snap.epoch == current_epoch`, clear live
    /// damage. If `snap.epoch < current_epoch`, paint arrived
    /// between peek and ack — subtract only the snapshot's
    /// region so post-peek damage survives.
    ///
    /// Per codex round 1 point 5: if scene_participating
    /// flipped to false since the snapshot, the live damage
    /// is already empty and the subtract is a no-op.
    pub(crate) fn ack_presentation_damage(&mut self, snap: DamageSnapshot) {
        let Some(d) = self.entries.get_mut(&snap.id) else {
            return;
        };
        if snap.epoch == d.presentation_damage_epoch {
            d.presentation_damage.clear();
        } else {
            d.presentation_damage.subtract(&snap.region);
        }
    }

    /// Peek protocol damage. Distinct from presentation
    /// damage: DAMAGE-ext clients see this independently of
    /// scene participation.
    pub(crate) fn peek_protocol_damage(&self, id: DrawableId) -> RegionSet {
        self.entries
            .get(&id)
            .map(|d| d.protocol_damage.clone())
            .unwrap_or_default()
    }

    pub(crate) fn subtract_protocol_damage(&mut self, id: DrawableId, region: &RegionSet) {
        if let Some(d) = self.entries.get_mut(&id) {
            d.protocol_damage.subtract(region);
        }
    }

    /// Record an in-flight ticket. Coalesces: replaces any
    /// prior ticket. The Rc inner stays alive in other
    /// holders (e.g. SceneCompositor's pending-ack list) per
    /// cross-cutting §5.
    pub(crate) fn touch_render_fence(&mut self, id: DrawableId, ticket: FenceTicket) {
        if let Some(d) = self.entries.get_mut(&id) {
            d.last_render_ticket = Some(ticket);
        }
    }

    /// Sweep `pending_retire`. Drawables whose ticket has
    /// signaled (or never had one) get destroyed and removed.
    pub(crate) fn poll_pending_retire(&mut self, platform: &mut PlatformBackend) {
        let mut survivors = Vec::with_capacity(self.pending_retire.len());
        let mut to_destroy = Vec::new();
        for id in std::mem::take(&mut self.pending_retire) {
            let ready = match self.entries.get(&id) {
                None => true, // already gone
                Some(d) => match d.last_render_ticket.as_ref() {
                    None => true,
                    Some(t) => match platform.vk.as_ref() {
                        Some(vk) => t.poll_signaled(vk),
                        None => true,
                    },
                },
            };
            if ready {
                to_destroy.push(id);
            } else {
                survivors.push(id);
            }
        }
        for id in to_destroy {
            self.destroy_now(platform, id);
        }
        self.pending_retire = survivors;
    }

    /// Number of live entries (test introspection).
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Number of entries pending retirement.
    pub(crate) fn pending_retire_count(&self) -> usize {
        self.pending_retire.len()
    }

    /// Stage-1b-era compatibility constructor (was `stub()`).
    /// Kept so existing callers in `kms::v2::backend` continue
    /// to compile until they're updated to call `new()`.
    pub(crate) fn stub() -> Self {
        Self::new()
    }
}

// ────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_storage() -> Storage {
        Storage::for_tests_null(
            vk::Extent2D {
                width: 16,
                height: 16,
            },
            vk::Format::B8G8R8A8_UNORM,
        )
    }

    fn rect(x: i32, y: i32, w: u32, h: u32) -> vk::Rect2D {
        vk::Rect2D {
            offset: vk::Offset2D { x, y },
            extent: vk::Extent2D {
                width: w,
                height: h,
            },
        }
    }

    #[test]
    fn allocate_and_lookup() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1234, DrawableKind::Pixmap, 32, false, stub_storage())
            .expect("allocate");
        assert_eq!(s.lookup(0x1234), Some(id));
        let d = s.get(id).expect("get");
        assert_eq!(d.xid, 0x1234);
        assert_eq!(d.kind, DrawableKind::Pixmap);
        assert_eq!(d.depth, 32);
        assert_eq!(d.refcount, 1);
        assert!(!d.scene_participating);
    }

    #[test]
    fn allocate_rejects_xid_collision() {
        let mut s = DrawableStore::new();
        s.allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .expect("first");
        let err = s
            .allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .expect_err("collision");
        assert!(matches!(err, AllocError::XidInUse));
    }

    #[test]
    fn decref_destroys_immediately_when_no_ticket() {
        let mut s = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        let id = s
            .allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .unwrap();
        assert_eq!(s.decref(&mut platform, id), RetireDecision::Destroyed);
        assert!(s.lookup(0x1).is_none());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn incref_then_decref_keeps_alive() {
        let mut s = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        let id = s
            .allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .unwrap();
        s.incref(id);
        assert_eq!(s.decref(&mut platform, id), RetireDecision::StillReferenced);
        assert!(s.lookup(0x1).is_some());
        assert_eq!(s.decref(&mut platform, id), RetireDecision::Destroyed);
        assert!(s.lookup(0x1).is_none());
    }

    #[test]
    fn damage_pixmap_only_accumulates_protocol() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .unwrap();
        s.damage(id, rect(0, 0, 4, 4));
        let d = s.get(id).unwrap();
        assert!(d.presentation_damage.is_empty());
        assert_eq!(d.protocol_damage.rects().len(), 1);
        assert_eq!(d.presentation_damage_epoch, 0);
    }

    #[test]
    fn damage_window_accumulates_both_and_bumps_epoch() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        s.damage(id, rect(0, 0, 4, 4));
        let d = s.get(id).unwrap();
        assert_eq!(d.presentation_damage.rects().len(), 1);
        assert_eq!(d.protocol_damage.rects().len(), 1);
        assert_eq!(d.presentation_damage_epoch, 1);
        s.damage(id, rect(8, 8, 2, 2));
        assert_eq!(s.get(id).unwrap().presentation_damage_epoch, 2);
    }

    #[test]
    fn peek_and_ack_clears_when_epoch_matches() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        s.damage(id, rect(0, 0, 4, 4));
        let snap = s.peek_presentation_damage(id).expect("snap");
        assert_eq!(snap.epoch, 1);
        s.ack_presentation_damage(snap);
        assert!(s.get(id).unwrap().presentation_damage.is_empty());
    }

    #[test]
    fn paint_between_peek_and_ack_survives() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        s.damage(id, rect(0, 0, 4, 4));
        let snap = s.peek_presentation_damage(id).unwrap();
        // Paint arrives between peek and ack.
        s.damage(id, rect(8, 8, 2, 2));
        s.ack_presentation_damage(snap);
        // The post-peek paint survives.
        let live = &s.get(id).unwrap().presentation_damage;
        assert_eq!(live.rects().len(), 1);
        assert_eq!(live.rects()[0].offset, vk::Offset2D { x: 8, y: 8 });
    }

    #[test]
    fn set_scene_participating_false_clears_unpresented_damage() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        s.damage(id, rect(0, 0, 4, 4));
        assert_eq!(s.get(id).unwrap().presentation_damage.rects().len(), 1);
        let epoch_before = s.get(id).unwrap().presentation_damage_epoch;
        s.set_scene_participating(id, false);
        let d = s.get(id).unwrap();
        assert!(d.presentation_damage.is_empty());
        assert!(d.presentation_damage_epoch > epoch_before);
        // Protocol damage is unaffected.
        assert_eq!(d.protocol_damage.rects().len(), 1);
    }

    #[test]
    fn ack_after_unmap_is_noop_against_empty_live_damage() {
        // Codex round 1 point 5: peek; unmap; ack against the
        // stale snapshot. Live damage is already empty; the
        // subtract-by-snapshot is a no-op. No corruption.
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Window, 24, true, stub_storage())
            .unwrap();
        s.damage(id, rect(0, 0, 4, 4));
        let snap = s.peek_presentation_damage(id).unwrap();
        s.set_scene_participating(id, false);
        s.ack_presentation_damage(snap);
        let d = s.get(id).unwrap();
        assert!(d.presentation_damage.is_empty());
        assert!(!d.scene_participating);
    }

    #[test]
    fn protocol_damage_independent_of_presentation() {
        let mut s = DrawableStore::new();
        let id = s
            .allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .unwrap();
        s.damage(id, rect(0, 0, 4, 4));
        s.damage(id, rect(8, 8, 2, 2));
        let proto = s.peek_protocol_damage(id);
        assert_eq!(proto.rects().len(), 2);
        let mut to_drop = RegionSet::new();
        to_drop.add(rect(0, 0, 4, 4));
        s.subtract_protocol_damage(id, &to_drop);
        let remaining = s.peek_protocol_damage(id);
        assert_eq!(remaining.rects().len(), 1);
        assert_eq!(remaining.rects()[0].offset, vk::Offset2D { x: 8, y: 8 });
    }

    #[test]
    fn poll_pending_retire_with_no_ticket_destroys() {
        let mut s = DrawableStore::new();
        let mut platform = PlatformBackend::for_tests();
        let id = s
            .allocate(0x1, DrawableKind::Pixmap, 32, false, stub_storage())
            .unwrap();
        // Force into pending_retire by simulating a never-
        // signaled ticket (here we just push id manually since
        // we can't easily construct a FenceTicket in tests).
        s.entries.get_mut(&id).unwrap().refcount = 0;
        s.pending_retire.push(id);
        s.poll_pending_retire(&mut platform);
        // No ticket attached → treated as signaled → destroyed.
        assert!(s.lookup(0x1).is_none());
        assert_eq!(s.pending_retire_count(), 0);
    }

    #[test]
    fn region_set_subtract_exact_match_only() {
        let mut r = RegionSet::new();
        r.add(rect(0, 0, 4, 4));
        r.add(rect(8, 8, 2, 2));
        let mut sub = RegionSet::new();
        sub.add(rect(0, 0, 4, 4));
        r.subtract(&sub);
        assert_eq!(r.rects().len(), 1);
        assert_eq!(r.rects()[0].offset, vk::Offset2D { x: 8, y: 8 });
    }

    #[test]
    fn region_set_zero_extent_ignored() {
        let mut r = RegionSet::new();
        r.add(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 0,
                height: 4,
            },
        });
        r.add(vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D {
                width: 4,
                height: 0,
            },
        });
        assert!(r.is_empty());
    }
}
