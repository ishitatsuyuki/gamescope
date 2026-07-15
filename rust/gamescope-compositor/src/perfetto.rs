//! Low-overhead Perfetto instrumentation shared by the compositor threads.

use std::time::Duration;

use perfetto_sdk::track_event::{TrackEvent, TrackEventFlow};

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
    init_system_producer();
    TrackEvent::init();
    let _ = perfetto_te_ns::register();
}

/// Initialize only the system producer using the stable C ABI present in the
/// checkout's generated Perfetto library.
///
/// The higher-level Rust builder currently also calls the newer machine-id ABI,
/// while the checked-in C amalgamation predates that optional setter. The
/// system service derives machine identity itself, so the older initialization
/// sequence has identical semantics for this process.
#[allow(unsafe_code)]
fn init_system_producer() {
    // SAFETY: `args` is created by Perfetto, used only with Perfetto producer
    // initialization functions, and destroyed exactly once after the system
    // initializer has copied its contents. A zero shared-memory hint requests
    // Perfetto's default sizing policy.
    unsafe {
        let args = perfetto_sdk_sys::PerfettoProducerBackendInitArgsCreate();
        perfetto_sdk_sys::PerfettoProducerBackendInitArgsSetShmemSizeHintKb(args, 0);
        perfetto_sdk_sys::PerfettoProducerSystemInit(args);
        perfetto_sdk_sys::PerfettoProducerBackendInitArgsDestroy(args);
    }
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
