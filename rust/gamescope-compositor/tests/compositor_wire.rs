use std::{os::unix::net::UnixStream, sync::Arc, time::Duration};

use gamescope_compositor::{ClientState as ServerClientState, OutputConfig, State};
use gamescope_protocols::input_method::client::{
    gamescope_input_method::{Action, Event as InputMethodEvent, GamescopeInputMethod},
    gamescope_input_method_manager::{
        Event as InputMethodManagerEvent, GamescopeInputMethodManager,
    },
};
use gamescope_protocols::swapchain::client::{
    gamescope_swapchain::{Event as SwapchainEvent, GamescopeSwapchain},
    gamescope_swapchain_factory_v2::{Event as FactoryEvent, GamescopeSwapchainFactoryV2},
};
use wayland_client::{
    Connection, Dispatch as ClientDispatch, EventQueue, QueueHandle,
    protocol::{
        wl_compositor::{self, WlCompositor},
        wl_registry::{self, WlRegistry},
        wl_seat::{self, WlSeat},
        wl_surface::{self, WlSurface},
    },
};
use wayland_server::Display;

#[derive(Debug, Default)]
struct TestClient {
    compositor: Option<WlCompositor>,
    swapchain_factory: Option<GamescopeSwapchainFactoryV2>,
    seat: Option<WlSeat>,
    input_method_manager: Option<GamescopeInputMethodManager>,
    input_method_serial: Option<u32>,
    refresh_cycles: Vec<u64>,
    presentation_ids: Vec<u32>,
}

impl ClientDispatch<WlRegistry, ()> for TestClient {
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
            "wl_compositor" => {
                state.compositor = Some(registry.bind(name, version.min(5), queue, ()));
            }
            "gamescope_swapchain_factory_v2" => {
                state.swapchain_factory = Some(registry.bind(name, version.min(1), queue, ()));
            }
            "wl_seat" => {
                state.seat = Some(registry.bind(name, version.min(9), queue, ()));
            }
            "gamescope_input_method_manager" => {
                state.input_method_manager = Some(registry.bind(name, version.min(3), queue, ()));
            }
            _ => {}
        }
    }
}

impl ClientDispatch<WlSeat, ()> for TestClient {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl ClientDispatch<GamescopeInputMethodManager, ()> for TestClient {
    fn event(
        _state: &mut Self,
        _proxy: &GamescopeInputMethodManager,
        event: InputMethodManagerEvent,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        let _ = event;
        unreachable!("input method manager has no events");
    }
}

impl ClientDispatch<GamescopeInputMethod, ()> for TestClient {
    fn event(
        state: &mut Self,
        _proxy: &GamescopeInputMethod,
        event: InputMethodEvent,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            InputMethodEvent::Done { serial } => state.input_method_serial = Some(serial),
            InputMethodEvent::Unavailable => {}
            _ => unreachable!("unknown input method event"),
        }
    }
}

impl ClientDispatch<WlCompositor, ()> for TestClient {
    fn event(
        _state: &mut Self,
        _proxy: &WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl ClientDispatch<WlSurface, ()> for TestClient {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
    }
}

impl ClientDispatch<GamescopeSwapchainFactoryV2, ()> for TestClient {
    fn event(
        _state: &mut Self,
        _proxy: &GamescopeSwapchainFactoryV2,
        event: FactoryEvent,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        let _ = event;
        unreachable!("factory has no events");
    }
}

impl ClientDispatch<GamescopeSwapchain, ()> for TestClient {
    fn event(
        state: &mut Self,
        _proxy: &GamescopeSwapchain,
        event: SwapchainEvent,
        _data: &(),
        _connection: &Connection,
        _queue: &QueueHandle<Self>,
    ) {
        match event {
            SwapchainEvent::RefreshCycle {
                refresh_cycle_hi,
                refresh_cycle_lo,
            } => state
                .refresh_cycles
                .push((u64::from(refresh_cycle_hi) << 32) | u64::from(refresh_cycle_lo)),
            SwapchainEvent::PastPresentTiming { present_id, .. } => {
                state.presentation_ids.push(present_id);
            }
            SwapchainEvent::Retired => {}
            _ => unreachable!("unknown swapchain event"),
        }
    }
}

fn client_to_server(
    queue: &EventQueue<TestClient>,
    display: &mut Display<State>,
    state: &mut State,
) {
    queue.flush().unwrap();
    display.dispatch_clients(state).unwrap();
    display.flush_clients().unwrap();
}

fn server_to_client(queue: &mut EventQueue<TestClient>, state: &mut TestClient) {
    queue.prepare_read().unwrap().read().unwrap();
    queue.dispatch_pending(state).unwrap();
}

#[test]
fn core_surface_commit_round_trips_vulkan_swapchain_timing() {
    let mut display = Display::<State>::new().unwrap();
    let mut server_state = State::new(&display.handle(), &OutputConfig::default());
    let (client_socket, server_socket) = UnixStream::pair().unwrap();
    display
        .handle()
        .insert_client(server_socket, Arc::new(ServerClientState::default()))
        .unwrap();

    let connection = Connection::from_socket(client_socket).unwrap();
    let mut queue = connection.new_event_queue::<TestClient>();
    let queue_handle = queue.handle();
    let _registry = connection.display().get_registry(&queue_handle, ());
    let mut client_state = TestClient::default();

    client_to_server(&queue, &mut display, &mut server_state);
    server_to_client(&mut queue, &mut client_state);

    let surface = client_state
        .compositor
        .as_ref()
        .unwrap()
        .create_surface(&queue_handle, ());
    let swapchain = client_state
        .swapchain_factory
        .as_ref()
        .unwrap()
        .create_swapchain(&surface, &queue_handle, ());
    swapchain.swapchain_feedback(3, 44, 0, 1, 1, 1, "integration-test".into());
    swapchain.set_present_mode(2);
    swapchain.set_present_time(77, 0, 1_000_000);
    surface.commit();
    client_to_server(&queue, &mut display, &mut server_state);

    server_state.presented(Duration::from_millis(20));
    display.flush_clients().unwrap();
    server_to_client(&mut queue, &mut client_state);

    assert_eq!(client_state.presentation_ids, [77]);
    assert_eq!(client_state.refresh_cycles, [16_666_666]);
}

#[test]
fn private_input_method_preserves_serial_and_wheel_semantics() {
    let mut display = Display::<State>::new().unwrap();
    let mut server_state = State::new(&display.handle(), &OutputConfig::default());
    let (client_socket, server_socket) = UnixStream::pair().unwrap();
    display
        .handle()
        .insert_client(server_socket, Arc::new(ServerClientState::default()))
        .unwrap();

    let connection = Connection::from_socket(client_socket).unwrap();
    let mut queue = connection.new_event_queue::<TestClient>();
    let queue_handle = queue.handle();
    let _registry = connection.display().get_registry(&queue_handle, ());
    let mut client_state = TestClient::default();
    client_to_server(&queue, &mut display, &mut server_state);
    server_to_client(&mut queue, &mut client_state);

    let input_method = client_state
        .input_method_manager
        .as_ref()
        .unwrap()
        .create_input_method(client_state.seat.as_ref().unwrap(), &queue_handle, ());
    client_to_server(&queue, &mut display, &mut server_state);
    server_to_client(&mut queue, &mut client_state);
    assert_eq!(client_state.input_method_serial, Some(1));

    input_method.set_string("A".into());
    input_method.set_action(Action::Submit);
    input_method.commit(0);
    input_method.pointer_wheel(-120, 240);
    input_method.commit(1);
    client_to_server(&queue, &mut display, &mut server_state);

    let commands: Vec<_> = server_state.gamescope_state.drain_commands().collect();
    assert_eq!(commands.len(), 2, "stale serial must not emit a commit");
    assert!(matches!(
        &commands[0],
        gamescope_wayland_server::Command::InputMethod(
            gamescope_wayland_server::InputMethodCommand::PointerWheel {
                horizontal,
                vertical,
                time_msec: 1,
            }
        ) if (*horizontal + 1.0).abs() < f64::EPSILON
            && (*vertical - 2.0).abs() < f64::EPSILON
    ));
    assert!(matches!(
        &commands[1],
        gamescope_wayland_server::Command::InputMethod(
            gamescope_wayland_server::InputMethodCommand::Commit(commit)
        ) if commit.text.as_deref() == Some("A")
            && commit.action == gamescope_core::input_method::InputMethodAction::Submit
    ));
}
