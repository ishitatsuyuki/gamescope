use std::{os::unix::net::UnixStream, sync::Arc};

use gamescope_core::control::ScreenType;
use gamescope_protocols::{
    action_binding::client::{
        gamescope_action_binding::{ArmFlag, Event as ActionBindingEvent, GamescopeActionBinding},
        gamescope_action_binding_manager::{
            Event as ActionBindingManagerEvent, GamescopeActionBindingManager,
        },
    },
    control::client::gamescope_control::{
        Event as ControlEvent, GamescopeControl, TargetRefreshCycleFlag,
    },
    pipewire::client::gamescope_pipewire::{Event as PipewireEvent, GamescopePipewire},
    private::client::gamescope_private::{Event as PrivateEvent, GamescopePrivate},
    reshade::client::gamescope_reshade::{Event as ReshadeEvent, GamescopeReshade},
};
use gamescope_wayland_server::{ActiveDisplayInfo, Command, GamescopeState, ServerConfig};
use wayland_client::{
    Connection, Dispatch as ClientDispatch, EventQueue, QueueHandle,
    protocol::wl_registry::{self, WlRegistry},
};
use wayland_server::Display;

#[derive(Debug, Default)]
struct ClientState {
    action_binding_manager: Option<GamescopeActionBindingManager>,
    action_events: Vec<(u32, u64, u32)>,
    control: Option<GamescopeControl>,
    pipewire: Option<GamescopePipewire>,
    private: Option<GamescopePrivate>,
    reshade: Option<GamescopeReshade>,
    features: Vec<(u32, u32, u32)>,
    active_display: Option<(String, String, String, u32, Vec<u32>)>,
    pipewire_node_id: Option<u32>,
    performance_stats: Vec<(u32, u64)>,
    private_completions: usize,
    logs: Vec<String>,
    ready_effects: Vec<String>,
}

impl ClientDispatch<WlRegistry, ()> for ClientState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _connection: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };

        match interface.as_str() {
            "gamescope_action_binding_manager" => {
                state.action_binding_manager = Some(registry.bind(name, version.min(1), queue, ()));
            }
            "gamescope_control" => {
                state.control = Some(registry.bind(name, version.min(6), queue, ()));
            }
            "gamescope_pipewire" => {
                state.pipewire = Some(registry.bind(name, version.min(1), queue, ()));
            }
            "gamescope_private" => {
                state.private = Some(registry.bind(name, version.min(1), queue, ()));
            }
            "gamescope_reshade" => {
                state.reshade = Some(registry.bind(name, version.min(1), queue, ()));
            }
            _ => {}
        }
    }
}

impl ClientDispatch<GamescopeActionBindingManager, ()> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &GamescopeActionBindingManager,
        _event: ActionBindingManagerEvent,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl ClientDispatch<GamescopeActionBinding, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &GamescopeActionBinding,
        event: ActionBindingEvent,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            ActionBindingEvent::Triggered {
                sequence,
                time_lo,
                time_hi,
                trigger_flags,
            } => state.action_events.push((
                sequence,
                (u64::from(time_hi) << 32) | u64::from(time_lo),
                trigger_flags.into(),
            )),
            _ => unreachable!("unknown gamescope_action_binding event"),
        }
    }
}

impl ClientDispatch<GamescopeControl, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &GamescopeControl,
        event: ControlEvent,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            ControlEvent::FeatureSupport {
                feature,
                version,
                flags,
            } => state.features.push((feature, version, flags)),
            ControlEvent::ActiveDisplayInfo {
                connector_name,
                display_make,
                display_model,
                display_flags,
                valid_refresh_rates,
            } => {
                let rates = valid_refresh_rates
                    .chunks_exact(size_of::<u32>())
                    .map(|bytes| u32::from_ne_bytes(bytes.try_into().expect("four-byte chunk")))
                    .collect();
                state.active_display = Some((
                    connector_name,
                    display_make,
                    display_model,
                    display_flags.into(),
                    rates,
                ));
            }
            ControlEvent::AppPerformanceStats {
                app_id,
                frametime_ns_lo,
                frametime_ns_hi,
            } => state.performance_stats.push((
                app_id,
                (u64::from(frametime_ns_hi) << 32) | u64::from(frametime_ns_lo),
            )),
            ControlEvent::ScreenshotTaken { .. } => {}
            _ => unreachable!("unknown gamescope_control event"),
        }
    }
}

impl ClientDispatch<GamescopePipewire, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &GamescopePipewire,
        event: PipewireEvent,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            PipewireEvent::StreamNode { node_id } => state.pipewire_node_id = Some(node_id),
            _ => unreachable!("unknown gamescope_pipewire event"),
        }
    }
}

impl ClientDispatch<GamescopePrivate, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &GamescopePrivate,
        event: PrivateEvent,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            PrivateEvent::Log { text } => state.logs.push(text),
            PrivateEvent::CommandExecuted => state.private_completions += 1,
            _ => unreachable!("unknown gamescope_private event"),
        }
    }
}

impl ClientDispatch<GamescopeReshade, ()> for ClientState {
    fn event(
        state: &mut Self,
        _proxy: &GamescopeReshade,
        event: ReshadeEvent,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            ReshadeEvent::EffectReady { effect_path } => state.ready_effects.push(effect_path),
            _ => unreachable!("unknown gamescope_reshade event"),
        }
    }
}

fn client_to_server(
    queue: &EventQueue<ClientState>,
    display: &mut Display<GamescopeState>,
    state: &mut GamescopeState,
) {
    queue.flush().unwrap();
    display.dispatch_clients(state).unwrap();
    display.flush_clients().unwrap();
}

fn server_to_client(queue: &mut EventQueue<ClientState>, state: &mut ClientState) {
    queue.prepare_read().unwrap().read().unwrap();
    queue.dispatch_pending(state).unwrap();
}

#[test]
fn gamescope_globals_round_trip_requests_and_events() {
    let mut display = Display::<GamescopeState>::new().unwrap();
    let mut server_state = GamescopeState::default();
    let config = ServerConfig {
        pipewire_node_id: Some(57),
        active_display: Some(ActiveDisplayInfo {
            connector_name: "eDP-1".into(),
            display_make: "Valve".into(),
            display_model: "Steam Deck".into(),
            flags: 0x1 | 0x2 | 0x4,
            valid_refresh_rates_hz: vec![40, 60, 90],
        }),
    };
    let _globals = GamescopeState::register_globals(&display.handle(), &config);

    let (client_socket, server_socket) = UnixStream::pair().unwrap();
    display
        .handle()
        .insert_client(server_socket, Arc::new(()))
        .unwrap();
    let connection = Connection::from_socket(client_socket).unwrap();
    let mut queue = connection.new_event_queue::<ClientState>();
    let queue_handle = queue.handle();
    let _registry = connection.display().get_registry(&queue_handle, ());
    let mut client_state = ClientState::default();

    // Request the registry, read globals, and queue binds.
    client_to_server(&queue, &mut display, &mut server_state);
    server_to_client(&mut queue, &mut client_state);

    // Process binds and their initial feature/display/PipeWire events.
    client_to_server(&queue, &mut display, &mut server_state);
    server_to_client(&mut queue, &mut client_state);

    assert_eq!(client_state.features.len(), 8);
    assert_eq!(client_state.features.last(), Some(&(0, 0, 0)));
    assert_eq!(client_state.pipewire_node_id, Some(57));
    assert_eq!(
        client_state.active_display,
        Some((
            "eDP-1".into(),
            "Valve".into(),
            "Steam Deck".into(),
            0x7,
            vec![40, 60, 90],
        ))
    );

    server_state.set_active_display(Some(ActiveDisplayInfo {
        connector_name: "DP-1".into(),
        display_make: "ACME".into(),
        display_model: "External".into(),
        flags: 0x4,
        valid_refresh_rates_hz: vec![60, 120],
    }));
    display.flush_clients().unwrap();
    server_to_client(&mut queue, &mut client_state);
    assert_eq!(
        client_state.active_display,
        Some((
            "DP-1".into(),
            "ACME".into(),
            "External".into(),
            0x4,
            vec![60, 120],
        ))
    );

    // Create an action binding and exercise its real wire event.
    let action_binding = client_state
        .action_binding_manager
        .as_ref()
        .unwrap()
        .create_action_binding(&queue_handle, ());
    action_binding.set_description("test binding".into());
    let trigger = [0xffe3_u32, u32::from(b'a')]
        .into_iter()
        .flat_map(u32::to_ne_bytes)
        .collect();
    action_binding.add_keyboard_trigger(trigger);
    action_binding.arm(ArmFlag::OneShot);
    client_to_server(&queue, &mut display, &mut server_state);

    assert!(
        !server_state.process_pressed_keysyms([0xffe3, u32::from(b'A')], 0x0123_4567_89ab_cdef,)
    );
    display.flush_clients().unwrap();
    server_to_client(&mut queue, &mut client_state);
    assert_eq!(
        client_state.action_events,
        [(0, 0x0123_4567_89ab_cdef, 0x1)]
    );
    assert!(!server_state.process_pressed_keysyms([0xffe3, u32::from(b'A')], 1,));

    let control = client_state.control.as_ref().unwrap();
    control.set_app_target_refresh_cycle(
        40,
        TargetRefreshCycleFlag::InternalDisplay
            | TargetRefreshCycleFlag::AllowRefreshSwitching
            | TargetRefreshCycleFlag::OnlyChangeRefreshRate,
    );
    control.request_app_performance_stats(769);
    client_state
        .private
        .as_ref()
        .unwrap()
        .execute("show_fps".into(), "1".into());
    client_state
        .reshade
        .as_ref()
        .unwrap()
        .set_effect("/tmp/test.fx".into());

    client_to_server(&queue, &mut display, &mut server_state);
    let commands: Vec<_> = server_state.drain_commands().collect();
    assert_eq!(commands.len(), 3);
    match &commands[0] {
        Command::SetRefreshCycle(refresh) => {
            assert_eq!(refresh.screen, ScreenType::Internal);
            assert_eq!(refresh.frames_per_second, 40);
            assert!(refresh.allow_refresh_switching);
            assert!(!refresh.apply_frame_limiter);
        }
        command => panic!("unexpected command: {command:?}"),
    }
    let private_reply = match &commands[1] {
        Command::ExecutePrivate {
            reply,
            command,
            value,
        } => {
            assert_eq!(command, "show_fps");
            assert_eq!(value, "1");
            reply
        }
        command => panic!("unexpected command: {command:?}"),
    };
    let reshade_reply = match &commands[2] {
        Command::SetReshadeEffect { reply, path } => {
            assert_eq!(path, "/tmp/test.fx");
            reply
        }
        command => panic!("unexpected command: {command:?}"),
    };

    GamescopeState::private_command_executed(private_reply);
    GamescopeState::reshade_effect_ready(reshade_reply, "/tmp/test.fx");
    server_state.broadcast_log("hello from gamescope");
    server_state.app_presented(769, 0x0123_4567_89ab_cdef);
    display.flush_clients().unwrap();
    server_to_client(&mut queue, &mut client_state);

    assert_eq!(client_state.private_completions, 1);
    assert_eq!(client_state.logs, ["hello from gamescope"]);
    assert_eq!(client_state.ready_effects, ["/tmp/test.fx"]);
    assert_eq!(
        client_state.performance_stats,
        [(769, 0x0123_4567_89ab_cdef)]
    );
}
