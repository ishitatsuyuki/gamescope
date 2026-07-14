use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    error::Error,
    fs::OpenOptions,
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use ::winit::platform::pump_events::PumpStatus;
use gamescope_compositor::{
    ClientState, LayerRenderElement, OutputConfig, State, SteamRuntimeRequest,
    drm::{
        DirectScanoutStatus, HardwareBackend, HardwareConfig, HardwareEvent, HardwareFrame,
        HardwareOutputInfo,
    },
    steam::{COMMON_COMPAT_ENV, STEAM_COMPAT_ENV},
};
use gamescope_core::control::{RefreshCycleOverride, ScreenType};
use gamescope_wayland_server::{ActiveDisplayInfo, Command as GamescopeCommand, ServerConfig};
use smithay::{
    backend::{
        drm::DrmNode,
        egl::EGLDevice,
        input::{
            AbsolutePositionEvent, Axis, Event, GestureBeginEvent, GestureEndEvent,
            GesturePinchUpdateEvent, GestureSwipeUpdateEvent, InputEvent, KeyboardKeyEvent,
            PointerAxisEvent, PointerButtonEvent, PointerMotionEvent, TouchEvent,
        },
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            Color32F, Frame, ImportDma, ImportMemWl, Renderer,
            element::{
                Kind,
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
                utils::RescaleRenderElement,
            },
            gles::GlesRenderer,
            utils::{draw_render_elements, on_commit_buffer_handler},
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{UdevBackend, UdevEvent, primary_gpu},
        winit::{self as winit_backend, WinitEvent},
    },
    input::keyboard::FilterResult,
    reexports::wayland_server::{Display, DisplayHandle, ListeningSocket},
    reexports::{
        calloop::{EventLoop, LoopHandle, RegistrationToken, channel::Event as ChannelEvent},
        input::Libinput,
        rustix::fs::OFlags,
    },
    utils::{Clock, Monotonic, Rectangle, SERIAL_COUNTER, Serial, Transform},
    wayland::{
        dmabuf::{DmabufFeedback, DmabufFeedbackBuilder},
        drm_syncobj::supports_syncobj_eventfd,
    },
    xwayland::{X11Wm, XWayland, XWaylandEvent},
};
use tracing::{debug, info, warn};
use wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1;
use wayland_server::backend::ClientData;

#[derive(Debug)]
struct Options {
    backend: BackendChoice,
    output: OutputConfig,
    window_width: Option<u32>,
    window_height: Option<u32>,
    socket: Option<String>,
    command: Vec<String>,
    xwayland_count: usize,
    steam: bool,
    expose_wayland: bool,
    borderless: bool,
    fullscreen: bool,
    keep_alive: bool,
    drm_device: Option<PathBuf>,
    output_refresh_millihz: Option<i32>,
    direct_scanout: bool,
    adaptive_sync: bool,
    connector_priorities: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackendChoice {
    Auto,
    Nested,
    Drm,
}

impl BackendChoice {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "auto" => Ok(Self::Auto),
            "nested" | "winit" => Ok(Self::Nested),
            "drm" | "kms" => Ok(Self::Drm),
            _ => Err(format!(
                "invalid backend {value:?}; expected auto, nested, or drm"
            )),
        }
    }
}

struct LimiterFile {
    path: PathBuf,
    remove_on_drop: bool,
}

impl LimiterFile {
    fn create() -> Result<Self, Box<dyn Error>> {
        if let Some(path) =
            std::env::var_os("GAMESCOPE_LIMITER_FILE").filter(|path| !path.is_empty())
        {
            let path = PathBuf::from(path);
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&path)?;
            return Ok(Self {
                path,
                remove_on_drop: false,
            });
        }

        let directory = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        for suffix in 0..32_u32 {
            let path = directory.join(format!("gamescope-limiter-{}-{suffix}", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => {
                    return Ok(Self {
                        path,
                        remove_on_drop: true,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err("unable to allocate a Gamescope limiter file".into())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LimiterFile {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

struct ManagedChild(Child);

impl ManagedChild {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.0.try_wait()
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            if let Ok(pid) = i32::try_from(self.0.id()) {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(-pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            } else {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }
}

struct SessionDevice {
    session: LibSeatSession,
    fd: Option<std::os::fd::OwnedFd>,
}

impl Drop for SessionDevice {
    fn drop(&mut self) {
        if let Some(fd) = self.fd.take() {
            let _ = self.session.close(fd);
        }
    }
}

impl Options {
    fn parse() -> Result<Self, String> {
        Self::parse_from(std::env::args().skip(1))
    }

    fn parse_from(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut options = Self {
            backend: BackendChoice::Auto,
            output: OutputConfig::unspecified(),
            window_width: None,
            window_height: None,
            socket: None,
            command: Vec::new(),
            xwayland_count: 1,
            steam: false,
            expose_wayland: false,
            borderless: false,
            fullscreen: false,
            keep_alive: false,
            drm_device: None,
            output_refresh_millihz: None,
            direct_scanout: true,
            adaptive_sync: false,
            connector_priorities: Vec::new(),
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--width" | "--nested-width" | "-w" => {
                    options.output.width = parse_value(&mut args, &arg)?;
                }
                "--height" | "--nested-height" | "-h" => {
                    options.output.height = parse_value(&mut args, &arg)?;
                }
                "--output-width" | "-W" => {
                    options.window_width = Some(parse_value(&mut args, &arg)?);
                }
                "--output-height" | "-H" => {
                    options.window_height = Some(parse_value(&mut args, &arg)?);
                }
                "--output-refresh" => {
                    let hz: f64 = parse_value(&mut args, &arg)?;
                    options.output_refresh_millihz = Some(hz_to_millihz(hz)?);
                }
                "--refresh" | "--nested-refresh" | "-r" => {
                    let hz: f64 = parse_value(&mut args, &arg)?;
                    options.output.refresh_millihz = hz_to_millihz(hz)?;
                }
                "--socket" => options.socket = Some(next_value(&mut args, &arg)?),
                "--no-xwayland" => options.xwayland_count = 0,
                "--xwayland-count" => {
                    options.xwayland_count = parse_value(&mut args, &arg)?;
                }
                "--borderless" | "-b" => options.borderless = true,
                "--fullscreen" | "-f" => options.fullscreen = true,
                "--backend" => {
                    options.backend = BackendChoice::parse(&next_value(&mut args, &arg)?)?;
                }
                "--nested" | "-n" => options.backend = BackendChoice::Nested,
                "--drm" => options.backend = BackendChoice::Drm,
                "--drm-device" => {
                    options.drm_device = Some(PathBuf::from(next_value(&mut args, &arg)?));
                    options.backend = BackendChoice::Drm;
                }
                "--disable-direct-scanout" => options.direct_scanout = false,
                "--adaptive-sync" => options.adaptive_sync = true,
                "--prefer-output" | "-O" => {
                    options.connector_priorities = next_value(&mut args, &arg)?
                        .split(',')
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned)
                        .collect();
                }
                "--keep-alive" => options.keep_alive = true,
                "--expose-wayland" => options.expose_wayland = true,
                "--steam" | "-e" => options.steam = true,
                "--" => {
                    options.command.extend(args);
                    break;
                }
                "--help" => {
                    println!(
                        "gamescope-rs [--backend auto|nested|drm] [-n|--nested] [--drm] [--drm-device CARD] [-O CONNECTORS] [-w WIDTH] [-h HEIGHT] [-W OUTPUT_WIDTH] [-H OUTPUT_HEIGHT] [-r HZ] [--output-refresh HZ] [--adaptive-sync] [--disable-direct-scanout] [-b] [-f] [-e|--steam] [--xwayland-count N] [--expose-wayland] [--socket NAME] [-- COMMAND ...]"
                    );
                    std::process::exit(0);
                }
                _ => return Err(format!("unknown option: {arg}")),
            }
        }
        if options.output.width < 0 || options.output.height < 0 {
            return Err("game dimensions cannot be negative".into());
        }
        if options.output.width != 0 && options.output.height == 0 {
            return Err("cannot specify -w without -h".into());
        }
        if options.window_width.is_some() && options.window_height.is_none() {
            return Err("cannot specify -W without -H".into());
        }
        if options.window_width == Some(0) || options.window_height == Some(0) {
            return Err("output dimensions must be positive".into());
        }
        if options.output.refresh_millihz <= 0 {
            return Err("refresh rate must be positive".into());
        }
        Ok(options)
    }

    fn selected_backend(&self) -> Result<BackendChoice, String> {
        if self.backend != BackendChoice::Auto {
            return Ok(self.backend);
        }
        if let Ok(value) = std::env::var("GAMESCOPE_BACKEND") {
            return BackendChoice::parse(&value);
        }
        if self.drm_device.is_some()
            || (std::env::var_os("WAYLAND_DISPLAY").is_none()
                && std::env::var_os("DISPLAY").is_none())
        {
            Ok(BackendChoice::Drm)
        } else {
            Ok(BackendChoice::Nested)
        }
    }
}

fn next_value(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("{option} requires a value"))
}

fn parse_value<T: std::str::FromStr>(
    args: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<T, String> {
    next_value(args, option)?
        .parse()
        .map_err(|_| format!("invalid value for {option}"))
}

fn hz_to_millihz(hz: f64) -> Result<i32, String> {
    let millihz = (hz * 1000.0).round();
    if !millihz.is_finite() || !(1.0..=f64::from(i32::MAX)).contains(&millihz) {
        return Err("refresh rate must be positive and finite".into());
    }
    Ok(millihz as i32)
}

fn spawn_xwayland(
    source_handle: &LoopHandle<'static, State>,
    display: &DisplayHandle,
    server_id: u32,
    create_identifier: Option<u32>,
) -> Result<RegistrationToken, Box<dyn Error>> {
    let (xwayland, xwayland_client) = XWayland::spawn(
        display,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        Stdio::null(),
        Stdio::null(),
        |_| {},
    )?;
    let xwm_handle = source_handle.clone();
    Ok(source_handle.insert_source(xwayland, move |event, (), state| match event {
        XWaylandEvent::Ready {
            x11_socket,
            display_number,
        } => match X11Wm::start_wm(xwm_handle.clone(), x11_socket, xwayland_client.clone()) {
            Ok(xwm) => {
                if let Err(error) = state.register_xwayland(xwm, server_id, display_number) {
                    warn!(%error, server_id, display = display_number, "Steam X11 bridge initialization failed");
                }
                if let Some(identifier) = create_identifier {
                    state.publish_xwayland_create_feedback(identifier, server_id, display_number);
                }
                info!(server_id, display = display_number, "Xwayland is ready");
            }
            Err(error) => warn!(%error, server_id, "failed to start the Xwayland window manager"),
        },
        XWaylandEvent::Error => warn!(server_id, "Xwayland exited during startup"),
    })?)
}

fn configure_child_environment(
    child: &mut Command,
    options: &Options,
    state: &State,
    socket_name: &str,
    limiter_file: &Path,
) {
    child
        .env("GAMESCOPE_WAYLAND_DISPLAY", socket_name)
        .env("XDG_CURRENT_DESKTOP", "gamescope")
        .env("GAMESCOPE_LIMITER_FILE", limiter_file)
        .env_remove("ENABLE_VKBASALT")
        .env_remove("SDL_VIDEODRIVER")
        .env_remove("SDL_VIDEO_DRIVER");
    for (name, value) in COMMON_COMPAT_ENV {
        child.env(name, value);
    }
    if options.steam {
        for (name, value) in STEAM_COMPAT_ENV {
            child.env(name, value);
        }
        if options.xwayland_count > 1 {
            child.env("STEAM_MULTIPLE_XWAYLANDS", "1");
        }
    }

    if options.expose_wayland || options.xwayland_count == 0 {
        child
            .env("WAYLAND_DISPLAY", socket_name)
            .env("XDG_SESSION_TYPE", "wayland");
    } else {
        child
            .env_remove("WAYLAND_DISPLAY")
            .env("XDG_SESSION_TYPE", "x11");
    }

    if options.xwayland_count > 1 {
        for game_display in 1..options.xwayland_count {
            if let Some(display_number) = state
                .xdisplays
                .get(&u32::try_from(game_display).unwrap_or(u32::MAX))
            {
                child.env(
                    format!("STEAM_GAME_DISPLAY_{}", game_display - 1),
                    format!(":{display_number}"),
                );
            }
        }
    } else if let Some(display_number) = state.xdisplay {
        child.env("STEAM_GAME_DISPLAY_0", format!(":{display_number}"));
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gamescope_compositor=info,gamescope_rs=info,warn".into()),
        )
        .init();

    let options = Options::parse().map_err(|message| format!("gamescope-rs: {message}"))?;
    match options
        .selected_backend()
        .map_err(|message| format!("gamescope-rs: {message}"))?
    {
        BackendChoice::Nested => run_nested(options),
        BackendChoice::Drm => run_drm(options),
        BackendChoice::Auto => unreachable!("auto backend is resolved above"),
    }
}

fn run_nested(options: Options) -> Result<(), Box<dyn Error>> {
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))?;

    let mut display: Display<State> = Display::new()?;
    let mut handle = display.handle();
    let window_height = options.window_height.unwrap_or(720);
    let window_width = options
        .window_width
        .unwrap_or_else(|| window_height.saturating_mul(16) / 9);
    let output_config = options
        .output
        .resolved_for_output(i32::try_from(window_width)?, i32::try_from(window_height)?);
    let mut state = State::new_with_steam(&handle, &output_config, options.steam);
    let limiter_file = LimiterFile::create()?;
    state.set_limiter_file(limiter_file.path().to_owned());
    let mut event_loop = EventLoop::<State>::try_new()?;
    state.set_loop_handle(event_loop.handle());
    let listener = if let Some(name) = options.socket.as_deref() {
        ListeningSocket::bind(name)?
    } else {
        ListeningSocket::bind_auto("gamescope", 0..32)?
    };
    let socket_name = listener
        .socket_name()
        .and_then(|name| name.to_str())
        .ok_or("Wayland socket has no UTF-8 name")?
        .to_owned();

    let (mut backend, mut winit) = winit_backend::init::<GlesRenderer>()?;
    let requested_size = ::winit::dpi::PhysicalSize::new(window_width, window_height);
    let _ = backend.window().request_inner_size(requested_size);
    if options.borderless || options.fullscreen {
        backend.window().set_decorations(false);
    }
    if options.fullscreen {
        backend
            .window()
            .set_fullscreen(Some(::winit::window::Fullscreen::Borderless(None)));
    }
    state
        .shm_state
        .update_formats(backend.renderer().shm_formats());
    let dmabuf_formats = backend.renderer().dmabuf_formats();
    let feedback = EGLDevice::device_for_display(backend.renderer().egl_context().display())
        .and_then(|device| device.try_get_render_node())
        .ok()
        .flatten()
        .and_then(|node| {
            DmabufFeedbackBuilder::new(node.dev_id(), dmabuf_formats.clone())
                .build()
                .ok()
        });
    if let Some(feedback) = feedback.as_ref() {
        state.init_dmabuf_feedback(&handle, feedback);
    } else {
        state.init_dmabuf(&handle, dmabuf_formats);
    }
    let start = state.started_at;
    let mut serial = 1_u32;
    let mut clients = Vec::new();
    let mut pending_command = (!options.command.is_empty()).then(|| options.command.clone());
    let mut primary_child = None;

    let source_handle = event_loop.handle();
    let mut xwayland_tokens = HashMap::<u32, RegistrationToken>::new();
    for server_id in 0..u32::try_from(options.xwayland_count)? {
        let token = spawn_xwayland(&source_handle, &handle, server_id, None)?;
        xwayland_tokens.insert(server_id, token);
    }
    let mut next_xwayland_server_id = u32::try_from(options.xwayland_count)?;

    info!(socket = socket_name, "Rust Gamescope compositor is ready");

    loop {
        if shutdown.load(Ordering::Relaxed) {
            info!("shutdown signal received");
            return Ok(());
        }
        event_loop.dispatch(Some(Duration::ZERO), &mut state)?;
        if pending_command.is_some() && state.ready_xwayland_count() >= options.xwayland_count {
            let command = pending_command.take().expect("command checked above");
            let (program, arguments) = command.split_first().expect("non-empty command");
            let mut child = Command::new(program);
            child.args(arguments).process_group(0);
            configure_child_environment(
                &mut child,
                &options,
                &state,
                &socket_name,
                limiter_file.path(),
            );
            if let Some(display_number) = state.xdisplay {
                child.env("DISPLAY", format!(":{display_number}"));
            }
            primary_child = Some(ManagedChild(child.spawn()?));
        }
        if !options.keep_alive
            && let Some(status) = primary_child
                .as_mut()
                .and_then(|child| child.try_wait().transpose())
                .transpose()?
        {
            info!(%status, "primary child exited");
            return Ok(());
        }
        let status = winit.dispatch_new_events(|event| match event {
            WinitEvent::Input(InputEvent::Keyboard { event }) => {
                state.bump_input_counter();
                if let Some(keyboard) = state.seat.get_keyboard() {
                    let timestamp =
                        u32::try_from(state.started_at.elapsed().as_millis()).unwrap_or(u32::MAX);
                    let key_state = event.state();
                    let monotonic_time_ns =
                        u64::try_from(state.started_at.elapsed().as_nanos()).unwrap_or(u64::MAX);
                    keyboard.input::<(), _>(
                        &mut state,
                        event.key_code(),
                        key_state,
                        Serial::from(serial),
                        timestamp,
                        |state, _, key| state.filter_key(key_state, &key, monotonic_time_ns),
                    );
                    serial = serial.wrapping_add(1);
                }
            }
            WinitEvent::Input(InputEvent::PointerMotionAbsolute { event }) => {
                state.bump_input_counter();
                state.refresh_focus(Serial::from(serial));
                serial = serial.wrapping_add(1);
                if let Some(mode) = state.output.current_mode() {
                    let location = state.transform_pointer_for_global_scale((
                        event.x_transformed(mode.size.w),
                        event.y_transformed(mode.size.h),
                    ));
                    state.pointer_motion(location.into(), Serial::from(serial), event.time_msec());
                    serial = serial.wrapping_add(1);
                }
            }
            WinitEvent::Input(InputEvent::PointerButton { event }) => {
                state.bump_input_counter();
                state.pointer_button(
                    event.button_code(),
                    event.state() == smithay::backend::input::ButtonState::Pressed,
                    Serial::from(serial),
                    event.time_msec(),
                );
                serial = serial.wrapping_add(1);
            }
            WinitEvent::Input(InputEvent::PointerAxis { event }) => {
                state.bump_input_counter();
                let horizontal = event
                    .amount_v120(Axis::Horizontal)
                    .map(|value| value / 120.0)
                    .or_else(|| event.amount(Axis::Horizontal).map(|value| value / 15.0))
                    .unwrap_or_default();
                let vertical = event
                    .amount_v120(Axis::Vertical)
                    .map(|value| value / 120.0)
                    .or_else(|| event.amount(Axis::Vertical).map(|value| value / 15.0))
                    .unwrap_or_default();
                state.pointer_wheel(horizontal, vertical, event.time_msec());
            }
            _ => {}
        });
        if matches!(status, PumpStatus::Exit(_)) {
            return Ok(());
        }

        while let Some(stream) = listener.accept()? {
            let data: Arc<dyn ClientData> = Arc::new(ClientState::default());
            clients.push(handle.insert_client(stream, data)?);
        }

        display.dispatch_clients(&mut state)?;
        for command in state.process_gamescope_commands(&mut serial) {
            debug!(?command, "command is unavailable on the nested backend");
        }
        let steam_requests = state.process_steam_events(Serial::from(serial));
        serial = serial.wrapping_add(1);
        for request in steam_requests {
            match request {
                SteamRuntimeRequest::CreateXwayland { identifier } => {
                    let server_id = next_xwayland_server_id;
                    next_xwayland_server_id = next_xwayland_server_id.wrapping_add(1);
                    match spawn_xwayland(&source_handle, &handle, server_id, Some(identifier)) {
                        Ok(token) => {
                            xwayland_tokens.insert(server_id, token);
                        }
                        Err(error) => warn!(%error, server_id, "failed to create dynamic Xwayland"),
                    }
                }
                SteamRuntimeRequest::DestroyXwayland { server_id } => {
                    if server_id == 0 {
                        warn!("refusing to destroy the root Xwayland server");
                    } else if let Some(token) = xwayland_tokens.remove(&server_id) {
                        source_handle.remove(token);
                        state.remove_xwayland(server_id);
                        info!(server_id, "destroyed dynamic Xwayland server");
                    }
                }
                SteamRuntimeRequest::SetVrr { enabled } => {
                    debug!(enabled, "ignoring VRR request on nested backend");
                }
                SteamRuntimeRequest::SetForceInternal { force } => {
                    debug!(
                        force,
                        "ignoring connector-selection request on nested backend"
                    );
                }
                SteamRuntimeRequest::RescanDisplay => {
                    debug!("ignoring display-mode nudge on nested backend");
                }
                SteamRuntimeRequest::SetDynamicRefresh { screen, refresh_hz } => {
                    debug!(
                        ?screen,
                        refresh_hz, "ignoring dynamic refresh request on nested backend"
                    );
                }
                SteamRuntimeRequest::SetCompositeForce { force } => {
                    debug!(force, "nested backend always composites");
                }
            }
        }
        state.update_timers();
        display.flush_clients()?;

        let size = backend.window_size();
        let damage = Rectangle::from_size(size);
        {
            let (renderer, mut framebuffer) = backend.bind()?;
            let output_size = state.output.current_mode().map_or(size, |mode| mode.size);
            let render_scale = (f64::from(size.w) / f64::from(output_size.w))
                .min(f64::from(size.h) / f64::from(output_size.h))
                * state.global_scale_ratio();
            let render_origin = centered_origin(size, output_size, render_scale);
            let mut elements = Vec::<
                LayerRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
            >::new();
            if let Some(cursor) = state.cursor_layer() {
                on_commit_buffer_handler::<State>(&cursor.surface);
                let cursor_origin = (
                    render_origin.0 + (cursor.location.x * render_scale).round() as i32,
                    render_origin.1 + (cursor.location.y * render_scale).round() as i32,
                );
                elements.extend(
                    render_elements_from_surface_tree(
                        renderer,
                        &cursor.surface,
                        cursor_origin,
                        1.0,
                        1.0,
                        Kind::Cursor,
                    )
                    .into_iter()
                    .map(|element| {
                        LayerRenderElement::new(
                            RescaleRenderElement::from_element(
                                element,
                                cursor_origin.into(),
                                render_scale,
                            ),
                            false,
                        )
                    }),
                );
            }
            for layer in state.render_layers().into_iter().rev() {
                on_commit_buffer_handler::<State>(&layer.surface);
                elements.extend(
                    render_elements_from_surface_tree(
                        renderer,
                        &layer.surface,
                        render_origin,
                        1.0,
                        layer.alpha,
                        Kind::Unspecified,
                    )
                    .into_iter()
                    .map(|element| {
                        LayerRenderElement::new(
                            RescaleRenderElement::from_element(
                                element,
                                render_origin.into(),
                                render_scale,
                            ),
                            layer.force_blend,
                        )
                    }),
                );
            }
            let mut frame = renderer.render(&mut framebuffer, size, Transform::Flipped180)?;
            frame.clear(Color32F::new(0.0, 0.0, 0.0, 1.0), &[damage])?;
            draw_render_elements(&mut frame, 1.0, &elements, &[damage])?;
            let _sync = frame.finish()?;
        }
        state.presented(start.elapsed());
        display.flush_clients()?;
        backend.submit(Some(&[damage]))?;
    }
}

#[allow(clippy::too_many_lines)]
fn run_drm(options: Options) -> Result<(), Box<dyn Error>> {
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown))?;
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown))?;

    let (mut session, session_notifier) = LibSeatSession::new()?;
    let (udev, drm_path, drm_device_id) = select_drm_device(&options, &session)?;
    let session_fd = session.open(
        &drm_path,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
    )?;
    let worker_fd = session_fd.try_clone()?;
    let _session_device = SessionDevice {
        session: session.clone(),
        fd: Some(session_fd),
    };

    let mut display: Display<State> = Display::new()?;
    let mut handle = display.handle();
    let (mut hardware, mut physical_output) = HardwareBackend::spawn(HardwareConfig {
        device_path: drm_path.clone(),
        device_fd: worker_fd,
        logical_output: options.output.clone(),
        mode_width: options.window_width,
        mode_height: options.window_height,
        mode_refresh_millihz: options.output_refresh_millihz,
        direct_scanout: options.direct_scanout,
        adaptive_sync: options.adaptive_sync,
        connector_priorities: options.connector_priorities.clone(),
    })
    .map_err(|error| format!("failed to initialize atomic DRM backend: {error}"))?;
    let hardware_control = hardware.control();
    let mut output_config = options
        .output
        .resolved_for_output(physical_output.width, physical_output.height);
    output_config.refresh_millihz = physical_output.refresh_millihz;
    let server_config = ServerConfig {
        pipewire_node_id: None,
        active_display: Some(active_display_info(&physical_output)),
    };
    let mut state =
        State::new_with_server_config(&handle, &output_config, options.steam, server_config);
    state.set_dmabuf_node(DrmNode::from_dev_id(physical_output.device_id)?);
    state.enable_vt_switching();
    let limiter_file = LimiterFile::create()?;
    state.set_limiter_file(limiter_file.path().to_owned());
    let mut event_loop = EventLoop::<State>::try_new()?;
    state.set_loop_handle(event_loop.handle());
    let hardware_events = Rc::new(RefCell::new(VecDeque::<HardwareEvent>::new()));
    let hardware_event_queue = Rc::clone(&hardware_events);
    let hardware_event_source = hardware
        .take_event_source()
        .ok_or("DRM event source was already registered")?;
    event_loop.handle().insert_source(
        hardware_event_source,
        move |event, (), _state| match event {
            ChannelEvent::Msg(event) => hardware_event_queue.borrow_mut().push_back(event),
            ChannelEvent::Closed => {
                hardware_event_queue
                    .borrow_mut()
                    .push_back(HardwareEvent::Error("DRM worker stopped".into()));
            }
        },
    )?;
    let listener = if let Some(name) = options.socket.as_deref() {
        ListeningSocket::bind(name)?
    } else {
        ListeningSocket::bind_auto("gamescope", 0..32)?
    };
    let socket_name = listener
        .socket_name()
        .and_then(|name| name.to_str())
        .ok_or("Wayland socket has no UTF-8 name")?
        .to_owned();

    state.publish_hardware_vrr(physical_output.vrr_capable, physical_output.vrr_enabled);

    if supports_syncobj_eventfd(&physical_output.syncobj_device) {
        state.init_drm_syncobj(&handle, physical_output.syncobj_device.clone());
    } else {
        warn!("DRM device does not support syncobj eventfd; explicit-sync global disabled");
    }

    let feedback = hardware_dmabuf_feedback(&physical_output)?;
    state.init_dmabuf_feedback(&handle, &feedback);

    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput_context
        .udev_assign_seat(&session.seat())
        .map_err(|()| "libinput rejected the active seat")?;
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());
    event_loop
        .handle()
        .insert_source(libinput_backend, |event, (), state| {
            process_libinput_event(event, state);
        })?;

    let session_hardware = hardware_control.clone();
    let mut session_libinput = libinput_context.clone();
    event_loop
        .handle()
        .insert_source(session_notifier, move |event, (), state| match event {
            SessionEvent::PauseSession => {
                info!("pausing input and DRM session");
                state.release_all_keys();
                session_libinput.suspend();
                session_hardware.pause();
            }
            SessionEvent::ActivateSession => {
                info!("reactivating input and DRM session");
                if session_libinput.resume().is_err() {
                    warn!("failed to resume libinput");
                }
                session_hardware.resume();
                session_hardware.rescan();
            }
        })?;

    let udev_hardware = hardware_control.clone();
    event_loop
        .handle()
        .insert_source(udev, move |event, (), _state| match event {
            UdevEvent::Changed { device_id } if device_id == drm_device_id => {
                udev_hardware.rescan();
            }
            UdevEvent::Removed { device_id } if device_id == drm_device_id => {
                udev_hardware.pause();
            }
            UdevEvent::Added { device_id, .. } if device_id == drm_device_id => {
                udev_hardware.resume();
                udev_hardware.rescan();
            }
            _ => {}
        })?;

    let mut serial = 1_u32;
    let mut clients = Vec::new();
    let mut pending_command = (!options.command.is_empty()).then(|| options.command.clone());
    let mut primary_child = None;
    let source_handle = event_loop.handle();
    let mut xwayland_tokens = HashMap::<u32, RegistrationToken>::new();
    for server_id in 0..u32::try_from(options.xwayland_count)? {
        let token = spawn_xwayland(&source_handle, &handle, server_id, None)?;
        xwayland_tokens.insert(server_id, token);
    }
    let mut next_xwayland_server_id = u32::try_from(options.xwayland_count)?;
    let mut next_frame_id = 1_u64;
    let mut frame_in_flight = None::<u64>;
    let mut repaint_at = Instant::now();
    let mut idle_present_at = None::<Instant>;
    let mut last_scanout_status = None::<DirectScanoutStatus>;
    let mut output_asleep = false;
    let mut protocol_frame_intervals = [None::<Duration>; 2];
    let monotonic_clock = Clock::<Monotonic>::new();
    let mut last_flip_time = None::<Duration>;
    let mut presentation_sequence = 0_u64;

    info!(
        socket = socket_name,
        device = %drm_path.display(),
        connector = physical_output.connector,
        atomic = physical_output.atomic,
        direct_scanout = options.direct_scanout,
        "Rust Gamescope hardware compositor is ready"
    );

    loop {
        if shutdown.load(Ordering::Relaxed) {
            info!("shutdown signal received");
            return Ok(());
        }
        let now = Instant::now();
        let deadline = if idle_present_at.is_some() {
            idle_present_at
        } else if !output_asleep && frame_in_flight.is_none() {
            Some(repaint_at)
        } else {
            None
        };
        let timeout = deadline
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or(Duration::from_millis(2))
            .min(Duration::from_millis(2));
        event_loop.dispatch(Some(timeout), &mut state)?;
        if let Some(vt) = state.take_vt_switch() {
            info!(vt, "switching virtual terminal");
            if let Err(error) = session.change_vt(vt) {
                warn!(%error, vt, "failed to switch virtual terminal");
            }
        }
        if pending_command.is_some() && state.ready_xwayland_count() >= options.xwayland_count {
            let command = pending_command.take().expect("command checked above");
            let (program, arguments) = command.split_first().expect("non-empty command");
            let mut child = Command::new(program);
            child.args(arguments).process_group(0);
            configure_child_environment(
                &mut child,
                &options,
                &state,
                &socket_name,
                limiter_file.path(),
            );
            if let Some(display_number) = state.xdisplay {
                child.env("DISPLAY", format!(":{display_number}"));
            }
            primary_child = Some(ManagedChild(child.spawn()?));
        }
        if !options.keep_alive
            && let Some(status) = primary_child
                .as_mut()
                .and_then(|child| child.try_wait().transpose())
                .transpose()?
        {
            info!(%status, "primary child exited");
            return Ok(());
        }

        while let Some(stream) = listener.accept()? {
            let data: Arc<dyn ClientData> = Arc::new(ClientState::default());
            clients.push(handle.insert_client(stream, data)?);
        }
        display.dispatch_clients(&mut state)?;
        for command in state.process_gamescope_commands(&mut serial) {
            match command {
                GamescopeCommand::SetRefreshCycle(request) => {
                    hardware_control.set_refresh_cycle(request);
                    protocol_frame_intervals[screen_slot(request.screen)] =
                        refresh_limit_interval(request);
                }
                GamescopeCommand::SetDisplayPower(operation) => {
                    hardware_control.set_display_power(operation);
                }
                command => {
                    debug!(
                        ?command,
                        "backend command is not implemented by the DRM renderer"
                    );
                }
            }
        }
        let steam_requests = state.process_steam_events(Serial::from(serial));
        serial = serial.wrapping_add(1);
        for request in steam_requests {
            match request {
                SteamRuntimeRequest::CreateXwayland { identifier } => {
                    let server_id = next_xwayland_server_id;
                    next_xwayland_server_id = next_xwayland_server_id.wrapping_add(1);
                    match spawn_xwayland(&source_handle, &handle, server_id, Some(identifier)) {
                        Ok(token) => {
                            xwayland_tokens.insert(server_id, token);
                        }
                        Err(error) => warn!(%error, server_id, "failed to create dynamic Xwayland"),
                    }
                }
                SteamRuntimeRequest::DestroyXwayland { server_id } => {
                    if server_id == 0 {
                        warn!("refusing to destroy the root Xwayland server");
                    } else if let Some(token) = xwayland_tokens.remove(&server_id) {
                        source_handle.remove(token);
                        state.remove_xwayland(server_id);
                        info!(server_id, "destroyed dynamic Xwayland server");
                    }
                }
                SteamRuntimeRequest::SetVrr { enabled } => {
                    hardware_control.set_vrr(enabled);
                }
                SteamRuntimeRequest::SetForceInternal { force } => {
                    hardware_control.set_force_internal(force);
                }
                SteamRuntimeRequest::RescanDisplay => {
                    hardware_control.nudge_modeset();
                }
                SteamRuntimeRequest::SetDynamicRefresh { screen, refresh_hz } => {
                    hardware_control.set_dynamic_refresh(screen, refresh_hz);
                }
                SteamRuntimeRequest::SetCompositeForce { force } => {
                    hardware_control.set_composite_force(force);
                }
            }
        }
        state.update_timers();

        while let Some(event) = hardware_events.borrow_mut().pop_front() {
            match event {
                HardwareEvent::Presented {
                    frame_id,
                    at,
                    monotonic_time,
                    sequence,
                    scanout_status,
                } => {
                    if frame_in_flight == Some(frame_id) {
                        frame_in_flight = None;
                    }
                    if last_scanout_status != Some(scanout_status) {
                        info!(?scanout_status, "DRM primary-plane path changed");
                        state.publish_direct_scanout_status(scanout_status.code());
                        last_scanout_status = Some(scanout_status);
                    }
                    debug!(
                        frame_id,
                        sequence,
                        ?scanout_status,
                        "DRM page flip completed"
                    );
                    let refresh = physical_refresh_interval(&physical_output);
                    last_flip_time = Some(
                        monotonic_time.unwrap_or_else(|| Duration::from(monotonic_clock.now())),
                    );
                    repaint_at = at
                        + repaint_delay(
                            refresh,
                            protocol_frame_intervals[screen_slot(physical_output.screen)],
                        );
                }
                HardwareEvent::FrameDeferred { frame_id } => {
                    if frame_in_flight == Some(frame_id) {
                        frame_in_flight = None;
                    }
                    idle_present_at =
                        Some(Instant::now() + physical_refresh_interval(&physical_output));
                }
                HardwareEvent::OutputChanged(output) => {
                    physical_output = output;
                    output_asleep = false;
                    state.publish_active_display(active_display_info(&physical_output));
                    let mut logical_output = options
                        .output
                        .resolved_for_output(physical_output.width, physical_output.height);
                    logical_output.refresh_millihz = physical_output.refresh_millihz;
                    state.publish_output_mode(&logical_output);
                    state.publish_hardware_vrr(
                        physical_output.vrr_capable,
                        physical_output.vrr_enabled,
                    );
                    match hardware_dmabuf_feedback(&physical_output) {
                        Ok(feedback) => state.update_dmabuf_feedback(&feedback),
                        Err(error) => {
                            warn!(%error, "failed to update dma-buf feedback after hotplug");
                        }
                    }
                    repaint_at = Instant::now();
                }
                HardwareEvent::OutputDisconnected => {
                    warn!("DRM output disconnected; waiting for hotplug");
                    frame_in_flight = None;
                    output_asleep = true;
                }
                HardwareEvent::OutputPowerChanged { asleep } => {
                    output_asleep = asleep;
                    frame_in_flight = None;
                    idle_present_at = None;
                    if !asleep {
                        repaint_at = Instant::now();
                    }
                }
                HardwareEvent::Error(message) => {
                    warn!(%message, "hardware compositor error");
                    frame_in_flight = None;
                    repaint_at = Instant::now() + physical_refresh_interval(&physical_output);
                }
            }
        }

        let now = Instant::now();
        if idle_present_at.is_some_and(|deadline| now >= deadline) {
            idle_present_at = None;
            let refresh = physical_refresh_interval(&physical_output);
            presentation_sequence = presentation_sequence.wrapping_add(1);
            state.presented_with_metadata(
                Duration::from(monotonic_clock.now()),
                refresh,
                presentation_sequence,
                wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync
                    | wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::HwClock,
            );
            repaint_at = now + Duration::from_millis(1);
        }
        if !output_asleep
            && frame_in_flight.is_none()
            && idle_present_at.is_none()
            && now >= repaint_at
        {
            let frame_id = next_frame_id;
            next_frame_id = next_frame_id.wrapping_add(1);
            if let Some(replaced) = hardware.submit(HardwareFrame {
                id: frame_id,
                layers: state.render_layers(),
                cursor: state.cursor_layer(),
            }) {
                debug!(replaced, frame_id, "coalesced obsolete DRM frame");
            }
            // Gamescope intentionally releases PresentWait at the latest
            // useful latch point and reports the predicted next display time.
            // Waiting for the page-flip completion here costs an entire frame
            // of application backpressure, especially at high refresh rates.
            let refresh = physical_refresh_interval(&physical_output);
            let interval = presentation_interval(
                refresh,
                protocol_frame_intervals[screen_slot(physical_output.screen)],
            );
            let predicted_present_time = last_flip_time
                .map(|last_flip| last_flip.saturating_add(interval))
                .unwrap_or_else(|| Duration::from(monotonic_clock.now()).saturating_add(refresh));
            let presentation_kind =
                wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync
                    | wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::HwClock
                    | wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::ZeroCopy;
            presentation_sequence = presentation_sequence.wrapping_add(1);
            state.presented_with_metadata(
                predicted_present_time,
                interval,
                presentation_sequence,
                presentation_kind,
            );
            frame_in_flight = Some(frame_id);
        }
        display.flush_clients()?;
    }
}

fn select_drm_device(
    options: &Options,
    session: &LibSeatSession,
) -> Result<(UdevBackend, PathBuf, u64), Box<dyn Error>> {
    let udev = UdevBackend::new(session.seat())?;
    let devices = udev
        .device_list()
        .map(|(device_id, path)| (device_id, path.to_owned()))
        .collect::<Vec<_>>();
    let requested = if let Some(path) = options.drm_device.as_ref() {
        Some(path.clone())
    } else {
        primary_gpu(session.seat())?
    };
    let selected = requested
        .as_ref()
        .and_then(|requested| devices.iter().find(|(_, path)| path == requested).cloned())
        .or_else(|| devices.first().cloned())
        .ok_or_else(|| format!("no DRM primary nodes found on {}", session.seat()))?;
    if let Some(requested) = requested
        && selected.1 != requested
        && options.drm_device.is_some()
    {
        return Err(format!(
            "requested DRM device {} is not available on {}",
            requested.display(),
            session.seat()
        )
        .into());
    }
    Ok((udev, selected.1, selected.0))
}

fn process_libinput_event(event: InputEvent<LibinputInputBackend>, state: &mut State) {
    match event {
        InputEvent::Keyboard { event } => {
            state.bump_input_counter();
            let key_state = event.state();
            let keycode = event.key_code();
            let vt_intercepted = state.filter_vt_keycode(keycode, key_state);
            if let Some(keyboard) = state.seat.get_keyboard() {
                keyboard.input::<(), _>(
                    state,
                    keycode,
                    key_state,
                    SERIAL_COUNTER.next_serial(),
                    event.time_msec(),
                    |state, _, key| {
                        if vt_intercepted {
                            FilterResult::Intercept(())
                        } else {
                            state.filter_key(key_state, &key, event.time() * 1_000)
                        }
                    },
                );
            }
        }
        InputEvent::PointerMotion { event } => {
            state.bump_input_counter();
            state.refresh_focus(SERIAL_COUNTER.next_serial());
            state.pointer_motion_relative(
                event.delta(),
                event.delta_unaccel(),
                SERIAL_COUNTER.next_serial(),
                event.time_msec(),
                event.time(),
            );
        }
        InputEvent::PointerMotionAbsolute { event } => {
            state.bump_input_counter();
            state.refresh_focus(SERIAL_COUNTER.next_serial());
            if let Some(mode) = state.output.current_mode() {
                let location = state.transform_pointer_for_global_scale((
                    event.x_transformed(mode.size.w),
                    event.y_transformed(mode.size.h),
                ));
                state.pointer_motion(
                    location.into(),
                    SERIAL_COUNTER.next_serial(),
                    event.time_msec(),
                );
            }
        }
        InputEvent::PointerButton { event } => {
            state.bump_input_counter();
            state.pointer_button(
                event.button_code(),
                event.state() == smithay::backend::input::ButtonState::Pressed,
                SERIAL_COUNTER.next_serial(),
                event.time_msec(),
            );
        }
        InputEvent::PointerAxis { event } => {
            state.bump_input_counter();
            let horizontal = event
                .amount_v120(Axis::Horizontal)
                .map(|value| value / 120.0)
                .or_else(|| event.amount(Axis::Horizontal).map(|value| value / 15.0))
                .unwrap_or_default();
            let vertical = event
                .amount_v120(Axis::Vertical)
                .map(|value| value / 120.0)
                .or_else(|| event.amount(Axis::Vertical).map(|value| value / 15.0))
                .unwrap_or_default();
            state.pointer_wheel(horizontal, vertical, event.time_msec());
        }
        InputEvent::TouchDown { event } => {
            state.bump_input_counter();
            if let Some(mode) = state.output.current_mode() {
                let location = state.transform_pointer_for_global_scale((
                    event.x_transformed(mode.size.w),
                    event.y_transformed(mode.size.h),
                ));
                state.touch_down(
                    location.into(),
                    event.slot(),
                    SERIAL_COUNTER.next_serial(),
                    event.time_msec(),
                );
            }
        }
        InputEvent::TouchMotion { event } => {
            state.bump_input_counter();
            if let Some(mode) = state.output.current_mode() {
                let location = state.transform_pointer_for_global_scale((
                    event.x_transformed(mode.size.w),
                    event.y_transformed(mode.size.h),
                ));
                state.touch_motion(location.into(), event.slot(), event.time_msec());
            }
        }
        InputEvent::TouchUp { event } => {
            state.bump_input_counter();
            state.touch_up(
                event.slot(),
                SERIAL_COUNTER.next_serial(),
                event.time_msec(),
            );
        }
        InputEvent::TouchFrame { .. } => state.touch_frame(),
        InputEvent::TouchCancel { .. } => state.touch_cancel(),
        InputEvent::GestureSwipeBegin { event } => {
            state.gesture_swipe_begin(event.fingers(), event.time_msec());
        }
        InputEvent::GestureSwipeUpdate { event } => {
            state.gesture_swipe_update(event.delta(), event.time_msec());
        }
        InputEvent::GestureSwipeEnd { event } => {
            state.gesture_swipe_end(event.cancelled(), event.time_msec());
        }
        InputEvent::GesturePinchBegin { event } => {
            state.gesture_pinch_begin(event.fingers(), event.time_msec());
        }
        InputEvent::GesturePinchUpdate { event } => {
            state.gesture_pinch_update(
                event.delta(),
                event.scale(),
                event.rotation(),
                event.time_msec(),
            );
        }
        InputEvent::GesturePinchEnd { event } => {
            state.gesture_pinch_end(event.cancelled(), event.time_msec());
        }
        InputEvent::GestureHoldBegin { event } => {
            state.gesture_hold_begin(event.fingers(), event.time_msec());
        }
        InputEvent::GestureHoldEnd { event } => {
            state.gesture_hold_end(event.cancelled(), event.time_msec());
        }
        _ => {}
    }
}

fn physical_refresh_interval(output: &HardwareOutputInfo) -> Duration {
    u64::try_from(output.refresh_millihz)
        .ok()
        .filter(|refresh| *refresh != 0)
        .map_or(Duration::from_nanos(16_666_667), |refresh| {
            Duration::from_nanos(1_000_000_000_000 / refresh)
        })
}

fn screen_slot(screen: ScreenType) -> usize {
    match screen {
        ScreenType::Internal => 0,
        ScreenType::External => 1,
    }
}

fn refresh_limit_interval(request: RefreshCycleOverride) -> Option<Duration> {
    (request.apply_frame_limiter && request.frames_per_second != 0)
        .then(|| Duration::from_nanos(1_000_000_000 / u64::from(request.frames_per_second)))
}

fn repaint_delay(refresh: Duration, frame_limit: Option<Duration>) -> Duration {
    let target = presentation_interval(refresh, frame_limit);
    target.saturating_sub(refresh.mul_f64(0.4))
}

fn presentation_interval(refresh: Duration, frame_limit: Option<Duration>) -> Duration {
    frame_limit.map_or(refresh, |limit| limit.max(refresh))
}

fn active_display_info(output: &HardwareOutputInfo) -> ActiveDisplayInfo {
    let mut flags = u32::from(output.screen == ScreenType::Internal);
    if output.vrr_capable {
        flags |= 0x4;
    }
    ActiveDisplayInfo {
        connector_name: output.connector.clone(),
        display_make: output.display_make.clone(),
        display_model: output.display_model.clone(),
        flags,
        valid_refresh_rates_hz: output.valid_refresh_rates_hz.clone(),
    }
}

fn hardware_dmabuf_feedback(output: &HardwareOutputInfo) -> Result<DmabufFeedback, std::io::Error> {
    DmabufFeedbackBuilder::new(output.device_id, output.render_formats.clone())
        .add_preference_tranche(
            output.device_id,
            Some(zwp_linux_dmabuf_feedback_v1::TrancheFlags::Scanout),
            output.scanout_formats.clone(),
        )
        .build()
}

fn centered_origin(
    target: smithay::utils::Size<i32, smithay::utils::Physical>,
    source: smithay::utils::Size<i32, smithay::utils::Physical>,
    scale: f64,
) -> (i32, i32) {
    (
        ((f64::from(target.w) - f64::from(source.w) * scale) / 2.0).round() as i32,
        ((f64::from(target.h) - f64::from(source.h) * scale) / 2.0).round() as i32,
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gamescope_core::control::{RefreshCycleOverride, ScreenType};

    use super::{Options, refresh_limit_interval, repaint_delay};

    #[test]
    fn steam_and_multi_xwayland_options_are_not_noops() {
        let options = Options::parse_from(
            ["--steam", "--xwayland-count", "3", "--expose-wayland"]
                .into_iter()
                .map(str::to_owned),
        )
        .expect("valid Steam options");
        assert!(options.steam);
        assert!(options.expose_wayland);
        assert_eq!(options.xwayland_count, 3);
    }

    #[test]
    fn no_xwayland_is_equivalent_to_zero_servers() {
        let options = Options::parse_from(["--no-xwayland"].into_iter().map(str::to_owned))
            .expect("valid no-Xwayland option");
        assert_eq!(options.xwayland_count, 0);
    }

    #[test]
    fn omitted_game_size_is_left_for_the_backend_to_resolve() {
        let options = Options::parse_from(std::iter::empty()).expect("default options");
        assert_eq!((options.output.width, options.output.height), (0, 0));
        assert_eq!(
            options.output.resolved_for_output(2560, 1440).mode().size,
            (2560, 1440).into()
        );
    }

    #[test]
    fn width_without_height_matches_gamescope_rejection() {
        let error = Options::parse_from(["-w", "1920"].into_iter().map(str::to_owned))
            .expect_err("-w without -h must fail");
        assert!(error.contains("cannot specify -w without -h"));
    }

    #[test]
    fn refresh_cycle_limiter_keeps_a_render_margin() {
        let physical = Duration::from_nanos(12_500_000);
        let request = RefreshCycleOverride {
            screen: ScreenType::Internal,
            frames_per_second: 40,
            allow_refresh_switching: true,
            apply_frame_limiter: true,
        };
        let limit = refresh_limit_interval(request).expect("limiter enabled");
        assert_eq!(limit, Duration::from_millis(25));
        assert_eq!(
            repaint_delay(physical, Some(limit)),
            Duration::from_millis(20)
        );
    }

    #[test]
    fn drm_options_preserve_connector_order_and_atomic_features() {
        let options = Options::parse_from(
            [
                "--drm",
                "-O",
                "DP-2,*,HDMI-A-1",
                "--adaptive-sync",
                "--disable-direct-scanout",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .expect("valid DRM options");
        assert_eq!(options.backend, super::BackendChoice::Drm);
        assert_eq!(options.connector_priorities, ["DP-2", "*", "HDMI-A-1"]);
        assert!(options.adaptive_sync);
        assert!(!options.direct_scanout);
    }
}
