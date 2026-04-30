use yserver_protocol::x11::randr as proto;

pub const OUTPUT_ID: u32 = 1;
pub const CRTC_ID: u32 = 2;
pub const MODE_ID: u32 = 3;

#[derive(Debug)]
pub struct RandrState {
    pub timestamp: u32,
    pub config_timestamp: u32,
    pub screen_width: u16,
    pub screen_height: u16,
    /// Derived from screen dimensions at 96 DPI, clamped to at least 1 mm.
    pub width_mm: u32,
    pub height_mm: u32,
}

impl RandrState {
    /// Create a `RandrState` for a nested (embedded) display of the given pixel dimensions.
    ///
    /// Physical size is estimated at 96 DPI: `pixels * 25.4 / 96 = pixels * 254 / 9600`.
    #[must_use]
    pub fn nested(timestamp: u32, width: u16, height: u16) -> Self {
        // Formula: pixels * 25.4 / 96 = pixels * 254 / 9600
        let width_mm = ((u32::from(width) * 254 + 4800) / 9600).max(1);
        let height_mm = ((u32::from(height) * 254 + 4800) / 9600).max(1);
        Self {
            timestamp,
            config_timestamp: timestamp,
            screen_width: width,
            screen_height: height,
            width_mm,
            height_mm,
        }
    }

    /// Returns `(min_width, min_height, max_width, max_height)`.
    ///
    /// For the first cut the minimum and maximum are both fixed at the current
    /// screen size (i.e. no dynamic resizing is supported yet).
    #[must_use]
    pub fn screen_size_range(&self) -> (u16, u16, u16, u16) {
        (
            self.screen_width,
            self.screen_height,
            self.screen_width,
            self.screen_height,
        )
    }

    pub fn resize(&mut self, timestamp: u32, width: u16, height: u16) {
        *self = Self::nested(timestamp, width, height);
    }

    /// Build a `ScreenResources` reply describing the single synthetic output/CRTC/mode.
    #[must_use]
    pub fn screen_resources_current(&self) -> proto::ScreenResources {
        let mode_name = format!("{}x{}", self.screen_width, self.screen_height).into_bytes();
        #[allow(clippy::cast_possible_truncation)]
        let name_len = mode_name.len() as u16;
        proto::ScreenResources {
            timestamp: self.timestamp,
            config_timestamp: self.config_timestamp,
            crtcs: vec![CRTC_ID],
            outputs: vec![OUTPUT_ID],
            modes: vec![proto::ModeInfo {
                id: MODE_ID,
                width: self.screen_width,
                height: self.screen_height,
                dot_clock: u32::from(self.screen_width) * u32::from(self.screen_height) * 60,
                hsync_start: self.screen_width + 40,
                hsync_end: self.screen_width + 168,
                htotal: self.screen_width + 264,
                hskew: 0,
                vsync_start: self.screen_height + 1,
                vsync_end: self.screen_height + 4,
                vtotal: self.screen_height + 28,
                name_len,
                mode_flags: 0,
            }],
            mode_names: mode_name,
        }
    }

    /// Return output info for the single synthetic output.
    ///
    /// Returns `None` if `output_id` does not match `OUTPUT_ID`.
    #[must_use]
    pub fn output_info(
        &self,
        output_id: u32,
        config_timestamp: u32,
    ) -> Option<OutputInfoReplyData> {
        if output_id != OUTPUT_ID {
            return None;
        }
        let _ = config_timestamp; // accepted but not used in first cut
        Some(OutputInfoReplyData {
            timestamp: self.timestamp,
            crtc: CRTC_ID,
            width_mm: self.width_mm,
            height_mm: self.height_mm,
        })
    }

    /// Return CRTC info for the single synthetic CRTC.
    ///
    /// Returns `None` if `crtc_id` does not match `CRTC_ID`.
    #[must_use]
    pub fn crtc_info(&self, crtc_id: u32, config_timestamp: u32) -> Option<CrtcInfoData> {
        if crtc_id != CRTC_ID {
            return None;
        }
        let _ = config_timestamp;
        Some(CrtcInfoData {
            timestamp: self.timestamp,
            width: self.screen_width,
            height: self.screen_height,
        })
    }
}

/// Data returned by [`RandrState::output_info`].
pub struct OutputInfoReplyData {
    pub timestamp: u32,
    pub crtc: u32,
    pub width_mm: u32,
    pub height_mm: u32,
}

/// Data returned by [`RandrState::crtc_info`].
pub struct CrtcInfoData {
    pub timestamp: u32,
    pub width: u16,
    pub height: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_constructor_dimensions() {
        // 800x600 at 96 DPI:
        //   width_mm  = (800*254 + 4800) / 9600 = (203200 + 4800) / 9600 = 208000 / 9600 = 21
        //   height_mm = (600*254 + 4800) / 9600 = (152400 + 4800) / 9600 = 157200 / 9600 = 16
        let state = RandrState::nested(42, 800, 600);
        assert_eq!(state.screen_width, 800);
        assert_eq!(state.screen_height, 600);
        assert_eq!(state.width_mm, 21);
        assert_eq!(state.height_mm, 16);
        assert_eq!(state.timestamp, 42);
        assert_eq!(state.config_timestamp, 42);
    }

    #[test]
    fn unknown_output_returns_none() {
        let state = RandrState::nested(0, 800, 600);
        assert!(state.output_info(99, 0).is_none());
    }

    #[test]
    fn unknown_crtc_returns_none() {
        let state = RandrState::nested(0, 800, 600);
        assert!(state.crtc_info(99, 0).is_none());
    }

    #[test]
    fn screen_resources_current_ids() {
        let state = RandrState::nested(0, 800, 600);
        let res = state.screen_resources_current();
        assert_eq!(res.crtcs, vec![CRTC_ID]);
        assert_eq!(res.outputs, vec![OUTPUT_ID]);
        assert_eq!(res.modes[0].id, MODE_ID);
    }
}
