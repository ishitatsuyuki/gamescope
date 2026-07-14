//! Behavior behind the version 6 `gamescope_control` global.

use std::collections::HashMap;

use crate::wire::split_u64;

/// Features advertised, in the same order as `gamescope_control_bind`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum Feature {
    Done = 0,
    ReshadeShaders = 1,
    DisplayInfo = 2,
    PixelFilter = 3,
    RefreshCycleOnlyChangeRefreshRate = 4,
    MuraCorrection = 5,
    Look = 6,
    PerfQuery = 7,
}

/// One `feature_support` event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeatureSupport {
    pub feature: Feature,
    pub version: u32,
    pub flags: u32,
}

/// Exact feature sequence sent to every control binding.
pub const FEATURE_ADVERTISEMENT: [FeatureSupport; 8] = [
    FeatureSupport {
        feature: Feature::ReshadeShaders,
        version: 1,
        flags: 0,
    },
    FeatureSupport {
        feature: Feature::DisplayInfo,
        version: 1,
        flags: 0,
    },
    FeatureSupport {
        feature: Feature::PixelFilter,
        version: 1,
        flags: 0,
    },
    FeatureSupport {
        feature: Feature::RefreshCycleOnlyChangeRefreshRate,
        version: 1,
        flags: 0,
    },
    FeatureSupport {
        feature: Feature::MuraCorrection,
        version: 1,
        flags: 0,
    },
    FeatureSupport {
        feature: Feature::Look,
        version: 1,
        flags: 0,
    },
    FeatureSupport {
        feature: Feature::PerfQuery,
        version: 1,
        flags: 0,
    },
    FeatureSupport {
        feature: Feature::Done,
        version: 0,
        flags: 0,
    },
];

pub const TARGET_REFRESH_INTERNAL_DISPLAY: u32 = 0x1;
pub const TARGET_REFRESH_ALLOW_SWITCHING: u32 = 0x2;
pub const TARGET_REFRESH_ONLY_CHANGE_REFRESH_RATE: u32 = 0x4;

/// Which physical display group an operation targets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScreenType {
    Internal,
    External,
}

/// Arguments passed to `steamcompmgr_set_app_refresh_cycle_override`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RefreshCycleOverride {
    pub screen: ScreenType,
    pub frames_per_second: u32,
    pub allow_refresh_switching: bool,
    pub apply_frame_limiter: bool,
}

/// Decode the control request exactly as the C++ handler does. External is the
/// default unless the internal bit is set.
#[must_use]
pub const fn decode_refresh_cycle_override(fps: u32, flags: u32) -> RefreshCycleOverride {
    RefreshCycleOverride {
        screen: if flags & TARGET_REFRESH_INTERNAL_DISPLAY != 0 {
            ScreenType::Internal
        } else {
            ScreenType::External
        },
        frames_per_second: fps,
        allow_refresh_switching: flags & TARGET_REFRESH_ALLOW_SWITCHING != 0,
        apply_frame_limiter: flags & TARGET_REFRESH_ONLY_CHANGE_REFRESH_RATE == 0,
    }
}

pub const DISPLAY_TYPE_INTERNAL: u32 = 0x1;
pub const DISPLAY_TYPE_EXTERNAL: u32 = 0x2;
pub const DISPLAY_SLEEP: u32 = 0x1;
pub const DISPLAY_WAKE: u32 = 0x2;

/// One backend screen power operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayPowerOperation {
    pub screen: ScreenType,
    pub sleep: bool,
}

/// Decode `display_sleep`. When both conflicting bits are supplied, current
/// Gamescope gives `sleep` precedence.
#[must_use]
pub fn decode_display_sleep(display_types: u32, flags: u32) -> Vec<DisplayPowerOperation> {
    if flags & (DISPLAY_SLEEP | DISPLAY_WAKE) == 0 {
        return Vec::new();
    }

    let sleep = flags & DISPLAY_SLEEP != 0;
    let mut operations = Vec::with_capacity(2);
    if display_types & DISPLAY_TYPE_EXTERNAL != 0 {
        operations.push(DisplayPowerOperation {
            screen: ScreenType::External,
            sleep,
        });
    }
    if display_types & DISPLAY_TYPE_INTERNAL != 0 {
        operations.push(DisplayPowerOperation {
            screen: ScreenType::Internal,
            sleep,
        });
    }
    operations
}

/// A response to `request_app_performance_stats`. Word order matches the XML:
/// low word followed by high word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PerformanceStatsEvent {
    pub control_id: u64,
    pub app_id: u32,
    pub frametime_ns_low: u32,
    pub frametime_ns_high: u32,
}

/// One-shot request fan-out from `wlserver.app_perf_requests`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PerformanceRequests {
    requests: HashMap<u32, Vec<u64>>,
}

impl PerformanceRequests {
    /// Register one control resource. Duplicate requests are retained, matching
    /// the C++ vector.
    pub fn request(&mut self, control_id: u64, app_id: u32) {
        self.requests.entry(app_id).or_default().push(control_id);
    }

    /// Remove a destroyed control from every outstanding app request.
    pub fn remove_control(&mut self, control_id: u64) {
        for controls in self.requests.values_mut() {
            controls.retain(|candidate| *candidate != control_id);
        }
    }

    /// Send to all requesters and consume the app's request list.
    pub fn app_presented(&mut self, app_id: u32, frametime_ns: u64) -> Vec<PerformanceStatsEvent> {
        let Some(controls) = self.requests.remove(&app_id) else {
            return Vec::new();
        };
        let (high, low) = split_u64(frametime_ns);

        controls
            .into_iter()
            .map(|control_id| PerformanceStatsEvent {
                control_id,
                app_id,
                frametime_ns_low: low,
                frametime_ns_high: high,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DISPLAY_SLEEP, DISPLAY_TYPE_EXTERNAL, DISPLAY_TYPE_INTERNAL, DISPLAY_WAKE,
        FEATURE_ADVERTISEMENT, Feature, PerformanceRequests, ScreenType,
        TARGET_REFRESH_ALLOW_SWITCHING, TARGET_REFRESH_INTERNAL_DISPLAY,
        TARGET_REFRESH_ONLY_CHANGE_REFRESH_RATE, decode_display_sleep,
        decode_refresh_cycle_override,
    };

    #[test]
    fn feature_list_is_terminated_by_done() {
        assert_eq!(FEATURE_ADVERTISEMENT.len(), 8);
        assert_eq!(FEATURE_ADVERTISEMENT.last().unwrap().feature, Feature::Done);
        assert_eq!(FEATURE_ADVERTISEMENT.last().unwrap().version, 0);
    }

    #[test]
    fn refresh_flags_map_to_existing_handler_arguments() {
        let decoded = decode_refresh_cycle_override(
            40,
            TARGET_REFRESH_INTERNAL_DISPLAY
                | TARGET_REFRESH_ALLOW_SWITCHING
                | TARGET_REFRESH_ONLY_CHANGE_REFRESH_RATE,
        );
        assert_eq!(decoded.screen, ScreenType::Internal);
        assert!(decoded.allow_refresh_switching);
        assert!(!decoded.apply_frame_limiter);

        assert_eq!(
            decode_refresh_cycle_override(0, 0).screen,
            ScreenType::External
        );
    }

    #[test]
    fn sleep_precedes_wake_when_both_conflicting_flags_are_set() {
        let operations = decode_display_sleep(
            DISPLAY_TYPE_INTERNAL | DISPLAY_TYPE_EXTERNAL,
            DISPLAY_SLEEP | DISPLAY_WAKE,
        );
        assert_eq!(operations.len(), 2);
        assert_eq!(operations[0].screen, ScreenType::External);
        assert!(operations.iter().all(|operation| operation.sleep));
    }

    #[test]
    fn performance_requests_fan_out_once() {
        let mut requests = PerformanceRequests::default();
        requests.request(10, 769);
        requests.request(11, 769);
        requests.request(11, 769);

        let events = requests.app_presented(769, 0x0123_4567_89ab_cdef);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].frametime_ns_low, 0x89ab_cdef);
        assert_eq!(events[0].frametime_ns_high, 0x0123_4567);
        assert!(requests.app_presented(769, 1).is_empty());
    }

    #[test]
    fn destroyed_controls_are_removed_from_pending_queries() {
        let mut requests = PerformanceRequests::default();
        requests.request(10, 1);
        requests.request(11, 1);
        requests.remove_control(10);
        let events = requests.app_presented(1, 2);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].control_id, 11);
    }
}
