//! One composited frame for one output.
//!
//! Phase 2 owns the renderable state: the BO slot the composite
//! targets, the descriptor pool slot allocated for it, and the
//! sync primitives (composite_fence). Phase 3 adds the per-frame
//! command buffer and scratch arena. `InFlightFrame` (in
//! `in_flight.rs`) wraps an `OutputFrame` with retirement
//! bookkeeping.

use ash::vk;

#[derive(Debug)]
pub struct OutputFrame {
    pub output_idx: usize,
    pub frame_id: u64,
    pub submitted_gen: u64,
    pub bo_slot: Option<usize>,
    /// Slot in the per-output `CompositePoolRing`. Released on
    /// retirement.
    pub composite_pool_slot: usize,
    /// Phase-1 sentinel `vk::Fence::null()`; phase 4 replaces with
    /// a real fence signalled by the composite submit.
    pub composite_fence: vk::Fence,
}

impl OutputFrame {
    pub fn new(
        output_idx: usize,
        frame_id: u64,
        submitted_gen: u64,
        bo_slot: Option<usize>,
        composite_pool_slot: usize,
        composite_fence: vk::Fence,
    ) -> Self {
        Self {
            output_idx,
            frame_id,
            submitted_gen,
            bo_slot,
            composite_pool_slot,
            composite_fence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_frame_records_all_fields() {
        let f = OutputFrame::new(0, 1, 7, Some(2), 1, vk::Fence::null());
        assert_eq!(f.output_idx, 0);
        assert_eq!(f.frame_id, 1);
        assert_eq!(f.submitted_gen, 7);
        assert_eq!(f.bo_slot, Some(2));
        assert_eq!(f.composite_pool_slot, 1);
    }
}
