//! Low-overhead Perfetto instrumentation shared by the compositor threads.

use std::time::Duration;

use perfetto_sdk::{
    producer::{Backends, Producer, ProducerInitArgsBuilder},
    track_event::{TrackEvent, TrackEventFlow},
};

perfetto_sdk::track_event_categories! {
    pub mod perfetto_te_ns {
        (
            "gamescope.event_loop",
            "Gamescope event-loop sleeps, wakeups, and callback batches",
            []
        ),
        (
            "gamescope.input",
            "Gamescope input delivery",
            []
        ),
        (
            "gamescope.frame",
            "Gamescope frame scheduling, rendering, and presentation",
            []
        ),
        (
            "gamescope.xwm",
            "Gamescope Steam/X11 worker queues and processing",
            []
        ),
    }
}

/// Register Gamescope's track-event categories with the system Perfetto service.
pub fn init() {
    let args = ProducerInitArgsBuilder::new().backends(Backends::SYSTEM);
    Producer::init(args.build());
    TrackEvent::init();
    let _ = perfetto_te_ns::register();
}

#[must_use]
pub fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// A distinct process-scoped flow for each hand-off in a frame's lifetime.
#[must_use]
pub fn frame_flow(frame_id: u64, handoff: u64) -> TrackEventFlow {
    TrackEventFlow::process_scoped_flow(frame_id.wrapping_mul(4).wrapping_add(handoff))
}
