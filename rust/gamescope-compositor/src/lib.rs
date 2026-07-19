//! Native Rust compositor frontend for Gamescope.

#![deny(unsafe_code)]

pub mod drm;
pub mod perfetto;
pub mod screenshot;
pub mod steam;

pub use perfetto::perfetto_te_ns;

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fmt::Write as _,
    os::fd::OwnedFd,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use gamescope_core::{
    control::ScreenType,
    input_method::{InputMethodAction, InputMethodCommit},
    swapchain::CommitMetadata,
};
use gamescope_wayland_server::{
    ActiveDisplayInfo, Command, GamescopeHandler, GamescopeState, InputMethodCommand, ServerConfig,
    delegate_gamescope,
};
use perfetto_sdk::track_event::EventContext;
use smithay::{
    backend::{
        allocator::{Format, dmabuf::Dmabuf},
        drm::DrmNode,
        input::{Axis, AxisSource, ButtonState, KeyState},
        renderer::{
            Renderer,
            element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
            utils::{CommitCounter, DamageSet, OpaqueRegions, on_commit_buffer_handler},
        },
    },
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_drm_syncobj,
    delegate_fractional_scale, delegate_layer_shell, delegate_output, delegate_pointer_constraints,
    delegate_pointer_gestures, delegate_presentation, delegate_relative_pointer, delegate_seat,
    delegate_shm, delegate_single_pixel_buffer, delegate_viewporter, delegate_xdg_shell,
    delegate_xwayland_shell,
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{FilterResult, KeyboardTarget, KeysymHandle, ModifiersState, XkbConfig},
        pointer::{
            AxisFrame, ButtonEvent, CursorImageAttributes, CursorImageStatus,
            GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
            GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
            GestureSwipeEndEvent, GestureSwipeUpdateEvent, MotionEvent, PointerHandle,
            RelativeMotionEvent,
        },
        touch::{
            DownEvent as TouchDownEvent, MotionEvent as TouchMotionEvent, UpEvent as TouchUpEvent,
        },
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::calloop::{
        LoopHandle, RegistrationToken,
        channel::Channel,
        timer::{TimeoutAction, Timer},
    },
    utils::{
        Buffer, IsAlive, Logical, Physical, Point, Rectangle, SERIAL_COUNTER, Serial, Transform,
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
            TraversalAction, add_blocker, add_pre_commit_hook, with_states,
            with_surface_tree_downward,
        },
        dmabuf::{DmabufFeedback, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        drm_syncobj::{DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState},
        fractional_scale::{FractionalScaleHandler, FractionalScaleManagerState},
        output::{OutputHandler, OutputManagerState},
        pointer_constraints::{PointerConstraintsHandler, PointerConstraintsState},
        pointer_gestures::PointerGesturesState,
        presentation::{PresentationFeedbackCachedState, PresentationState, Refresh},
        relative_pointer::RelativePointerManagerState,
        seat::WaylandFocus,
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
            },
        },
        shell::{
            wlr_layer::{Layer, LayerSurface, WlrLayerShellHandler, WlrLayerShellState},
            xdg::{PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState},
        },
        shm::{ShmHandler, ShmState},
        single_pixel_buffer::SinglePixelBufferState,
        viewporter::ViewporterState,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWaylandClientData, XwmHandler,
        xwm::{Reorder, ResizeEdge, WmWindowProperty, XwmId},
    },
};
use steam::{
    BridgeEvent, FocusCandidate, FocusControl, SteamBridgeWorker, SteamWorkerEvent,
    TimedSteamWorkerEvent, WindowMetadata, select_focus, select_managed_ancestor, select_override,
};
use wayland_protocols::{
    wp::presentation_time::server::wp_presentation_feedback, xdg::shell::server::xdg_toplevel,
};
use wayland_server::{
    Client, DisplayHandle, Resource,
    backend::{ClientData, ClientId, DisconnectReason},
    protocol::{wl_buffer, wl_output, wl_seat, wl_shm, wl_surface::WlSurface},
};

/// Runtime output configuration shared by the nested and future DRM backends.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputConfig {
    pub width: i32,
    pub height: i32,
    pub refresh_millihz: i32,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            refresh_millihz: 60_000,
        }
    }
}

impl OutputConfig {
    /// An output size which should be resolved from the backend's selected
    /// physical mode. This mirrors Gamescope's zero-initialized
    /// `g_nNestedWidth`/`g_nNestedHeight` command-line state.
    #[must_use]
    pub const fn unspecified() -> Self {
        Self {
            width: 0,
            height: 0,
            refresh_millihz: 60_000,
        }
    }

    /// Resolve an omitted game size against the backend output.
    ///
    /// Gamescope uses the physical mode when neither dimension was supplied,
    /// and derives a 16:9 width when only `-h` was supplied. `-w` without
    /// `-h` is rejected while parsing command-line options.
    #[must_use]
    pub fn resolved_for_output(&self, width: i32, height: i32) -> Self {
        let (width, height) = if self.height == 0 {
            (width, height)
        } else if self.width == 0 {
            (self.height.saturating_mul(16) / 9, self.height)
        } else {
            (self.width, self.height)
        };
        Self {
            width,
            height,
            refresh_millihz: self.refresh_millihz,
        }
    }

    #[must_use]
    pub fn mode(&self) -> OutputMode {
        OutputMode {
            size: smithay::utils::Size::from((self.width, self.height)),
            refresh: self.refresh_millihz,
        }
    }
}

/// Per-client state required by Smithay's compositor implementation.
#[derive(Debug, Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

/// A surface selected for nested composition, in bottom-to-top order.
#[derive(Clone, Debug)]
pub struct RenderLayer {
    pub surface: WlSurface,
    pub alpha: f32,
    /// The layer contains per-pixel transparency and must not occlude lower
    /// layers based solely on Xwayland's opaque-region hint.
    pub force_blend: bool,
}

/// Preserve a render element's storage and geometry while optionally ignoring
/// its opaque-region hint. Steam overlay buffers contain per-pixel alpha, and
/// treating Xwayland's full-window hint as authoritative can cull the game
/// below transparent pixels and replace it with the clear color.
#[derive(Debug)]
pub struct LayerRenderElement<E> {
    element: E,
    force_blend: bool,
}

impl<E> LayerRenderElement<E> {
    #[must_use]
    pub const fn new(element: E, force_blend: bool) -> Self {
        Self {
            element,
            force_blend,
        }
    }
}

impl<E: Element> Element for LayerRenderElement<E> {
    fn id(&self) -> &Id {
        self.element.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.element.current_commit()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.element.src()
    }

    fn geometry(&self, scale: smithay::utils::Scale<f64>) -> Rectangle<i32, Physical> {
        self.element.geometry(scale)
    }

    fn transform(&self) -> Transform {
        self.element.transform()
    }

    fn damage_since(
        &self,
        scale: smithay::utils::Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.element.damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: smithay::utils::Scale<f64>) -> OpaqueRegions<i32, Physical> {
        if self.force_blend {
            OpaqueRegions::default()
        } else {
            self.element.opaque_regions(scale)
        }
    }

    fn alpha(&self) -> f32 {
        self.element.alpha()
    }

    fn kind(&self) -> Kind {
        self.element.kind()
    }
}

impl<R, E> RenderElement<R> for LayerRenderElement<E>
where
    R: Renderer,
    E: RenderElement<R>,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
    ) -> Result<(), R::Error> {
        self.element.draw(frame, src, dst, damage, opaque_regions)
    }

    fn underlying_storage(&self, renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        self.element.underlying_storage(renderer)
    }
}

/// Client-supplied cursor surface positioned in logical output coordinates.
#[derive(Clone, Debug)]
pub struct CursorLayer {
    pub surface: WlSurface,
    pub location: Point<f64, Logical>,
}

/// Runtime requests sent through Steam's root-window compatibility channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SteamRuntimeRequest {
    CreateXwayland { identifier: u32 },
    DestroyXwayland { server_id: u32 },
    SetVrr { enabled: bool },
    SetForceInternal { force: bool },
    RescanDisplay,
    SetDynamicRefresh { screen: ScreenType, refresh_hz: u32 },
    SetCompositeForce { force: bool },
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}

    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Keyboard focus must retain the X11 wrapper so Smithay can apply the ICCCM
/// input model before forwarding keys to Xwayland's Wayland surface.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyboardFocusTarget {
    Wayland(WlSurface),
    X11(X11Surface),
}

impl KeyboardFocusTarget {
    fn owned_wl_surface(&self) -> Option<WlSurface> {
        match self {
            Self::Wayland(surface) => Some(surface.clone()),
            Self::X11(window) => window.wl_surface(),
        }
    }

    fn trace_enter(&self) {
        match self {
            Self::Wayland(surface) => perfetto_sdk::track_event_instant!(
                "gamescope.input",
                "Wayland keyboard focus enter",
                |ctx: &mut EventContext| perfetto::add_event_fields(
                    ctx,
                    &[perfetto::EventField::SurfaceAlive(surface.is_alive())],
                )
            ),
            Self::X11(window) => perfetto_sdk::track_event_instant!(
                "gamescope.input",
                "X11 keyboard focus enter",
                |ctx: &mut EventContext| perfetto::add_event_fields(
                    ctx,
                    &[
                        perfetto::EventField::Window(u64::from(window.window_id())),
                        perfetto::EventField::SurfaceAlive(window.alive()),
                        perfetto::EventField::SurfaceAssociated(window.wl_surface().is_some()),
                    ],
                )
            ),
        }
    }

    fn trace_leave(&self) {
        match self {
            Self::Wayland(surface) => perfetto_sdk::track_event_instant!(
                "gamescope.input",
                "Wayland keyboard focus leave",
                |ctx: &mut EventContext| perfetto::add_event_fields(
                    ctx,
                    &[perfetto::EventField::SurfaceAlive(surface.is_alive())],
                )
            ),
            Self::X11(window) => perfetto_sdk::track_event_instant!(
                "gamescope.input",
                "X11 keyboard focus leave",
                |ctx: &mut EventContext| perfetto::add_event_fields(
                    ctx,
                    &[
                        perfetto::EventField::Window(u64::from(window.window_id())),
                        perfetto::EventField::SurfaceAlive(window.alive()),
                        perfetto::EventField::SurfaceAssociated(window.wl_surface().is_some()),
                    ],
                )
            ),
        }
    }

    fn trace_key(&self, keysym: u64, pressed: bool) {
        match self {
            Self::Wayland(surface) => perfetto_sdk::track_event_instant!(
                "gamescope.input",
                "Wayland keyboard target key",
                |ctx: &mut EventContext| perfetto::add_event_fields(
                    ctx,
                    &[
                        perfetto::EventField::SurfaceAlive(surface.is_alive()),
                        perfetto::EventField::Keysym(keysym),
                        perfetto::EventField::Pressed(pressed),
                    ],
                )
            ),
            Self::X11(window) => perfetto_sdk::track_event_instant!(
                "gamescope.input",
                "X11 keyboard target key",
                |ctx: &mut EventContext| perfetto::add_event_fields(
                    ctx,
                    &[
                        perfetto::EventField::Window(u64::from(window.window_id())),
                        perfetto::EventField::SurfaceAlive(window.alive()),
                        perfetto::EventField::SurfaceAssociated(window.wl_surface().is_some()),
                        perfetto::EventField::Keysym(keysym),
                        perfetto::EventField::Pressed(pressed),
                    ],
                )
            ),
        }
    }
}

impl WaylandFocus for KeyboardFocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(surface) => Some(Cow::Borrowed(surface)),
            Self::X11(window) => WaylandFocus::wl_surface(window),
        }
    }
}

impl IsAlive for KeyboardFocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(surface) => IsAlive::alive(surface),
            Self::X11(window) => IsAlive::alive(window),
        }
    }
}

impl KeyboardTarget<State> for KeyboardFocusTarget {
    fn enter(
        &self,
        seat: &Seat<State>,
        state: &mut State,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        self.trace_enter();
        match self {
            Self::Wayland(surface) => KeyboardTarget::enter(surface, seat, state, keys, serial),
            Self::X11(window) => KeyboardTarget::enter(window, seat, state, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<State>, state: &mut State, serial: Serial) {
        self.trace_leave();
        match self {
            Self::Wayland(surface) => KeyboardTarget::leave(surface, seat, state, serial),
            Self::X11(window) => KeyboardTarget::leave(window, seat, state, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<State>,
        state: &mut State,
        key: KeysymHandle<'_>,
        key_state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        self.trace_key(
            u64::from(key.modified_sym().raw()),
            key_state == KeyState::Pressed,
        );
        match self {
            Self::Wayland(surface) => {
                KeyboardTarget::key(surface, seat, state, key, key_state, serial, time);
            }
            Self::X11(window) => {
                KeyboardTarget::key(window, seat, state, key, key_state, serial, time);
            }
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<State>,
        state: &mut State,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(surface) => {
                KeyboardTarget::modifiers(surface, seat, state, modifiers, serial);
            }
            Self::X11(window) => {
                KeyboardTarget::modifiers(window, seat, state, modifiers, serial);
            }
        }
    }
}

const fn x11_property_changes_input_mode(property: WmWindowProperty) -> bool {
    matches!(
        property,
        WmWindowProperty::Hints | WmWindowProperty::Protocols
    )
}

fn x11_window_identity_matches<T: PartialEq>(
    candidate_xwm_id: Option<T>,
    candidate_window_id: u32,
    xwm_id: T,
    window_id: u32,
) -> bool {
    candidate_xwm_id == Some(xwm_id) && candidate_window_id == window_id
}

/// Complete protocol state for the compositor frontend.
pub struct State {
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub output_manager_state: OutputManagerState,
    pub fractional_scale_state: FractionalScaleManagerState,
    pub presentation_state: PresentationState,
    pub dmabuf_state: DmabufState,
    pub drm_syncobj_state: Option<DrmSyncobjState>,
    pub xwayland_shell_state: XWaylandShellState,
    pub gamescope_state: GamescopeState,
    pub seat: Seat<Self>,
    pub pointer: PointerHandle<Self>,
    pub output: Output,
    pub pointer_location: Point<f64, Logical>,
    pub focused_surface: Option<WlSurface>,
    keyboard_focus: Option<KeyboardFocusTarget>,
    pub cursor_status: CursorImageStatus,
    pub started_at: Instant,
    pub xwms: HashMap<XwmId, X11Wm>,
    pub xdisplay: Option<u32>,
    pub xdisplays: HashMap<u32, u32>,
    dmabuf_global: Option<DmabufGlobal>,
    dmabuf_node: Option<DrmNode>,
    frame_sequence: u64,
    pending_swapchain_commits: Vec<(WlSurface, CommitMetadata)>,
    pressed_keysyms: Vec<u32>,
    pressed_evdev_keys: HashSet<u32>,
    intercepted_vt_keys: HashSet<u32>,
    ime_reset_timer: Option<RegistrationToken>,
    x11_windows: Vec<X11Surface>,
    x11_window_metadata: HashMap<(XwmId, u32), WindowMetadata>,
    x11_window_sequences: HashMap<(XwmId, u32), u64>,
    xwayland_server_ids: HashMap<XwmId, u32>,
    steam_ready_servers: HashSet<u32>,
    steam_worker: SteamBridgeWorker,
    pending_steam_events: Vec<TimedSteamWorkerEvent>,
    focus_control: FocusControl,
    steam_mode: bool,
    games_running: u32,
    steam_max_height: u32,
    input_counter: u32,
    fps_limit: u32,
    limiter_file: Option<PathBuf>,
    overscan_scale: f64,
    zoom_scale: f64,
    next_window_sequence: u64,
    last_x11_focus: Option<(XwmId, u32)>,
    content_overrides: HashMap<(u32, u32), WlSurface>,
    content_override_parents: HashMap<(u32, u32), u32>,
    content_override_pending: HashSet<(u32, u32)>,
    loop_handle: Option<LoopHandle<'static, Self>>,
    vt_switching: bool,
    pending_vt: Option<i32>,
}

impl State {
    /// Create every core and Gamescope global needed by ordinary Wayland games.
    ///
    /// # Panics
    ///
    /// Panics if the system's default XKB keymap cannot be compiled.
    #[must_use]
    pub fn new(handle: &DisplayHandle, output_config: &OutputConfig) -> Self {
        Self::new_with_steam(handle, output_config, false)
    }

    /// Create compositor state and optionally enable Steam's X11 policy.
    #[must_use]
    pub fn new_with_steam(
        handle: &DisplayHandle,
        output_config: &OutputConfig,
        steam_mode: bool,
    ) -> Self {
        let server_config = ServerConfig {
            pipewire_node_id: None,
            active_display: Some(ActiveDisplayInfo {
                connector_name: "gamescope".into(),
                display_make: "Valve".into(),
                display_model: "Gamescope".into(),
                flags: 0,
                valid_refresh_rates_hz: vec![
                    u32::try_from((output_config.refresh_millihz + 499) / 1000).unwrap_or(60),
                ],
            }),
        };
        Self::new_with_server_config(handle, output_config, steam_mode, server_config)
    }

    /// Create compositor state with backend-provided Gamescope protocol data.
    #[must_use]
    pub fn new_with_server_config(
        handle: &DisplayHandle,
        output_config: &OutputConfig,
        steam_mode: bool,
        server_config: ServerConfig,
    ) -> Self {
        let compositor_state = CompositorState::new::<Self>(handle);
        let xdg_shell_state = XdgShellState::new::<Self>(handle);
        let layer_shell_state = WlrLayerShellState::new::<Self>(handle);
        let mut shm_state = ShmState::new::<Self>(handle, vec![]);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(handle, "seat0");
        let pointer = seat.add_pointer();
        seat.add_touch();
        seat.add_keyboard(XkbConfig::default(), 200, 25)
            .expect("the built-in xkb configuration must compile");
        let data_device_state = DataDeviceState::new::<Self>(handle);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(handle);
        let fractional_scale_state = FractionalScaleManagerState::new::<Self>(handle);
        let presentation_state = PresentationState::new::<Self>(handle, 1);
        let dmabuf_state = DmabufState::new();
        let xwayland_shell_state = XWaylandShellState::new::<Self>(handle);
        ViewporterState::new::<Self>(handle);
        RelativePointerManagerState::new::<Self>(handle);
        PointerConstraintsState::new::<Self>(handle);
        PointerGesturesState::new::<Self>(handle);
        SinglePixelBufferState::new::<Self>(handle);

        let mode = output_config.mode();
        let output = Output::new(
            "gamescope".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Valve".into(),
                model: "Gamescope".into(),
            },
        );
        output.create_global::<Self>(handle);
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Fractional(1.0)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);

        // The nested GLES renderer imports ARGB/XRGB shm buffers. Additional
        // formats are added by the backend after renderer initialization.
        shm_state.update_formats([wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888]);

        let _gamescope_globals =
            GamescopeState::register_globals_for::<Self>(handle, &server_config);

        Self {
            compositor_state,
            xdg_shell_state,
            layer_shell_state,
            shm_state,
            seat_state,
            data_device_state,
            output_manager_state,
            fractional_scale_state,
            presentation_state,
            dmabuf_state,
            drm_syncobj_state: None,
            xwayland_shell_state,
            gamescope_state: GamescopeState::with_config(&server_config),
            seat,
            pointer,
            output,
            pointer_location: (0.0, 0.0).into(),
            focused_surface: None,
            keyboard_focus: None,
            cursor_status: CursorImageStatus::default_named(),
            started_at: Instant::now(),
            xwms: HashMap::new(),
            xdisplay: None,
            xdisplays: HashMap::new(),
            dmabuf_global: None,
            dmabuf_node: None,
            frame_sequence: 0,
            pending_swapchain_commits: Vec::new(),
            pressed_keysyms: Vec::new(),
            pressed_evdev_keys: HashSet::new(),
            intercepted_vt_keys: HashSet::new(),
            ime_reset_timer: None,
            x11_windows: Vec::new(),
            x11_window_metadata: HashMap::new(),
            x11_window_sequences: HashMap::new(),
            xwayland_server_ids: HashMap::new(),
            steam_ready_servers: HashSet::new(),
            steam_worker: SteamBridgeWorker::spawn(),
            pending_steam_events: Vec::new(),
            focus_control: FocusControl::default(),
            steam_mode,
            games_running: 0,
            steam_max_height: 0,
            input_counter: 0,
            fps_limit: 0,
            limiter_file: None,
            overscan_scale: 1.0,
            zoom_scale: 1.0,
            next_window_sequence: 1,
            last_x11_focus: None,
            content_overrides: HashMap::new(),
            content_override_parents: HashMap::new(),
            content_override_pending: HashSet::new(),
            loop_handle: None,
            vt_switching: false,
            pending_vt: None,
        }
    }

    /// Advertise renderer-supported dma-buf formats after backend creation.
    pub fn init_dmabuf(
        &mut self,
        handle: &DisplayHandle,
        formats: impl IntoIterator<Item = Format>,
    ) {
        if self.dmabuf_global.is_none() {
            self.dmabuf_global = Some(self.dmabuf_state.create_global::<Self>(handle, formats));
        }
    }

    /// Advertise dma-buf v4 with device and tranche feedback.
    pub fn init_dmabuf_feedback(&mut self, handle: &DisplayHandle, feedback: &DmabufFeedback) {
        if self.dmabuf_global.is_none() {
            self.dmabuf_global = Some(
                self.dmabuf_state
                    .create_global_with_default_feedback::<Self>(handle, feedback),
            );
        }
    }

    /// Replace dma-buf v4 feedback after a DRM connector/GPU change.
    pub fn update_dmabuf_feedback(&mut self, feedback: &DmabufFeedback) {
        if let Some(global) = self.dmabuf_global.as_ref() {
            self.dmabuf_state.set_default_feedback(global, feedback);
        }
    }

    /// Tag imported dma-bufs with the render node advertised in feedback.
    ///
    /// The DRM framebuffer exporter uses this hint to reject buffers from a
    /// different GPU. The linux-dmabuf protocol itself does not carry a node,
    /// so the compositor must supply the device selected for rendering.
    pub fn set_dmabuf_node(&mut self, node: DrmNode) {
        self.dmabuf_node = Some(node);
    }

    /// Attach the Wayland event-loop handle used by asynchronous transaction
    /// blockers such as linux-drm-syncobj acquire points.
    pub fn set_loop_handle(&mut self, handle: LoopHandle<'static, Self>) {
        self.loop_handle = Some(handle);
    }

    /// Enable Ctrl+Alt+F1…F12 handling for a libseat-owned hardware session.
    pub fn enable_vt_switching(&mut self) {
        self.vt_switching = true;
    }

    pub fn take_vt_switch(&mut self) -> Option<i32> {
        self.pending_vt.take()
    }

    /// Release compositor keyboard state before libinput is suspended for a
    /// VT switch, preventing Ctrl/Alt from remaining held on resume.
    pub fn release_all_keys(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        for keycode in keyboard.pressed_keys() {
            keyboard.input::<(), _>(
                self,
                keycode,
                KeyState::Released,
                SERIAL_COUNTER.next_serial(),
                0,
                |_, _, _| FilterResult::Forward,
            );
        }
        self.pressed_keysyms.clear();
        self.pressed_evdev_keys.clear();
        self.intercepted_vt_keys.clear();
    }

    /// Detect the kernel's layout-independent Ctrl+Alt+Fn chord before XKB
    /// transforms it. Smithay keycodes use the XKB offset of eight.
    pub fn filter_vt_keycode(
        &mut self,
        keycode: smithay::input::keyboard::Keycode,
        state: KeyState,
    ) -> bool {
        if !self.vt_switching {
            return false;
        }
        let evdev_key = u32::from(keycode).saturating_sub(8);
        match state {
            KeyState::Pressed => {
                self.pressed_evdev_keys.insert(evdev_key);
                if let Some(vt) = vt_from_pressed_evdev_keys(&self.pressed_evdev_keys) {
                    self.pending_vt = Some(vt);
                    self.intercepted_vt_keys.insert(evdev_key);
                    true
                } else {
                    false
                }
            }
            KeyState::Released => {
                self.pressed_evdev_keys.remove(&evdev_key);
                self.intercepted_vt_keys.remove(&evdev_key)
            }
        }
    }

    /// Advertise linux-drm-syncobj when the active KMS device supports timeline
    /// eventfd waits.
    pub fn init_drm_syncobj(
        &mut self,
        handle: &DisplayHandle,
        device: smithay::backend::drm::DrmDeviceFd,
    ) {
        self.drm_syncobj_state = Some(DrmSyncobjState::new::<Self>(handle, device));
    }

    /// Attach a ready Xwayland/XWM pair and initialize Steam's X11 contract.
    pub fn register_xwayland(
        &mut self,
        xwm: X11Wm,
        server_id: u32,
        display_number: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let xwm_id = xwm.id();
        self.xwms.insert(xwm_id, xwm);
        self.xwayland_server_ids.insert(xwm_id, server_id);
        self.xdisplays.insert(server_id, display_number);
        if server_id == 0 {
            self.xdisplay = Some(display_number);
        }

        let refresh = self
            .output
            .current_mode()
            .map_or(60_000, |mode| mode.refresh);
        self.steam_worker
            .register(display_number, server_id, refresh);
        self.refresh_focus(SERIAL_COUNTER.next_serial());
        Ok(())
    }

    /// Remove all compositor state belonging to a dynamic Xwayland server.
    pub fn remove_xwayland(&mut self, server_id: u32) -> bool {
        let Some(xwm_id) = self
            .xwayland_server_ids
            .iter()
            .find_map(|(xwm_id, id)| (*id == server_id).then_some(*xwm_id))
        else {
            return false;
        };
        let focused_belongs_to_server = self.keyboard_focus.as_ref().is_some_and(|focus| {
            matches!(
                focus,
                KeyboardFocusTarget::X11(window) if window.xwm_id() == Some(xwm_id)
            )
        });
        if focused_belongs_to_server {
            self.set_keyboard_focus(None, SERIAL_COUNTER.next_serial());
        }
        self.x11_windows
            .retain(|window| window.xwm_id() != Some(xwm_id));
        self.x11_window_metadata
            .retain(|(candidate, _), _| *candidate != xwm_id);
        self.x11_window_sequences
            .retain(|(candidate, _), _| *candidate != xwm_id);
        self.content_overrides
            .retain(|(candidate, _), _| *candidate != server_id);
        self.content_override_parents
            .retain(|(candidate, _), _| *candidate != server_id);
        self.content_override_pending
            .retain(|(candidate, _)| *candidate != server_id);
        self.steam_worker.remove(server_id);
        self.steam_ready_servers.remove(&server_id);
        self.xwayland_server_ids.remove(&xwm_id);
        self.xdisplays.remove(&server_id);
        self.xwms.remove(&xwm_id);
        if self
            .last_x11_focus
            .is_some_and(|(candidate, _)| candidate == xwm_id)
        {
            self.last_x11_focus = None;
        }
        true
    }

    /// Number of Xwayland instances that reached the ready state.
    #[must_use]
    pub fn ready_xwayland_count(&self) -> usize {
        if self.steam_mode {
            self.steam_ready_servers.len()
        } else {
            self.xdisplays.len()
        }
    }

    /// Publish the C++-compatible response for dynamic Xwayland creation.
    pub fn publish_xwayland_create_feedback(
        &self,
        identifier: u32,
        server_id: u32,
        display_number: u32,
    ) {
        self.steam_worker.publish_create_feedback(
            identifier,
            server_id,
            format!(":{display_number}"),
        );
    }

    /// Move the Steam worker's wakeable event receiver into the compositor loop.
    pub fn take_steam_event_source(&mut self) -> Option<Channel<TimedSteamWorkerEvent>> {
        self.steam_worker.take_event_source()
    }

    /// Queue one worker event delivered by the compositor event loop.
    pub fn queue_steam_event(&mut self, event: TimedSteamWorkerEvent) {
        self.pending_steam_events.push(event);
    }

    /// Process queued Steam X11 events and return process-lifecycle work.
    pub fn process_steam_events(&mut self, serial: Serial) -> Vec<SteamRuntimeRequest> {
        let mut requests = Vec::new();
        let mut focus_dirty = false;
        for (queued_at, event) in std::mem::take(&mut self.pending_steam_events) {
            event.trace_delivery_kind();
            perfetto_sdk::scoped_track_event!(
                "gamescope.xwm",
                "Steam worker event delivery",
                |ctx: &mut EventContext| {
                    perfetto::add_event_fields(
                        ctx,
                        &[perfetto::EventField::WorkerToFrontendNs(
                            perfetto::duration_ns(queued_at.elapsed()),
                        )],
                    );
                }
            );
            match event {
                SteamWorkerEvent::Ready { server_id, initial } => {
                    self.steam_ready_servers.insert(server_id);
                    if server_id == 0 {
                        self.focus_control = initial.focus_control;
                        self.games_running = initial.games_running;
                        self.steam_max_height = initial.steam_max_height;
                        self.fps_limit = initial.fps_limit;
                        self.overscan_scale = initial.screen_scale;
                        self.zoom_scale = initial.screen_magnification;
                        self.update_limiter_file();
                        requests.push(SteamRuntimeRequest::SetForceInternal {
                            force: initial.force_internal,
                        });
                        requests.push(SteamRuntimeRequest::SetCompositeForce {
                            force: initial.composite_force,
                        });
                    }
                    focus_dirty = true;
                }
                SteamWorkerEvent::WindowMetadata {
                    server_id,
                    window,
                    metadata,
                } => {
                    if let Some(xwm_id) =
                        self.xwayland_server_ids
                            .iter()
                            .find_map(|(xwm_id, candidate)| {
                                (*candidate == server_id).then_some(*xwm_id)
                            })
                    {
                        self.x11_window_metadata.insert((xwm_id, window), metadata);
                        focus_dirty = true;
                    }
                }
                SteamWorkerEvent::WindowAncestors {
                    server_id,
                    window,
                    ancestors,
                } => {
                    let key = (server_id, window);
                    self.content_override_pending.remove(&key);
                    let managed = self
                        .x11_windows
                        .iter()
                        .filter(|candidate| self.server_id_for(candidate) == server_id)
                        .map(X11Surface::window_id)
                        .collect::<Vec<_>>();
                    if let Some(parent) = select_managed_ancestor(&ancestors, &managed) {
                        self.content_override_parents.insert(key, parent);
                        if parent != window
                            && let Some(surface) = self.content_overrides.remove(&key)
                        {
                            self.content_overrides.insert((server_id, parent), surface);
                        }
                        tracing::debug!(
                            server_id,
                            drawable = window,
                            toplevel = parent,
                            "resolved Gamescope WSI content override"
                        );
                    }
                }
                SteamWorkerEvent::FocusControl {
                    server_id: 0,
                    control,
                } => {
                    self.focus_control = control;
                    focus_dirty = true;
                }
                SteamWorkerEvent::ScreenScale {
                    server_id: 0,
                    scale,
                    magnification,
                } => {
                    self.overscan_scale = scale;
                    self.zoom_scale = magnification;
                }
                SteamWorkerEvent::Event {
                    server_id: 0,
                    event,
                } => match event {
                    BridgeEvent::CreateXwayland(identifier) => {
                        requests.push(SteamRuntimeRequest::CreateXwayland { identifier });
                    }
                    BridgeEvent::DestroyXwayland(server_id) => {
                        requests.push(SteamRuntimeRequest::DestroyXwayland { server_id });
                    }
                    BridgeEvent::GamesRunningChanged(count) => self.games_running = count,
                    BridgeEvent::SteamMaxHeightChanged(height) => {
                        self.steam_max_height = height;
                        focus_dirty = true;
                    }
                    BridgeEvent::FpsLimitChanged(limit) => {
                        self.fps_limit = limit;
                        self.update_limiter_file();
                    }
                    BridgeEvent::VrrEnabledChanged(enabled) => {
                        requests.push(SteamRuntimeRequest::SetVrr { enabled });
                    }
                    BridgeEvent::DisplayForceInternalChanged(force) => {
                        requests.push(SteamRuntimeRequest::SetForceInternal { force });
                    }
                    BridgeEvent::DisplayModeNudge => {
                        requests.push(SteamRuntimeRequest::RescanDisplay);
                    }
                    BridgeEvent::DynamicRefreshChanged { screen, refresh_hz } => {
                        requests
                            .push(SteamRuntimeRequest::SetDynamicRefresh { screen, refresh_hz });
                    }
                    BridgeEvent::CompositeForceChanged(force) => {
                        requests.push(SteamRuntimeRequest::SetCompositeForce { force });
                    }
                    _ => {}
                },
                SteamWorkerEvent::Error { server_id, message } => {
                    if let Some(server_id) = server_id {
                        self.steam_ready_servers.insert(server_id);
                    }
                    tracing::warn!(?server_id, %message, "Steam X11 policy worker error");
                }
                _ => {}
            }
        }
        if focus_dirty {
            self.refresh_focus(serial);
        }
        requests
    }

    /// Increment Steam's root-window activity counter after physical input.
    pub fn bump_input_counter(&mut self) {
        self.input_counter = self.input_counter.wrapping_add(1);
        self.steam_worker.publish_input_counter(self.input_counter);
    }

    /// Connect Steam's FPS-limit root property to the reused WSI layer.
    pub fn set_limiter_file(&mut self, path: PathBuf) {
        self.limiter_file = Some(path);
        self.update_limiter_file();
    }

    /// Publish current hardware VRR capability and usage to Steam's root
    /// compatibility properties without blocking the Wayland thread.
    pub fn publish_hardware_vrr(&self, capable: bool, in_use: bool) {
        self.steam_worker.publish_vrr(capable, in_use);
    }

    /// Publish the DRM primary-plane decision for runtime verification.
    pub fn publish_direct_scanout_status(&self, status: u32) {
        self.steam_worker.publish_direct_scanout_status(status);
    }

    /// Publish physical connector information to all gamescope-control clients.
    pub fn publish_active_display(&mut self, display: ActiveDisplayInfo) {
        self.gamescope_state.set_active_display(Some(display));
    }

    pub fn publish_screenshot_taken(&mut self, path: impl Into<String>) {
        self.gamescope_state.screenshot_taken(path);
    }

    /// Keep the Wayland presentation rate and Steam's X11 feedback property in
    /// sync with a physical mode switch.
    pub fn publish_output_refresh(&mut self, refresh_millihz: i32) {
        if let Some(current) = self.output.current_mode() {
            let mode = OutputMode {
                size: current.size,
                refresh: refresh_millihz,
            };
            self.output
                .change_current_state(Some(mode), None, None, None);
            self.output.set_preferred(mode);
        }
        self.steam_worker.publish_refresh(refresh_millihz);
    }

    /// Keep the logical Wayland/Xwayland mode synchronized with an automatic
    /// physical output selection or hotplug.
    pub fn publish_output_mode(&mut self, config: &OutputConfig) {
        let mode = config.mode();
        self.output
            .change_current_state(Some(mode), None, None, None);
        self.output.set_preferred(mode);
        self.steam_worker.publish_refresh(config.refresh_millihz);
    }

    fn update_limiter_file(&self) {
        let Some(path) = self.limiter_file.as_ref() else {
            return;
        };
        let enabled = u32::from(self.fps_limit != 0);
        if let Err(error) = std::fs::write(path, enabled.to_ne_bytes()) {
            tracing::warn!(%error, path = %path.display(), "failed to update WSI limiter file");
        }
    }

    /// Steam's fixed-point overscan and magnification product.
    #[must_use]
    pub fn global_scale_ratio(&self) -> f64 {
        (self.overscan_scale * self.zoom_scale).clamp(f64::EPSILON, 1.0)
    }

    /// Map host-output coordinates back through Steam's centered global scale.
    #[must_use]
    pub fn transform_pointer_for_global_scale(&self, location: (f64, f64)) -> (f64, f64) {
        let Some(mode) = self.output.current_mode() else {
            return location;
        };
        let scale = self.global_scale_ratio();
        let offset_x = f64::from(mode.size.w) * (1.0 - scale) / 2.0;
        let offset_y = f64::from(mode.size.h) * (1.0 - scale) / 2.0;
        (
            (location.0 - offset_x) / scale,
            (location.1 - offset_y) / scale,
        )
    }

    fn track_x11_window(&mut self, window: &X11Surface) {
        let Some(xwm_id) = window.xwm_id() else {
            return;
        };
        let key = (xwm_id, window.window_id());
        if !self.x11_window_sequences.contains_key(&key) {
            self.x11_window_sequences
                .insert(key, self.next_window_sequence);
            self.next_window_sequence = self.next_window_sequence.wrapping_add(1);
        }
        self.steam_worker.watch_window(
            self.server_id_for(window),
            window.window_id(),
            window.pid(),
        );
        self.refresh_x11_metadata(xwm_id, window.window_id());
    }

    fn remember_x11_window(&mut self, window: &X11Surface) {
        let Some(xwm_id) = window.xwm_id() else {
            return;
        };
        let window_id = window.window_id();

        // X11 resource IDs may be reused after DestroyNotify. Smithay marks a
        // destroyed X11Surface dead before calling destroyed_window, and its
        // PartialEq deliberately reports dead surfaces as unequal even to
        // themselves. Compare the stable identity here so an old wrapper can
        // never shadow a newly-created window with the same XID.
        self.x11_windows.retain(|candidate| {
            !x11_window_identity_matches(
                candidate.xwm_id(),
                candidate.window_id(),
                xwm_id,
                window_id,
            ) || candidate.alive()
        });
        if !self.x11_windows.iter().any(|candidate| {
            x11_window_identity_matches(
                candidate.xwm_id(),
                candidate.window_id(),
                xwm_id,
                window_id,
            )
        }) {
            self.x11_windows.push(window.clone());
        }
    }

    fn refresh_x11_metadata(&mut self, xwm_id: XwmId, window_id: u32) {
        let pid = self
            .x11_windows
            .iter()
            .find(|window| window.xwm_id() == Some(xwm_id) && window.window_id() == window_id)
            .and_then(X11Surface::pid);
        let Some(server_id) = self.xwayland_server_ids.get(&xwm_id).copied() else {
            return;
        };
        self.steam_worker.read_window(server_id, window_id, pid);
    }

    fn metadata_for(&self, window: &X11Surface) -> WindowMetadata {
        window
            .xwm_id()
            .and_then(|xwm_id| self.x11_window_metadata.get(&(xwm_id, window.window_id())))
            .cloned()
            .unwrap_or_default()
    }

    fn server_id_for(&self, window: &X11Surface) -> u32 {
        window
            .xwm_id()
            .and_then(|xwm_id| self.xwayland_server_ids.get(&xwm_id).copied())
            .unwrap_or(0)
    }

    fn focus_candidate(&self, window: &X11Surface) -> Option<FocusCandidate> {
        if !window.alive() {
            return None;
        }
        let xwm_id = window.xwm_id()?;
        let metadata = self.metadata_for(window);
        if !metadata.is_focus_candidate() {
            return None;
        }
        let geometry = window.geometry();
        Some(FocusCandidate {
            server_id: self.server_id_for(window),
            window_id: window.window_id(),
            app_id: metadata.effective_app_id(self.steam_mode, window.window_id()),
            mapped: window.is_mapped(),
            override_redirect: window.is_override_redirect(),
            transient_for: window.is_transient_for(),
            width: geometry.size.w,
            height: geometry.size.h,
            sequence: self
                .x11_window_sequences
                .get(&(xwm_id, window.window_id()))
                .copied()
                .unwrap_or(0),
        })
    }

    fn selected_x11_window(&self) -> Option<X11Surface> {
        let candidates = self
            .x11_windows
            .iter()
            .filter_map(|window| self.focus_candidate(window))
            .collect::<Vec<_>>();
        let selected = select_focus(&candidates, &self.focus_control)?;
        self.x11_windows
            .iter()
            .find(|window| {
                window.alive()
                    && self.server_id_for(window) == selected.server_id
                    && window.window_id() == selected.window_id
            })
            .cloned()
    }

    fn base_render_window(&self) -> Option<X11Surface> {
        let focus = self.selected_x11_window()?;
        if !self.metadata_for(&focus).streaming_client {
            return Some(focus);
        }
        let server_id = self.server_id_for(&focus);
        self.x11_windows
            .iter()
            .filter(|window| {
                window.is_mapped()
                    && self.server_id_for(window) == server_id
                    && self.metadata_for(window).streaming_client_video
            })
            .max_by_key(|window| {
                window
                    .xwm_id()
                    .and_then(|id| self.x11_window_sequences.get(&(id, window.window_id())))
                    .copied()
                    .unwrap_or(0)
            })
            .cloned()
            .or(Some(focus))
    }

    fn selected_override(&self, focus: &X11Surface) -> Option<X11Surface> {
        let focus_candidate = self.focus_candidate(focus)?;
        let candidates = self
            .x11_windows
            .iter()
            .filter_map(|window| self.focus_candidate(window))
            .collect::<Vec<_>>();
        let selected = select_override(focus_candidate, &candidates)?;
        self.x11_windows
            .iter()
            .find(|window| {
                window.alive()
                    && self.server_id_for(window) == selected.server_id
                    && window.window_id() == selected.window_id
            })
            .cloned()
    }

    /// Surface carrying pixels for an X11 window. Gamescope WSI may replace
    /// Xwayland's normally blank surface with a private Vulkan surface.
    fn render_surface_for_x11(&self, window: &X11Surface) -> Option<WlSurface> {
        self.content_overrides
            .get(&(self.server_id_for(window), window.window_id()))
            .cloned()
            .or_else(|| window.wl_surface())
    }

    /// Surface carrying input for an X11 window. Content overrides explicitly
    /// affect buffer submission only; Xwayland must retain keyboard, pointer,
    /// touch, gesture, and cursor ownership.
    fn input_surface_for_x11(window: &X11Surface) -> Option<WlSurface> {
        window.wl_surface()
    }

    /// The surface Gamescope should composite as the primary game layer.
    #[must_use]
    pub fn primary_surface(&self) -> Option<WlSurface> {
        self.base_render_window()
            .and_then(|window| self.render_surface_for_x11(&window))
            .or_else(|| self.focused_surface.clone())
            .or_else(|| {
                self.xdg_shell_state
                    .toplevel_surfaces()
                    .iter()
                    .next_back()
                    .map(|surface| surface.wl_surface().clone())
            })
    }

    /// All Steam layers selected for the current frame, bottom to top.
    #[must_use]
    pub fn render_layers(&self) -> Vec<RenderLayer> {
        let Some(focus) = self.selected_x11_window() else {
            return self
                .primary_surface()
                .map(|surface| {
                    vec![RenderLayer {
                        surface,
                        alpha: 1.0,
                        force_blend: false,
                    }]
                })
                .unwrap_or_default();
        };
        let mut layers = Vec::new();
        let render_base = self.base_render_window().unwrap_or_else(|| focus.clone());
        if let Some(surface) = self.render_surface_for_x11(&render_base) {
            layers.push(RenderLayer {
                surface,
                alpha: 1.0,
                force_blend: false,
            });
        }
        if let Some(override_window) = self.selected_override(&focus)
            && let Some(surface) = self.render_surface_for_x11(&override_window)
        {
            layers.push(RenderLayer {
                surface,
                alpha: 1.0,
                force_blend: true,
            });
        }

        let primary_overlay = self
            .x11_windows
            .iter()
            .filter(|window| {
                window.is_mapped()
                    && window.geometry().size.w > 1200
                    && self.metadata_for(window).overlay
            })
            .max_by_key(|window| self.metadata_for(window).opacity);
        let notification = self
            .x11_windows
            .iter()
            .filter(|window| {
                window.is_mapped()
                    && self.metadata_for(window).overlay
                    && primary_overlay != Some(window)
            })
            .max_by_key(|window| {
                window
                    .xwm_id()
                    .and_then(|id| self.x11_window_sequences.get(&(id, window.window_id())))
                    .copied()
                    .unwrap_or(0)
            });
        for window in [notification, primary_overlay].into_iter().flatten() {
            let metadata = self.metadata_for(window);
            if metadata.opacity != 0
                && let Some(surface) = self.render_surface_for_x11(window)
            {
                layers.push(RenderLayer {
                    surface,
                    alpha: metadata.alpha(),
                    force_blend: true,
                });
            }
        }

        let external = self
            .x11_windows
            .iter()
            .filter(|window| window.is_mapped() && self.metadata_for(window).external_overlay)
            .max_by_key(|window| self.metadata_for(window).opacity);
        if let Some(window) = external
            && self.metadata_for(window).opacity != 0
            && let Some(surface) = self.render_surface_for_x11(window)
        {
            layers.push(RenderLayer {
                surface,
                alpha: self.metadata_for(window).alpha(),
                force_blend: true,
            });
        }
        layers
    }

    /// Current client cursor, if the focused client supplied a cursor surface.
    #[must_use]
    pub fn cursor_layer(&self) -> Option<CursorLayer> {
        let CursorImageStatus::Surface(surface) = &self.cursor_status else {
            return None;
        };
        let hotspot = with_states(surface, |states| {
            states
                .data_map
                .get::<Mutex<CursorImageAttributes>>()
                .and_then(|attributes| attributes.lock().ok().map(|attributes| attributes.hotspot))
                .unwrap_or_default()
        });
        Some(CursorLayer {
            surface: surface.clone(),
            location: (
                self.pointer_location.x - f64::from(hotspot.x),
                self.pointer_location.y - f64::from(hotspot.y),
            )
                .into(),
        })
    }

    fn input_overlay(&self) -> Option<X11Surface> {
        self.x11_windows
            .iter()
            .filter(|window| {
                let metadata = self.metadata_for(window);
                window.is_mapped()
                    && window.geometry().size.w > 1200
                    && metadata.overlay
                    && metadata.input_focus_mode != 0
            })
            .max_by_key(|window| self.metadata_for(window).opacity)
            .cloned()
    }

    fn keyboard_x11_window(&self) -> Option<X11Surface> {
        let focus = self.selected_x11_window()?;
        if let Some(overlay) = self.input_overlay()
            && self.metadata_for(&overlay).input_focus_mode != 2
        {
            return Some(overlay);
        }
        self.selected_override(&focus).or(Some(focus))
    }

    fn pointer_focus_surface(&self) -> Option<WlSurface> {
        if let Some(overlay) = self.input_overlay() {
            return Self::input_surface_for_x11(&overlay);
        }
        let focus = self.selected_x11_window()?;
        self.selected_override(&focus)
            .and_then(|window| Self::input_surface_for_x11(&window))
            .or_else(|| Self::input_surface_for_x11(&focus))
    }

    fn primary_input_surface(&self) -> Option<WlSurface> {
        if let Some(focus) = self.selected_x11_window() {
            return Self::input_surface_for_x11(&focus);
        }
        self.focused_surface.clone().or_else(|| {
            self.xdg_shell_state
                .toplevel_surfaces()
                .iter()
                .next_back()
                .map(|surface| surface.wl_surface().clone())
        })
    }

    /// Re-evaluate Steam's base/overlay policy and apply X11 and Wayland focus.
    pub fn refresh_focus(&mut self, serial: Serial) {
        let next = self
            .keyboard_x11_window()
            .map(KeyboardFocusTarget::X11)
            .or_else(|| {
                self.primary_input_surface()
                    .map(KeyboardFocusTarget::Wayland)
            });
        self.set_keyboard_focus(next, serial);

        // Smithay clears Xwayland's core focus while leaving the previous X11
        // target. Globally-active clients only receive WM_TAKE_FOCUS on enter,
        // so they do not set core focus again themselves. Queue Gamescope's
        // explicit XSetInputFocus after the leave/enter transition to ensure
        // the worker's request is the final focus operation.
        self.apply_x11_focus();
        self.publish_steam_focus();
    }

    fn apply_x11_focus(&mut self) {
        let keyboard_window = self.keyboard_x11_window();
        let next_key = keyboard_window
            .as_ref()
            .and_then(|window| Some((window.xwm_id()?, window.window_id())));
        if let Some(focus) = self.selected_x11_window()
            && let Some(mode) = self.output.current_mode()
        {
            let mut size = (mode.size.w, mode.size.h);
            if self
                .metadata_for(&focus)
                .effective_app_id(self.steam_mode, focus.window_id())
                == steam::STEAM_APP_ID
                && self.steam_max_height != 0
                && u32::try_from(mode.size.h).is_ok_and(|height| height > self.steam_max_height)
            {
                let scale = f64::from(self.steam_max_height) / f64::from(mode.size.h);
                size = (
                    (f64::from(mode.size.w) * scale).round() as i32,
                    i32::try_from(self.steam_max_height).unwrap_or(i32::MAX),
                );
            }
            if focus.geometry().size != size.into() {
                let _ = focus.configure(smithay::utils::Rectangle::from_size(size.into()));
            }
        }

        if next_key == self.last_x11_focus {
            return;
        }

        // Upstream Gamescope explicitly applies XSetInputFocus, including for
        // the globally-active ICCCM model used by Wine/Unity. Do so only when
        // the target changes: refresh_focus also runs for pointer motion, and
        // repeatedly focusing the same window produces an X FocusOut/FocusIn
        // storm. Intentional ICCCM re-entry invalidates last_x11_focus below.
        for (xwm_id, server_id) in &self.xwayland_server_ids {
            let target = next_key
                .filter(|(target_xwm, _)| target_xwm == xwm_id)
                .map(|(_, window)| window);
            self.steam_worker.set_input_focus(*server_id, target);
        }

        for window in &self.x11_windows {
            let active = next_key.is_some_and(|(xwm_id, id)| {
                window.xwm_id() == Some(xwm_id) && window.window_id() == id
            });
            if let Err(error) = window.set_activated(active) {
                tracing::warn!(%error, window = window.window_id(), "failed to update X11 activation");
            }
        }

        if let Some(window) = keyboard_window
            && let Some(xwm_id) = window.xwm_id()
            && let Some(xwm) = self.xwms.get_mut(&xwm_id)
            && let Err(error) = xwm.raise_window(&window)
        {
            tracing::warn!(%error, window = window.window_id(), "failed to raise focused X11 window");
        }
        self.last_x11_focus = next_key;
    }

    fn publish_steam_focus(&self) {
        let mut focusable_apps = Vec::new();
        let mut seen_apps = std::collections::HashSet::new();
        let mut focusable_windows = Vec::new();
        for window in &self.x11_windows {
            let Some(candidate) = self.focus_candidate(window) else {
                continue;
            };
            if !candidate.useful() || candidate.override_redirect {
                continue;
            }
            if candidate.app_id != 0 && seen_apps.insert(candidate.app_id) {
                focusable_apps.push(candidate.app_id);
            }
            focusable_windows.extend([
                candidate.window_id,
                candidate.app_id,
                self.metadata_for(window).pid,
            ]);
        }

        let focus = self.selected_x11_window();
        let focused_window = focus.as_ref().map(X11Surface::window_id);
        let focused_app = focus.as_ref().map(|window| {
            self.metadata_for(window)
                .effective_app_id(self.steam_mode, window.window_id())
        });
        let Some(root_display) = self.xdisplays.get(&0) else {
            return;
        };
        let focus_display = focus
            .as_ref()
            .and_then(X11Surface::xwm_id)
            .and_then(|xwm_id| self.xwayland_server_ids.get(&xwm_id))
            .and_then(|server_id| self.xdisplays.get(server_id))
            .unwrap_or(root_display);
        self.steam_worker.publish_focus(
            focusable_apps,
            focusable_windows,
            focused_window,
            focused_app.filter(|app_id| *app_id != 0),
            focused_app.filter(|app_id| *app_id != 0),
            format!(":{focus_display}"),
            self.steam_mode,
        );
    }

    fn set_keyboard_focus(&mut self, next: Option<KeyboardFocusTarget>, serial: Serial) {
        if next != self.keyboard_focus {
            if let Some(keyboard) = self.seat.get_keyboard() {
                keyboard.set_focus(self, next, serial);
            }
        }
    }

    fn keyboard_focus_is_x11_window(&self, xwm_id: XwmId, window_id: u32) -> bool {
        self.keyboard_focus.as_ref().is_some_and(|focus| {
            matches!(
                focus,
                KeyboardFocusTarget::X11(window)
                    if x11_window_identity_matches(
                        window.xwm_id(),
                        window.window_id(),
                        xwm_id,
                        window_id,
                    )
            )
        })
    }

    /// Focus a native Wayland surface only when Steam's X11 policy has no
    /// target. If the surface already belongs to a tracked X11 window, retain
    /// the wrapper that implements the client's ICCCM input model.
    fn focus_wayland_surface(&mut self, surface: WlSurface, serial: Serial) {
        if let Some(window) = self
            .x11_windows
            .iter()
            .find(|window| window.wl_surface().as_ref() == Some(&surface))
            .cloned()
        {
            self.set_keyboard_focus(Some(KeyboardFocusTarget::X11(window)), serial);
        } else if self.keyboard_x11_window().is_none() {
            self.set_keyboard_focus(Some(KeyboardFocusTarget::Wayland(surface)), serial);
        }
    }

    /// Repair a focus target that was replaced by a late Wayland shell event.
    /// Called at the physical-input boundary so the triggering key itself is
    /// delivered to the policy-selected X11 window.
    pub fn repair_keyboard_focus(&mut self, serial: Serial) -> bool {
        let Some(window) = self.keyboard_x11_window() else {
            return false;
        };
        let next = KeyboardFocusTarget::X11(window);
        if self.keyboard_focus.as_ref() == Some(&next) {
            return false;
        }
        self.set_keyboard_focus(Some(next), serial);
        self.apply_x11_focus();
        true
    }

    fn clear_early_x11_focus(&mut self, window: &X11Surface) {
        if window
            .xwm_id()
            .is_some_and(|xwm_id| self.keyboard_focus_is_x11_window(xwm_id, window.window_id()))
        {
            self.set_keyboard_focus(None, SERIAL_COUNTER.next_serial());
            // The selected X11 window itself may be unchanged, but Smithay's
            // leave cleared Xwayland's core focus. Force apply_x11_focus to
            // restore it after the matching enter transition.
            self.last_x11_focus = None;
        }
    }

    /// Complete callbacks and presentation feedback after a backend presents.
    pub fn presented(&mut self, at: Duration) {
        let refresh = self
            .output
            .current_mode()
            .and_then(|mode| u64::try_from(mode.refresh).ok())
            .filter(|refresh| *refresh != 0)
            .map_or(Duration::from_nanos(16_666_667), |refresh| {
                Duration::from_nanos(1_000_000_000_000 / refresh)
            });
        let sequence = self.frame_sequence.wrapping_add(1);
        self.presented_with_metadata(at, refresh, sequence, wp_presentation_feedback::Kind::Vsync);
    }

    /// Complete callbacks using the backend's actual refresh, sequence, and
    /// completion guarantees.
    pub fn presented_with_metadata(
        &mut self,
        at: Duration,
        refresh: Duration,
        sequence: u64,
        kind: wp_presentation_feedback::Kind,
    ) {
        let sequence = if sequence == 0 {
            self.frame_sequence.wrapping_add(1)
        } else {
            sequence
        };
        self.frame_sequence = sequence;

        let mut surfaces = self
            .render_layers()
            .into_iter()
            .map(|layer| layer.surface)
            .collect::<Vec<_>>();
        if let Some(cursor) = self.cursor_layer() {
            surfaces.push(cursor.surface);
        }
        surfaces.dedup();
        for surface in &surfaces {
            with_surface_tree_downward(
                surface,
                (),
                |_, _, &()| TraversalAction::DoChildren(()),
                |_surface, states, &()| {
                    for callback in states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .frame_callbacks
                        .drain(..)
                    {
                        callback.done(u32::try_from(at.as_millis()).unwrap_or(u32::MAX));
                    }
                    let feedback = std::mem::take(
                        &mut states
                            .cached_state
                            .get::<PresentationFeedbackCachedState>()
                            .current()
                            .callbacks,
                    );
                    for feedback in feedback {
                        feedback.presented(
                            &self.output,
                            at,
                            Refresh::fixed(refresh),
                            sequence,
                            kind,
                        );
                    }
                },
                |_, _, &()| true,
            );
        }

        let Some(surface) = self.primary_surface() else {
            return;
        };
        if let Some(index) = self
            .pending_swapchain_commits
            .iter()
            .rposition(|(pending_surface, _)| pending_surface == &surface)
        {
            let (_, commit) = self.pending_swapchain_commits.remove(index);
            let present_time_ns = u64::try_from(at.as_nanos()).unwrap_or(u64::MAX);
            let refresh_cycle_ns = u64::try_from(refresh.as_nanos()).unwrap_or(u64::MAX);
            self.gamescope_state.surface_presented(
                &surface,
                &commit,
                present_time_ns,
                present_time_ns,
                0,
                refresh_cycle_ns,
            );
        }
    }

    /// Forward an absolute pointer position to the selected game surface.
    pub fn pointer_motion(&mut self, location: Point<f64, Logical>, serial: Serial, time: u32) {
        let Some(mode) = self.output.current_mode() else {
            return;
        };
        self.pointer_location = (
            location
                .x
                .clamp(0.0, f64::from(mode.size.w.saturating_sub(1))),
            location
                .y
                .clamp(0.0, f64::from(mode.size.h.saturating_sub(1))),
        )
            .into();
        let focus = self
            .pointer_focus_surface()
            .or_else(|| self.primary_input_surface())
            .map(|surface| (surface, (0.0, 0.0).into()));
        let pointer = self.pointer.clone();
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: self.pointer_location,
                serial,
                time,
            },
        );
        pointer.frame(self);
    }

    /// Forward relative motion and update the compositor's absolute pointer
    /// location without introducing a second protocol frame.
    pub fn pointer_motion_relative(
        &mut self,
        delta: Point<f64, Logical>,
        delta_unaccel: Point<f64, Logical>,
        serial: Serial,
        time_msec: u32,
        time_usec: u64,
    ) {
        let Some(mode) = self.output.current_mode() else {
            return;
        };
        let focus = self
            .pointer_focus_surface()
            .or_else(|| self.primary_input_surface())
            .map(|surface| (surface, (0.0, 0.0).into()));
        let pointer = self.pointer.clone();
        pointer.relative_motion(
            self,
            focus.clone(),
            &RelativeMotionEvent {
                delta,
                delta_unaccel,
                utime: time_usec,
            },
        );
        self.pointer_location = (
            (self.pointer_location.x + delta.x)
                .clamp(0.0, f64::from(mode.size.w.saturating_sub(1))),
            (self.pointer_location.y + delta.y)
                .clamp(0.0, f64::from(mode.size.h.saturating_sub(1))),
        )
            .into();
        pointer.motion(
            self,
            focus,
            &MotionEvent {
                location: self.pointer_location,
                serial,
                time: time_msec,
            },
        );
        pointer.frame(self);
    }

    pub fn touch_down(
        &mut self,
        location: Point<f64, Logical>,
        slot: smithay::backend::input::TouchSlot,
        serial: Serial,
        time: u32,
    ) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let focus = self
            .pointer_focus_surface()
            .or_else(|| self.primary_input_surface())
            .map(|surface| (surface, (0.0, 0.0).into()));
        touch.down(
            self,
            focus,
            &TouchDownEvent {
                slot,
                location,
                serial,
                time,
            },
        );
    }

    pub fn touch_motion(
        &mut self,
        location: Point<f64, Logical>,
        slot: smithay::backend::input::TouchSlot,
        time: u32,
    ) {
        let Some(touch) = self.seat.get_touch() else {
            return;
        };
        let focus = self
            .pointer_focus_surface()
            .or_else(|| self.primary_input_surface())
            .map(|surface| (surface, (0.0, 0.0).into()));
        touch.motion(
            self,
            focus,
            &TouchMotionEvent {
                slot,
                location,
                time,
            },
        );
    }

    pub fn touch_up(
        &mut self,
        slot: smithay::backend::input::TouchSlot,
        serial: Serial,
        time: u32,
    ) {
        if let Some(touch) = self.seat.get_touch() {
            touch.up(self, &TouchUpEvent { slot, serial, time });
        }
    }

    pub fn touch_frame(&mut self) {
        if let Some(touch) = self.seat.get_touch() {
            touch.frame(self);
        }
    }

    pub fn touch_cancel(&mut self) {
        if let Some(touch) = self.seat.get_touch() {
            touch.cancel(self);
        }
    }

    pub fn gesture_swipe_begin(&mut self, fingers: u32, time: u32) {
        let pointer = self.pointer.clone();
        pointer.gesture_swipe_begin(
            self,
            &GestureSwipeBeginEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time,
                fingers,
            },
        );
    }

    pub fn gesture_swipe_update(&mut self, delta: Point<f64, Logical>, time: u32) {
        let pointer = self.pointer.clone();
        pointer.gesture_swipe_update(self, &GestureSwipeUpdateEvent { time, delta });
    }

    pub fn gesture_swipe_end(&mut self, cancelled: bool, time: u32) {
        let pointer = self.pointer.clone();
        pointer.gesture_swipe_end(
            self,
            &GestureSwipeEndEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time,
                cancelled,
            },
        );
    }

    pub fn gesture_pinch_begin(&mut self, fingers: u32, time: u32) {
        let pointer = self.pointer.clone();
        pointer.gesture_pinch_begin(
            self,
            &GesturePinchBeginEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time,
                fingers,
            },
        );
    }

    pub fn gesture_pinch_update(
        &mut self,
        delta: Point<f64, Logical>,
        scale: f64,
        rotation: f64,
        time: u32,
    ) {
        let pointer = self.pointer.clone();
        pointer.gesture_pinch_update(
            self,
            &GesturePinchUpdateEvent {
                time,
                delta,
                scale,
                rotation,
            },
        );
    }

    pub fn gesture_pinch_end(&mut self, cancelled: bool, time: u32) {
        let pointer = self.pointer.clone();
        pointer.gesture_pinch_end(
            self,
            &GesturePinchEndEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time,
                cancelled,
            },
        );
    }

    pub fn gesture_hold_begin(&mut self, fingers: u32, time: u32) {
        let pointer = self.pointer.clone();
        pointer.gesture_hold_begin(
            self,
            &GestureHoldBeginEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time,
                fingers,
            },
        );
    }

    pub fn gesture_hold_end(&mut self, cancelled: bool, time: u32) {
        let pointer = self.pointer.clone();
        pointer.gesture_hold_end(
            self,
            &GestureHoldEndEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time,
                cancelled,
            },
        );
    }

    /// Forward a Linux input button code to the selected game surface.
    pub fn pointer_button(&mut self, button: u32, pressed: bool, serial: Serial, time: u32) {
        let pointer = self.pointer.clone();
        pointer.button(
            self,
            &ButtonEvent {
                serial,
                time,
                button,
                state: if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                },
            },
        );
        pointer.frame(self);
    }

    /// Forward Gamescope's logical wheel units.
    pub fn pointer_wheel(&mut self, horizontal: f64, vertical: f64, time: u32) {
        let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
        if horizontal != 0.0 {
            frame = frame
                .value(Axis::Horizontal, horizontal * 15.0)
                .v120(Axis::Horizontal, wheel_v120(horizontal));
        }
        if vertical != 0.0 {
            frame = frame
                .value(Axis::Vertical, vertical * 15.0)
                .v120(Axis::Vertical, wheel_v120(vertical));
        }
        let pointer = self.pointer.clone();
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    /// Maintain the exact pressed-keysym set consumed by action bindings.
    pub fn filter_key(
        &mut self,
        state: KeyState,
        key: &KeysymHandle<'_>,
        monotonic_time_ns: u64,
    ) -> FilterResult<()> {
        let keysym = key.modified_sym().raw();
        match state {
            KeyState::Pressed if !self.pressed_keysyms.contains(&keysym) => {
                self.pressed_keysyms.push(keysym);
            }
            KeyState::Released => self.pressed_keysyms.retain(|pressed| *pressed != keysym),
            KeyState::Pressed => {}
        }
        let intercepted = self
            .gamescope_state
            .process_pressed_keysyms(self.pressed_keysyms.iter().copied(), monotonic_time_ns);
        perfetto_sdk::track_event_instant!(
            "gamescope.input",
            "Keyboard filter decision",
            |ctx: &mut EventContext| {
                perfetto::add_event_fields(
                    ctx,
                    &[
                        perfetto::EventField::Keysym(u64::from(keysym)),
                        perfetto::EventField::Pressed(state == KeyState::Pressed),
                        perfetto::EventField::Intercepted(intercepted),
                        perfetto::EventField::PressedKeys(self.pressed_keysyms.len() as u64),
                    ],
                );
            }
        );
        if intercepted {
            FilterResult::Intercept(())
        } else {
            FilterResult::Forward
        }
    }

    /// Execute protocol commands that belong to the compositor thread.
    pub fn process_gamescope_commands(&mut self, serial: &mut u32) -> Vec<Command> {
        let commands: Vec<_> = self.gamescope_state.drain_commands().collect();
        let mut backend_commands = Vec::new();
        for command in commands {
            match command {
                Command::InputMethod(command) => self.process_input_method(command, serial),
                Command::ExecutePrivate { reply, .. } => {
                    GamescopeState::private_command_executed(&reply);
                }
                Command::SetReshadeEffect { reply, path } => {
                    GamescopeState::reshade_effect_ready(&reply, path);
                }
                Command::OverrideWindowContent {
                    surface,
                    xwayland_server_id,
                    x11_window,
                } => {
                    let key = (xwayland_server_id, x11_window);
                    let target = self
                        .content_override_parents
                        .get(&key)
                        .copied()
                        .unwrap_or(x11_window);
                    self.content_overrides
                        .insert((xwayland_server_id, target), surface);
                    if target == x11_window && self.content_override_pending.insert(key) {
                        self.steam_worker
                            .resolve_window(xwayland_server_id, x11_window);
                    }
                }
                command => backend_commands.push(command),
            }
        }
        backend_commands
    }

    fn schedule_ime_reset(&mut self) {
        let Some(handle) = self.loop_handle.clone() else {
            return;
        };
        if let Some(token) = self.ime_reset_timer.take() {
            handle.remove(token);
        }
        self.ime_reset_timer = handle
            .insert_source(
                Timer::from_duration(Duration::from_millis(100)),
                |_, _, state| {
                    state.ime_reset_timer = None;
                    if let Some(keyboard) = state.seat.get_keyboard() {
                        let _ = keyboard.set_xkb_config(state, XkbConfig::default());
                    }
                    TimeoutAction::Drop
                },
            )
            .ok();
    }

    fn process_input_method(&mut self, command: InputMethodCommand, serial: &mut u32) {
        match command {
            InputMethodCommand::Commit(commit) => self.type_input_method_commit(commit, serial),
            InputMethodCommand::PointerMotion { dx, dy, time_msec } => {
                self.pointer_motion(
                    self.pointer_location + Point::from((dx, dy)),
                    next_serial(serial),
                    time_msec,
                );
            }
            InputMethodCommand::PointerWarp { x, y, time_msec } => {
                self.pointer_motion((x, y).into(), next_serial(serial), time_msec);
            }
            InputMethodCommand::PointerWheel {
                horizontal,
                vertical,
                time_msec,
            } => self.pointer_wheel(horizontal, vertical, time_msec),
            InputMethodCommand::PointerButton {
                button,
                pressed,
                time_msec,
            } => self.pointer_button(button, pressed, next_serial(serial), time_msec),
        }
    }

    fn type_input_method_commit(&mut self, commit: InputMethodCommit, serial: &mut u32) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        if let Some(text) = commit.text.filter(|text| !text.is_empty()) {
            let keycodes = ime_character_keycodes(&text);
            if !keycodes.is_empty() {
                let _ = keyboard.set_keymap_from_string(self, ime_keymap(&text));
                for keycode in keycodes {
                    keyboard.input::<(), _>(
                        self,
                        keycode.into(),
                        KeyState::Pressed,
                        next_serial(serial),
                        0,
                        |_, _, _| FilterResult::Forward,
                    );
                    keyboard.input::<(), _>(
                        self,
                        keycode.into(),
                        KeyState::Released,
                        next_serial(serial),
                        0,
                        |_, _, _| FilterResult::Forward,
                    );
                }
                self.schedule_ime_reset();
            }
        }
        if let Some(keycode) = action_keycode(commit.action) {
            keyboard.input::<(), _>(
                self,
                keycode.into(),
                KeyState::Pressed,
                next_serial(serial),
                0,
                |_, _, _| FilterResult::Forward,
            );
            keyboard.input::<(), _>(
                self,
                keycode.into(),
                KeyState::Released,
                next_serial(serial),
                0,
                |_, _, _| FilterResult::Forward,
            );
        }
    }
}

fn vt_from_pressed_evdev_keys(pressed: &HashSet<u32>) -> Option<i32> {
    const KEY_LEFTCTRL: u32 = 29;
    const KEY_RIGHTCTRL: u32 = 97;
    const KEY_LEFTALT: u32 = 56;
    const KEY_RIGHTALT: u32 = 100;
    const KEY_F1: u32 = 59;
    const KEY_F10: u32 = 68;
    const KEY_F11: u32 = 87;
    const KEY_F12: u32 = 88;

    let control = pressed.contains(&KEY_LEFTCTRL) || pressed.contains(&KEY_RIGHTCTRL);
    let alt = pressed.contains(&KEY_LEFTALT) || pressed.contains(&KEY_RIGHTALT);
    if !control || !alt {
        return None;
    }
    pressed.iter().find_map(|key| match *key {
        KEY_F1..=KEY_F10 => i32::try_from(*key - KEY_F1 + 1).ok(),
        KEY_F11 => Some(11),
        KEY_F12 => Some(12),
        _ => None,
    })
}

fn next_serial(serial: &mut u32) -> Serial {
    let current = *serial;
    *serial = serial.wrapping_add(1);
    Serial::from(current)
}

fn wheel_v120(value: f64) -> i32 {
    (value * 120.0)
        .round()
        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

const IME_CHARACTER_KEYCODES: [u32; 48] = [
    2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 30, 31,
    32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 86,
];

fn ime_character_keycodes(text: &str) -> Vec<u32> {
    text.chars()
        .enumerate()
        .map(|(index, _)| IME_CHARACTER_KEYCODES[index % IME_CHARACTER_KEYCODES.len()])
        .collect()
}

fn ime_keymap(text: &str) -> String {
    let mut keycodes = String::new();
    let mut symbols = String::new();
    for (character, keycode) in text.chars().zip(ime_character_keycodes(text)) {
        let _ = writeln!(keycodes, "key <K{keycode}> = {};", keycode + 8);
        let _ = writeln!(
            symbols,
            "key <K{keycode}> {{ [ U{:04X} ] }};",
            u32::from(character)
        );
    }
    for (keycode, symbol) in [
        (28, "Return"),
        (14, "BackSpace"),
        (111, "Delete"),
        (105, "Left"),
        (106, "Right"),
        (103, "Up"),
        (108, "Down"),
    ] {
        let _ = writeln!(keycodes, "key <K{keycode}> = {};", keycode + 8);
        let _ = writeln!(symbols, "key <K{keycode}> {{ [ {symbol} ] }};");
    }
    format!(
        "xkb_keymap {{\nxkb_keycodes \"gamescope-ime\" {{ minimum = 9; maximum = 255; {keycodes} }};\nxkb_types \"gamescope-ime\" {{ include \"complete\" }};\nxkb_compatibility \"gamescope-ime\" {{ include \"complete\" }};\nxkb_symbols \"gamescope-ime\" {{ {symbols} }};\n}};"
    )
}

const fn action_keycode(action: InputMethodAction) -> Option<u32> {
    match action {
        InputMethodAction::None => None,
        InputMethodAction::Submit => Some(28),
        InputMethodAction::DeleteLeft => Some(14),
        InputMethodAction::DeleteRight => Some(111),
        InputMethodAction::MoveLeft => Some(105),
        InputMethodAction::MoveRight => Some(106),
        InputMethodAction::MoveUp => Some(103),
        InputMethodAction::MoveDown => Some(108),
    }
}

impl GamescopeHandler for State {
    fn gamescope_state(&mut self) -> &mut GamescopeState {
        &mut self.gamescope_state
    }

    fn xwayland_server_id(&self, client: &Client) -> u32 {
        client
            .get_data::<XWaylandClientData>()
            .and_then(|data| data.user_data().get::<XwmId>().copied())
            .and_then(|xwm_id| self.xwayland_server_ids.get(&xwm_id).copied())
            .unwrap_or(0)
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if let Some(node) = self.dmabuf_node {
            dmabuf.set_node(node);
        }
        // Renderers import lazily when the surface tree is drawn. On DRM the
        // node tag above also permits the KMS framebuffer exporter to attempt
        // direct scanout without forcing an otherwise redundant GL import.
        let _ = notifier.successful::<Self>();
    }
}

impl DrmSyncobjHandler for State {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.drm_syncobj_state.as_mut()
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(state) = client.get_data::<XWaylandClientData>() {
            return &state.compositor_state;
        }
        &client
            .get_data::<ClientState>()
            .expect("all ordinary clients use ClientState")
            .compositor_state
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, |state, display, surface| {
            if state.drm_syncobj_state.is_none() {
                return;
            }
            let acquire_point = with_states(surface, |states| {
                states
                    .cached_state
                    .get::<DrmSyncobjCachedState>()
                    .pending()
                    .acquire_point
                    .clone()
            });
            let Some(acquire_point) = acquire_point else {
                return;
            };
            let Ok((blocker, source)) = acquire_point.generate_blocker() else {
                return;
            };
            let Some(handle) = state.loop_handle.clone() else {
                return;
            };
            let Some(client) = surface.client() else {
                return;
            };
            let display = display.clone();
            if handle
                .insert_source(source, move |_, _, state| {
                    state
                        .client_compositor_state(&client)
                        .blocker_cleared(state, &display);
                    Ok(())
                })
                .is_ok()
            {
                add_blocker(surface, blocker);
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if let Some(metadata) = self.gamescope_state.prepare_surface_commit(surface) {
            self.pending_swapchain_commits
                .push((surface.clone(), metadata));
        }
        if self.focused_surface.is_none() {
            self.focus_wayland_surface(surface.clone(), SERIAL_COUNTER.next_serial());
        }
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.size = self
                .output
                .current_mode()
                .map(|mode| (mode.size.w, mode.size.h).into());
        });
        let _ = surface.send_configure();
        self.focus_wayland_surface(surface.wl_surface().clone(), SERIAL_COUNTER.next_serial());
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = surface.send_configure();
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
        let _ = surface.send_configure();
    }
}

impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: LayerSurface,
        _output: Option<wl_output::WlOutput>,
        _layer: Layer,
        _namespace: String,
    ) {
        if let Some(mode) = self.output.current_mode() {
            surface.with_pending_state(|state| {
                state.size = Some((mode.size.w, mode.size.h).into());
            });
        }
        surface.send_configure();
        self.focus_wayland_surface(surface.wl_surface().clone(), SERIAL_COUNTER.next_serial());
    }
}

impl XWaylandShellHandler for State {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }

    fn surface_associated(&mut self, _xwm: XwmId, _wl_surface: WlSurface, window: X11Surface) {
        self.remember_x11_window(&window);
        self.track_x11_window(&window);
        // X11 mapping and focus may precede xwayland-shell association. The
        // first enter then has no wl_surface to notify, so re-enter now that
        // Xwayland has attached its input surface.
        self.clear_early_x11_focus(&window);
        self.refresh_focus(SERIAL_COUNTER.next_serial());
    }
}

impl XwmHandler for State {
    fn xwm_state(&mut self, xwm: XwmId) -> &mut X11Wm {
        self.xwms
            .get_mut(&xwm)
            .expect("XWM event without an active XWM")
    }

    fn new_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.remember_x11_window(&window);
        self.track_x11_window(&window);
    }

    fn new_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.remember_x11_window(&window);
        self.track_x11_window(&window);
    }

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(mode) = self.output.current_mode() {
            let _ = window.configure(smithay::utils::Rectangle::from_size(
                (mode.size.w, mode.size.h).into(),
            ));
        }
        let _ = window.set_fullscreen(true);
        let _ = window.set_mapped(true);
        self.remember_x11_window(&window);
        self.track_x11_window(&window);
    }

    fn map_window_notify(&mut self, _xwm: XwmId, window: X11Surface) {
        // The reparenting frame is viewable only once MapNotify arrives.
        // Focusing from map_window_request races that transition and can fail
        // with BadMatch, leaving globally-active Wine clients unfocused.
        self.clear_early_x11_focus(&window);
        self.refresh_focus(SERIAL_COUNTER.next_serial());
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.remember_x11_window(&window);
        self.track_x11_window(&window);
        self.refresh_focus(SERIAL_COUNTER.next_serial());
    }

    fn unmapped_window(&mut self, xwm: XwmId, window: X11Surface) {
        let was_focused = self.keyboard_focus_is_x11_window(xwm, window.window_id());
        if !window.is_override_redirect() {
            let _ = window.set_mapped(false);
        }
        if was_focused {
            self.set_keyboard_focus(None, SERIAL_COUNTER.next_serial());
        }
        self.refresh_focus(SERIAL_COUNTER.next_serial());
    }

    fn destroyed_window(&mut self, xwm: XwmId, window: X11Surface) {
        let window_id = window.window_id();
        if self.keyboard_focus_is_x11_window(xwm, window_id) {
            self.set_keyboard_focus(None, SERIAL_COUNTER.next_serial());
        }
        let server_id = self.xwayland_server_ids.get(&xwm).copied().unwrap_or(0);
        self.x11_window_metadata.remove(&(xwm, window_id));
        self.x11_window_sequences.remove(&(xwm, window_id));
        self.content_overrides.remove(&(server_id, window_id));
        self.content_override_parents
            .retain(|(candidate, raw), parent| {
                !(*candidate == server_id && (*raw == window_id || *parent == window_id))
            });
        self.content_override_pending
            .remove(&(server_id, window_id));
        self.x11_windows.retain(|candidate| {
            !x11_window_identity_matches(candidate.xwm_id(), candidate.window_id(), xwm, window_id)
        });
        self.refresh_focus(SERIAL_COUNTER.next_serial());
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        width: Option<u32>,
        height: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        let mut geometry = window.geometry();
        if let Some(x) = x {
            geometry.loc.x = x;
        }
        if let Some(y) = y {
            geometry.loc.y = y;
        }
        if let Some(width) = width.and_then(|value| i32::try_from(value).ok()) {
            geometry.size.w = width;
        }
        if let Some(height) = height.and_then(|value| i32::try_from(value).ok()) {
            geometry.size.h = height;
        }
        let _ = window.configure(geometry);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _geometry: smithay::utils::Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
    }

    fn property_notify(&mut self, xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        self.refresh_x11_metadata(xwm, window.window_id());
        // Wine commonly installs WM_HINTS and WM_TAKE_FOCUS after mapping.
        // Re-enter the already selected target so Smithay re-evaluates the
        // ICCCM input model and sends WM_TAKE_FOCUS without requiring a click.
        if x11_property_changes_input_mode(property) {
            self.clear_early_x11_focus(&window);
        }
        self.refresh_focus(SERIAL_COUNTER.next_serial());
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        let _ = window.set_fullscreen(true);
        if let Some(mode) = self.output.current_mode() {
            let _ = window.configure(smithay::utils::Rectangle::from_size(
                (mode.size.w, mode.size.h).into(),
            ));
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        // Gamescope presents managed game windows as fullscreen even if an
        // application transiently withdraws its EWMH request.
        let _ = window.set_fullscreen(true);
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _button: u32,
        _resize_edge: ResizeEdge,
    ) {
    }

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl OutputHandler for State {}
impl FractionalScaleHandler for State {}

impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {}

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        self.pointer_location = location;
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for State {}

impl ServerDndGrabHandler for State {
    fn send(&mut self, _mime_type: String, _fd: OwnedFd, _seat: Seat<Self>) {}
}

impl SeatHandler for State {
    type KeyboardFocus = KeyboardFocusTarget;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, focused: Option<&KeyboardFocusTarget>) {
        match focused {
            Some(KeyboardFocusTarget::Wayland(surface)) => {
                perfetto_sdk::track_event_instant!(
                    "gamescope.input",
                    "Wayland keyboard focus changed",
                    |ctx: &mut EventContext| perfetto::add_event_fields(
                        ctx,
                        &[perfetto::EventField::SurfaceAlive(surface.is_alive())],
                    )
                );
            }
            Some(KeyboardFocusTarget::X11(window)) => {
                perfetto_sdk::track_event_instant!(
                    "gamescope.input",
                    "X11 keyboard focus changed",
                    |ctx: &mut EventContext| perfetto::add_event_fields(
                        ctx,
                        &[
                            perfetto::EventField::Window(u64::from(window.window_id())),
                            perfetto::EventField::SurfaceAlive(window.alive()),
                            perfetto::EventField::SurfaceAssociated(window.wl_surface().is_some()),
                        ],
                    )
                );
            }
            None => perfetto_sdk::track_event_instant!("gamescope.input", "Keyboard focus cleared"),
        }
        self.keyboard_focus = focused.cloned();
        self.focused_surface = focused.and_then(KeyboardFocusTarget::owned_wl_surface);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }
}

delegate_compositor!(State);
delegate_xdg_shell!(State);
delegate_layer_shell!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_data_device!(State);
delegate_dmabuf!(State);
delegate_drm_syncobj!(State);
delegate_output!(State);
delegate_fractional_scale!(State);
delegate_presentation!(State);
delegate_viewporter!(State);
delegate_relative_pointer!(State);
delegate_pointer_constraints!(State);
delegate_pointer_gestures!(State);
delegate_single_pixel_buffer!(State);
delegate_xwayland_shell!(State);
delegate_gamescope!(State);

/// Client data shared by ordinary clients and test harnesses.
#[must_use]
pub fn client_data() -> Arc<ClientState> {
    Arc::new(ClientState::default())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use smithay::{
        backend::renderer::{
            element::{Element, Id, Kind},
            utils::{CommitCounter, DamageSet, OpaqueRegions},
        },
        utils::{Buffer, Physical, Rectangle, Scale, Transform},
    };

    use super::{
        LayerRenderElement, OutputConfig, State, vt_from_pressed_evdev_keys,
        x11_property_changes_input_mode, x11_window_identity_matches,
    };
    use smithay::xwayland::xwm::WmWindowProperty;
    use wayland_server::Display;

    #[derive(Debug)]
    struct OpaqueElement {
        id: Id,
    }

    impl Element for OpaqueElement {
        fn id(&self) -> &Id {
            &self.id
        }

        fn current_commit(&self) -> CommitCounter {
            CommitCounter::default()
        }

        fn src(&self) -> Rectangle<f64, Buffer> {
            Rectangle::from_size((64.0, 64.0).into())
        }

        fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
            Rectangle::from_size((64, 64).into())
        }

        fn transform(&self) -> Transform {
            Transform::Normal
        }

        fn damage_since(
            &self,
            _scale: Scale<f64>,
            _commit: Option<CommitCounter>,
        ) -> DamageSet<i32, Physical> {
            DamageSet::default()
        }

        fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
            OpaqueRegions::from_slice(&[Rectangle::from_size((64, 64).into())])
        }

        fn alpha(&self) -> f32 {
            1.0
        }

        fn kind(&self) -> Kind {
            Kind::Unspecified
        }
    }

    #[test]
    fn output_mode_preserves_gamescope_millihertz_units() {
        let config = OutputConfig {
            width: 1920,
            height: 1080,
            refresh_millihz: 59_940,
        };
        let mode = config.mode();
        assert_eq!(mode.size, (1920, 1080).into());
        assert_eq!(mode.refresh, 59_940);
    }

    #[test]
    fn omitted_game_size_tracks_the_physical_output() {
        let config = OutputConfig::unspecified().resolved_for_output(2560, 1440);
        assert_eq!((config.width, config.height), (2560, 1440));

        let height_only = OutputConfig {
            width: 0,
            height: 900,
            refresh_millihz: 60_000,
        }
        .resolved_for_output(2560, 1440);
        assert_eq!((height_only.width, height_only.height), (1600, 900));
    }

    #[test]
    fn blended_overlay_never_culls_the_game_below_it() {
        let blended = LayerRenderElement::new(OpaqueElement { id: Id::new() }, true);
        assert!(blended.opaque_regions(Scale::from(1.0)).is_empty());

        let base = LayerRenderElement::new(OpaqueElement { id: Id::new() }, false);
        assert_eq!(base.opaque_regions(Scale::from(1.0)).len(), 1);
    }

    #[test]
    fn late_icccm_focus_metadata_forces_x11_reentry() {
        assert!(x11_property_changes_input_mode(WmWindowProperty::Hints));
        assert!(x11_property_changes_input_mode(WmWindowProperty::Protocols));
        assert!(!x11_property_changes_input_mode(WmWindowProperty::Title));
    }

    #[test]
    fn x11_window_identity_is_independent_of_wrapper_liveness() {
        #[derive(Clone, Copy)]
        struct TrackedWindow {
            xwm_id: Option<u8>,
            window_id: u32,
            alive: bool,
        }

        let mut windows = vec![
            TrackedWindow {
                xwm_id: Some(1),
                window_id: 42,
                alive: false,
            },
            TrackedWindow {
                xwm_id: Some(1),
                window_id: 43,
                alive: true,
            },
            TrackedWindow {
                xwm_id: Some(2),
                window_id: 42,
                alive: true,
            },
        ];

        windows.retain(|candidate| {
            !x11_window_identity_matches(candidate.xwm_id, candidate.window_id, 1, 42)
        });

        assert_eq!(windows.len(), 2);
        assert!(windows.iter().all(|candidate| candidate.alive));
    }

    #[test]
    fn steam_global_scale_is_centered_and_inverted_for_pointer_input() {
        let display = Display::<State>::new().expect("display");
        let mut state = State::new(&display.handle(), &OutputConfig::default());
        state.overscan_scale = 0.5;
        state.zoom_scale = 1.0;
        assert_eq!(state.global_scale_ratio(), 0.5);
        assert_eq!(
            state.transform_pointer_for_global_scale((320.0, 180.0)),
            (0.0, 0.0)
        );
        assert_eq!(
            state.transform_pointer_for_global_scale((640.0, 360.0)),
            (640.0, 360.0)
        );
    }

    #[test]
    fn ctrl_alt_function_keys_select_vts() {
        assert_eq!(
            vt_from_pressed_evdev_keys(&HashSet::from([29, 56, 65])),
            Some(7)
        );
        assert_eq!(vt_from_pressed_evdev_keys(&HashSet::from([29, 65])), None);
        assert_eq!(
            vt_from_pressed_evdev_keys(&HashSet::from([97, 100, 88])),
            Some(12)
        );

        let display = Display::<State>::new().expect("display");
        let mut state = State::new(&display.handle(), &OutputConfig::default());
        state.enable_vt_switching();
        assert!(
            !state.filter_vt_keycode(37_u32.into(), smithay::backend::input::KeyState::Pressed)
        );
        assert!(
            !state.filter_vt_keycode(64_u32.into(), smithay::backend::input::KeyState::Pressed)
        );
        assert!(state.filter_vt_keycode(73_u32.into(), smithay::backend::input::KeyState::Pressed));
        assert_eq!(state.take_vt_switch(), Some(7));
        assert!(
            state.filter_vt_keycode(73_u32.into(), smithay::backend::input::KeyState::Released)
        );
    }
}
