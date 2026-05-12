//! Server-wide in-flight frame retirement queue.
//!
//! Tracks `OutputFrame`s that have been submitted but not yet
//! fully retired. Each frame has two retirement points:
//!
//! 1. GPU retirement — composite work complete on the GPU.
//!    Releases composite command buffer, descriptors, scratch
//!    bound to this frame. Phase 1 uses a `VkFence` polled via
//!    `vkGetFenceStatus`; later phases may switch to a timeline
//!    counter.
//!
//! 2. Scanout retirement — KMS has released the scanout BO via
//!    pageflip-complete (the BO is no longer on-screen). Releases
//!    the BO slot back to the pool.
//!
//! A frame is fully retired (and removed from the queue) only when
//! both bools are true. Phase 1 may unify the implementation; the
//! two-point split is the invariant.

use std::collections::VecDeque;

use ash::vk;

/// A single in-flight `OutputFrame`'s retirement bookkeeping.
///
/// The fields are public to the scheduler module so the polling
/// code (which lives in `KmsBackend` because it owns `VkContext`
/// and the BO pools) can set the bools directly.
#[derive(Debug)]
pub struct InFlightFrame {
    pub output_idx: usize,
    pub frame_id: u64,
    pub submitted_gen: u64,
    pub composite_fence: vk::Fence,
    pub bo_slot: Option<usize>,
    pub gpu_retired: bool,
    pub scanout_retired: bool,
}

impl InFlightFrame {
    pub fn fully_retired(&self) -> bool {
        self.gpu_retired && self.scanout_retired
    }
}

#[derive(Debug, Default)]
pub struct InFlight {
    frames: VecDeque<InFlightFrame>,
    next_frame_id: u64,
}

impl InFlight {
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn allocate_frame_id(&mut self) -> u64 {
        self.next_frame_id += 1;
        self.next_frame_id
    }

    pub fn push(&mut self, frame: InFlightFrame) {
        self.frames.push_back(frame);
    }

    pub fn frames(&self) -> impl Iterator<Item = &InFlightFrame> {
        self.frames.iter()
    }

    pub fn frames_mut(&mut self) -> impl Iterator<Item = &mut InFlightFrame> {
        self.frames.iter_mut()
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut InFlightFrame> {
        self.frames.get_mut(index)
    }

    /// Drain the prefix of fully-retired frames. Returns how many
    /// were drained. Stops at the first non-retired frame —
    /// out-of-order retirement is allowed in the bools but not in
    /// the queue, because resource lifetimes are layered on
    /// submission order.
    pub fn drain_retired(&mut self) -> usize {
        let mut drained = 0;
        while let Some(front) = self.frames.front() {
            if front.fully_retired() {
                self.frames.pop_front();
                drained += 1;
            } else {
                break;
            }
        }
        drained
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_frame(id: u64, output: usize) -> InFlightFrame {
        InFlightFrame {
            output_idx: output,
            frame_id: id,
            submitted_gen: id,
            composite_fence: vk::Fence::null(),
            bo_slot: Some(0),
            gpu_retired: false,
            scanout_retired: false,
        }
    }

    #[test]
    fn new_queue_is_empty() {
        let q = InFlight::default();
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn next_frame_id_is_monotonic() {
        let mut q = InFlight::default();
        let a = q.allocate_frame_id();
        let b = q.allocate_frame_id();
        assert_eq!(b, a + 1);
    }

    #[test]
    fn push_grows_queue() {
        let mut q = InFlight::default();
        q.push(mk_frame(1, 0));
        q.push(mk_frame(2, 1));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn drain_retired_removes_only_fully_retired_frames() {
        let mut q = InFlight::default();
        q.push(mk_frame(1, 0));
        q.push(mk_frame(2, 0));
        // GPU done on frame 1, scanout not yet.
        q.frames_mut().next().unwrap().gpu_retired = true;
        let drained = q.drain_retired();
        assert_eq!(drained, 0, "GPU-only retirement is not enough");
        // Scanout also done on frame 1.
        q.frames_mut().next().unwrap().scanout_retired = true;
        let drained = q.drain_retired();
        assert_eq!(drained, 1);
        assert_eq!(q.len(), 1);
        assert_eq!(q.frames().next().unwrap().frame_id, 2);
    }

    #[test]
    fn drain_retired_only_drains_prefix() {
        // Frames retire in submission order on the same output. The
        // queue is FIFO; out-of-order retirement (e.g. a later
        // frame's GPU work completing before an earlier frame's)
        // does not cause a hole — the later frame waits for the
        // earlier to drain.
        let mut q = InFlight::default();
        q.push(mk_frame(1, 0));
        q.push(mk_frame(2, 0));
        // Mark frame 2 fully retired, frame 1 not.
        {
            let mut iter = q.frames_mut();
            let _f1 = iter.next();
            let f2 = iter.next().unwrap();
            f2.gpu_retired = true;
            f2.scanout_retired = true;
        }
        let drained = q.drain_retired();
        assert_eq!(
            drained, 0,
            "must not drain frame 2 while frame 1 is still in-flight"
        );
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn is_empty_reflects_push_and_drain() {
        let mut q = InFlight::default();
        assert!(q.is_empty());
        q.push(mk_frame(1, 0));
        assert!(!q.is_empty());
        let mut iter = q.frames_mut();
        let f = iter.next().unwrap();
        f.gpu_retired = true;
        f.scanout_retired = true;
        drop(iter);
        q.drain_retired();
        assert!(q.is_empty());
    }

    #[test]
    fn drain_retired_on_empty_queue_returns_zero() {
        let mut q = InFlight::default();
        assert_eq!(q.drain_retired(), 0);
    }

    #[test]
    fn drain_retired_blocks_on_scanout_only_retirement() {
        // Symmetric to drain_retired_removes_only_fully_retired_frames
        // but exercising the scanout-only side. Both bits must be set.
        let mut q = InFlight::default();
        q.push(mk_frame(1, 0));
        let mut iter = q.frames_mut();
        let f = iter.next().unwrap();
        f.scanout_retired = true;
        drop(iter);
        assert_eq!(
            q.drain_retired(),
            0,
            "scanout-only retirement is not enough"
        );
    }

    #[test]
    fn get_mut_returns_frame_at_index() {
        let mut q = InFlight::default();
        q.push(mk_frame(1, 0));
        q.push(mk_frame(2, 1));
        assert_eq!(q.get_mut(0).unwrap().frame_id, 1);
        assert_eq!(q.get_mut(1).unwrap().frame_id, 2);
        assert!(q.get_mut(2).is_none());
    }
}
