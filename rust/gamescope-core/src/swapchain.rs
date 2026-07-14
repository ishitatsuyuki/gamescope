//! State carried by the `gamescope_swapchain` protocol.

use crate::wire::{join_u64, split_u64};

/// Vulkan swapchain properties sent by Gamescope's WSI layer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwapchainFeedback {
    pub image_count: u32,
    pub vk_format: u32,
    pub vk_colorspace: u32,
    pub vk_composite_alpha: u32,
    pub vk_pre_transform: u32,
    pub vk_clipped: u32,
    pub vk_engine_name: String,
    pub hdr_metadata: Option<HdrMetadata>,
}

/// CTA-861-G static HDR metadata as encoded by the protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HdrMetadata {
    pub display_primary_red: Chromaticity,
    pub display_primary_green: Chromaticity,
    pub display_primary_blue: Chromaticity,
    pub white_point: Chromaticity,
    pub max_display_mastering_luminance: u32,
    pub min_display_mastering_luminance: u32,
    pub max_cll: u32,
    pub max_fall: u32,
}

impl HdrMetadata {
    /// Gamescope discards metadata with no CLL, no FALL, or a `(0, 0)` white
    /// point. The primaries and mastering luminance are otherwise accepted as
    /// supplied.
    #[must_use]
    pub const fn is_accepted_by_gamescope(self) -> bool {
        self.max_cll != 0
            && self.max_fall != 0
            && (self.white_point.x != 0 || self.white_point.y != 0)
    }
}

/// Fixed-point chromaticity words used by HDR metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Chromaticity {
    pub x: u32,
    pub y: u32,
}

/// Result of trying to attach HDR metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetHdrMetadataResult {
    Stored,
    DiscardedInvalid,
    MissingSwapchainFeedback,
}

/// Metadata consumed atomically by one surface commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitMetadata {
    pub feedback: Option<SwapchainFeedback>,
    pub present_id: Option<u32>,
    pub desired_present_time_ns: u64,
    pub vk_present_mode: Option<u32>,
}

/// Per-surface protocol state.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SwapchainState {
    feedback: Option<SwapchainFeedback>,
    present_id: Option<u32>,
    desired_present_time_ns: u64,
    vk_present_mode: Option<u32>,
}

impl SwapchainState {
    /// Replace the persistent Vulkan feedback, just like a new
    /// `swapchain_feedback` request.
    pub fn set_feedback(&mut self, feedback: SwapchainFeedback) {
        self.feedback = Some(feedback);
    }

    /// Current persistent feedback.
    #[must_use]
    pub const fn feedback(&self) -> Option<&SwapchainFeedback> {
        self.feedback.as_ref()
    }

    /// Store HDR metadata in the current feedback object.
    pub fn set_hdr_metadata(&mut self, metadata: HdrMetadata) -> SetHdrMetadataResult {
        let Some(feedback) = self.feedback.as_mut() else {
            return SetHdrMetadataResult::MissingSwapchainFeedback;
        };

        if !metadata.is_accepted_by_gamescope() {
            return SetHdrMetadataResult::DiscardedInvalid;
        }

        feedback.hdr_metadata = Some(metadata);
        SetHdrMetadataResult::Stored
    }

    /// Set the raw `VkPresentModeKHR` value for the next commit.
    pub const fn set_present_mode(&mut self, vk_present_mode: u32) {
        self.vk_present_mode = Some(vk_present_mode);
    }

    /// Set the ID and desired monotonic presentation time for the next commit.
    pub const fn set_present_time(&mut self, present_id: u32, high: u32, low: u32) {
        self.present_id = Some(present_id);
        self.desired_present_time_ns = join_u64(high, low);
    }

    /// Snapshot commit metadata and reset one-shot fields.
    ///
    /// Swapchain feedback, including accepted HDR metadata, intentionally
    /// persists. Present ID, desired time, and present mode are one-shot in
    /// `PrepareCommit`.
    pub fn prepare_commit(&mut self) -> CommitMetadata {
        CommitMetadata {
            feedback: self.feedback.clone(),
            present_id: self.present_id.take(),
            desired_present_time_ns: std::mem::take(&mut self.desired_present_time_ns),
            vk_present_mode: self.vk_present_mode.take(),
        }
    }
}

/// Fields for `gamescope_swapchain.past_present_timing`, in XML argument order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PastPresentTiming {
    pub present_id: u32,
    pub desired_present_time_ns: u64,
    pub actual_present_time_ns: u64,
    pub earliest_present_time_ns: u64,
    pub present_margin_ns: u64,
}

impl PastPresentTiming {
    /// Convert to the eight split time words expected by the wire event.
    #[must_use]
    pub const fn wire_time_words(self) -> [(u32, u32); 4] {
        [
            split_u64(self.desired_present_time_ns),
            split_u64(self.actual_present_time_ns),
            split_u64(self.earliest_present_time_ns),
            split_u64(self.present_margin_ns),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Chromaticity, HdrMetadata, SetHdrMetadataResult, SwapchainFeedback, SwapchainState,
    };

    fn feedback() -> SwapchainFeedback {
        SwapchainFeedback {
            image_count: 3,
            vk_format: 64,
            vk_colorspace: 1_000_104_002,
            vk_composite_alpha: 1,
            vk_pre_transform: 1,
            vk_clipped: 1,
            vk_engine_name: "test-engine".into(),
            hdr_metadata: None,
        }
    }

    fn valid_hdr() -> HdrMetadata {
        HdrMetadata {
            display_primary_red: Chromaticity {
                x: 35_400,
                y: 14_600,
            },
            display_primary_green: Chromaticity {
                x: 8_500,
                y: 39_850,
            },
            display_primary_blue: Chromaticity { x: 6_550, y: 2_300 },
            white_point: Chromaticity {
                x: 15_635,
                y: 16_450,
            },
            max_display_mastering_luminance: 1_000,
            min_display_mastering_luminance: 1,
            max_cll: 1_000,
            max_fall: 400,
        }
    }

    #[test]
    fn hdr_requires_feedback_and_gamescope_validity_fields() {
        let mut state = SwapchainState::default();
        assert_eq!(
            state.set_hdr_metadata(valid_hdr()),
            SetHdrMetadataResult::MissingSwapchainFeedback
        );

        state.set_feedback(feedback());
        let mut invalid = valid_hdr();
        invalid.max_fall = 0;
        assert_eq!(
            state.set_hdr_metadata(invalid),
            SetHdrMetadataResult::DiscardedInvalid
        );
        assert_eq!(state.feedback().unwrap().hdr_metadata, None);

        assert_eq!(
            state.set_hdr_metadata(valid_hdr()),
            SetHdrMetadataResult::Stored
        );
        assert_eq!(state.feedback().unwrap().hdr_metadata, Some(valid_hdr()));
    }

    #[test]
    fn commit_consumes_only_one_shot_fields() {
        let mut state = SwapchainState::default();
        state.set_feedback(feedback());
        state.set_present_mode(2);
        state.set_present_time(42, 0x0123_4567, 0x89ab_cdef);

        let first = state.prepare_commit();
        assert_eq!(first.present_id, Some(42));
        assert_eq!(first.desired_present_time_ns, 0x0123_4567_89ab_cdef);
        assert_eq!(first.vk_present_mode, Some(2));
        assert_eq!(first.feedback, Some(feedback()));

        let second = state.prepare_commit();
        assert_eq!(second.present_id, None);
        assert_eq!(second.desired_present_time_ns, 0);
        assert_eq!(second.vk_present_mode, None);
        assert_eq!(second.feedback, Some(feedback()));
    }
}
