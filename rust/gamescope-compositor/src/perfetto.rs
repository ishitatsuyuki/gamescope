//! Low-overhead Perfetto instrumentation shared by the compositor threads.

use std::time::Duration;

use perfetto_sdk::{
    producer::{Backends, Producer, ProducerInitArgsBuilder},
    track_event::{
        EventContext, TrackEvent, TrackEventFlow, TrackEventProtoField, TrackEventProtoFields,
    },
};

/// Field number of `gamescope.protos.GamescopeExtension.gamescope_event`.
const GAMESCOPE_EVENT_FIELD_NUMBER: u32 = 3200;

/// Field numbers in `gamescope.protos.GamescopeEvent`.
///
/// Keep this in sync with `protos/gamescope_event.proto`. Values are exposed
/// through this enum so instrumentation sites cannot mix up field numbers.
#[derive(Clone, Copy, Debug)]
#[repr(u32)]
enum GamescopeEventFieldNumber {
    FrameId = 1,
    ReplacedFrameId = 2,
    NewFrameId = 3,
    LayerCount = 4,
    HasCursor = 5,
    DirectScanout = 6,
    Sequence = 7,
    ElapsedNs = 8,
    QueuedEvents = 9,
    TimerLateNs = 10,
    WinitQueueDepth = 11,
    QueueDelayNs = 12,
    ReadyToRenderNs = 13,
    RequestedTimeoutNs = 14,
    HardwareEventQueueDepth = 15,
    CallbackQueueDelayNs = 16,
    WorkerToFrontendNs = 17,
    RepaintLateNs = 18,
    Keycode = 19,
    Pressed = 20,
    EventTimeUs = 21,
    SurfaceAlive = 22,
    Window = 23,
    SurfaceAssociated = 24,
    Keysym = 25,
    Intercepted = 26,
    PressedKeys = 27,
    ServerId = 28,
    EventCount = 29,
    BridgeCount = 30,
    MailboxWakeDelayNs = 31,
}

/// A typed field accepted by Gamescope's TrackEvent extension.
#[derive(Clone, Copy, Debug)]
pub enum EventField {
    FrameId(u64),
    ReplacedFrameId(u64),
    NewFrameId(u64),
    LayerCount(u64),
    HasCursor(bool),
    DirectScanout(bool),
    Sequence(u64),
    ElapsedNs(u64),
    QueuedEvents(u64),
    TimerLateNs(u64),
    WinitQueueDepth(u64),
    QueueDelayNs(u64),
    ReadyToRenderNs(u64),
    RequestedTimeoutNs(u64),
    HardwareEventQueueDepth(u64),
    CallbackQueueDelayNs(u64),
    WorkerToFrontendNs(u64),
    RepaintLateNs(u64),
    Keycode(u64),
    Pressed(bool),
    EventTimeUs(u64),
    SurfaceAlive(bool),
    Window(u64),
    SurfaceAssociated(bool),
    Keysym(u64),
    Intercepted(bool),
    PressedKeys(u64),
    ServerId(u64),
    EventCount(u64),
    BridgeCount(u64),
    MailboxWakeDelayNs(u64),
}

impl EventField {
    fn encode(self) -> TrackEventProtoField<'static> {
        use EventField::*;
        use GamescopeEventFieldNumber as Number;

        let (number, value) = match self {
            FrameId(value) => (Number::FrameId, value),
            ReplacedFrameId(value) => (Number::ReplacedFrameId, value),
            NewFrameId(value) => (Number::NewFrameId, value),
            LayerCount(value) => (Number::LayerCount, value),
            HasCursor(value) => (Number::HasCursor, u64::from(value)),
            DirectScanout(value) => (Number::DirectScanout, u64::from(value)),
            Sequence(value) => (Number::Sequence, value),
            ElapsedNs(value) => (Number::ElapsedNs, value),
            QueuedEvents(value) => (Number::QueuedEvents, value),
            TimerLateNs(value) => (Number::TimerLateNs, value),
            WinitQueueDepth(value) => (Number::WinitQueueDepth, value),
            QueueDelayNs(value) => (Number::QueueDelayNs, value),
            ReadyToRenderNs(value) => (Number::ReadyToRenderNs, value),
            RequestedTimeoutNs(value) => (Number::RequestedTimeoutNs, value),
            HardwareEventQueueDepth(value) => (Number::HardwareEventQueueDepth, value),
            CallbackQueueDelayNs(value) => (Number::CallbackQueueDelayNs, value),
            WorkerToFrontendNs(value) => (Number::WorkerToFrontendNs, value),
            RepaintLateNs(value) => (Number::RepaintLateNs, value),
            Keycode(value) => (Number::Keycode, value),
            Pressed(value) => (Number::Pressed, u64::from(value)),
            EventTimeUs(value) => (Number::EventTimeUs, value),
            SurfaceAlive(value) => (Number::SurfaceAlive, u64::from(value)),
            Window(value) => (Number::Window, value),
            SurfaceAssociated(value) => (Number::SurfaceAssociated, u64::from(value)),
            Keysym(value) => (Number::Keysym, value),
            Intercepted(value) => (Number::Intercepted, u64::from(value)),
            PressedKeys(value) => (Number::PressedKeys, value),
            ServerId(value) => (Number::ServerId, value),
            EventCount(value) => (Number::EventCount, value),
            BridgeCount(value) => (Number::BridgeCount, value),
            MailboxWakeDelayNs(value) => (Number::MailboxWakeDelayNs, value),
        };
        TrackEventProtoField::VarInt(number as u32, value)
    }
}

/// Attach strongly-typed Gamescope data to a TrackEvent.
pub fn add_event_fields(ctx: &mut EventContext, fields: &[EventField]) {
    let encoded = fields
        .iter()
        .copied()
        .map(EventField::encode)
        .collect::<Vec<_>>();
    let extension = [TrackEventProtoField::Nested(
        GAMESCOPE_EVENT_FIELD_NUMBER,
        &encoded,
    )];
    ctx.set_proto_fields(&TrackEventProtoFields { fields: &extension });
}

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
