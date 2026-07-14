//! Steam's observable X11 contract and Gamescope window-selection policy.
//!
//! Steam still communicates compositor state through X11 properties.  This
//! module keeps that compatibility channel separate from Smithay's XWM: the
//! XWM owns window management while this connection watches Valve-specific
//! properties and publishes the root-window feedback consumed by Steam.

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fs, process,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use gamescope_core::control::ScreenType;
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        res::{ClientIdMask, ClientIdSpec, ConnectionExt as _},
        xproto::{
            Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, PropMode,
            Window,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

pub const STEAM_APP_ID: u32 = 769;
pub const OPAQUE: u32 = u32::MAX;

/// Environment variables exported by C++ Gamescope's compatibility path.
pub const STEAM_COMPAT_ENV: &[(&str, &str)] = &[
    ("STEAM_GAMESCOPE_NIS_SUPPORTED", "1"),
    ("SRT_URLOPEN_PREFER_STEAM", "1"),
    ("STEAM_GAMESCOPE_VRR_SUPPORTED", "1"),
    ("STEAM_DISABLE_MANGOAPP_ATOM_WORKAROUND", "1"),
    ("STEAM_MANGOAPP_HORIZONTAL_SUPPORTED", "1"),
    ("STEAM_GAMESCOPE_FANCY_SCALING_SUPPORT", "1"),
    ("STEAM_GAMESCOPE_HDR_SUPPORTED", "1"),
    ("STEAM_GAMESCOPE_DYNAMIC_FPSLIMITER", "1"),
    ("STEAM_MANGOAPP_PRESETS_SUPPORTED", "1"),
    ("STEAM_USE_MANGOAPP", "1"),
];

/// Environment shared by Steam mode and ordinary nested applications.
pub const COMMON_COMPAT_ENV: &[(&str, &str)] = &[
    ("vk_xwayland_wait_ready", "false"),
    ("XWAYLAND_FORCE_ENABLE_EXTRA_MODES", "1"),
    ("SDL_VIDEO_MINIMIZE_ON_FOCUS_LOSS", "0"),
    ("ENABLE_GAMESCOPE_WSI", "1"),
];

/// Steam-specific metadata attached to an X11 window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowMetadata {
    pub app_id: u32,
    pub pid: u32,
    pub legacy_big_picture: bool,
    pub overlay: bool,
    pub external_overlay: bool,
    pub streaming_client: bool,
    pub streaming_client_video: bool,
    pub input_focus_mode: u32,
    pub opacity: u32,
}

impl Default for WindowMetadata {
    fn default() -> Self {
        Self {
            app_id: 0,
            pid: 0,
            legacy_big_picture: false,
            overlay: false,
            external_overlay: false,
            streaming_client: false,
            streaming_client_video: false,
            input_focus_mode: 0,
            opacity: OPAQUE,
        }
    }
}

impl WindowMetadata {
    #[must_use]
    pub fn effective_app_id(&self, steam_mode: bool, window_id: u32) -> u32 {
        if self.external_overlay {
            0
        } else if self.legacy_big_picture {
            STEAM_APP_ID
        } else if steam_mode {
            self.app_id
        } else {
            window_id
        }
    }

    #[must_use]
    pub const fn is_focus_candidate(&self) -> bool {
        !self.overlay && !self.external_overlay && !self.streaming_client_video
    }

    #[must_use]
    pub fn alpha(&self) -> f32 {
        self.opacity as f32 / OPAQUE as f32
    }
}

/// A window projected into the pure focus policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FocusCandidate {
    pub server_id: u32,
    pub window_id: u32,
    pub app_id: u32,
    pub mapped: bool,
    pub override_redirect: bool,
    pub transient_for: Option<u32>,
    pub width: i32,
    pub height: i32,
    pub sequence: u64,
}

impl FocusCandidate {
    #[must_use]
    pub const fn useful(self) -> bool {
        self.mapped && self.width > 1 && self.height > 1
    }
}

/// Explicit focus requests written by Steam/gamescopectl to the root window.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FocusControl {
    pub window: Option<u32>,
    pub app_ids: Vec<u32>,
}

/// Pick the base window using steamcompmgr's externally visible priority.
#[must_use]
pub fn select_focus(
    candidates: &[FocusCandidate],
    control: &FocusControl,
) -> Option<FocusCandidate> {
    let eligible = |candidate: &&FocusCandidate| candidate.useful();
    let priority = |candidate: &&FocusCandidate| {
        (
            candidate.app_id != 0,
            !candidate.override_redirect,
            candidate.sequence,
        )
    };

    if let Some(window) = control.window
        && let Some(candidate) = candidates
            .iter()
            .filter(eligible)
            .find(|candidate| candidate.window_id == window)
    {
        return Some(*candidate);
    }

    for app_id in &control.app_ids {
        if let Some(candidate) = candidates
            .iter()
            .filter(eligible)
            .filter(|candidate| candidate.app_id == *app_id)
            .max_by_key(priority)
        {
            return Some(*candidate);
        }
    }

    candidates
        .iter()
        .filter(eligible)
        .max_by_key(priority)
        .copied()
}

/// Follow transient children and select a popup/override for the focused app.
#[must_use]
pub fn select_override(
    focus: FocusCandidate,
    candidates: &[FocusCandidate],
) -> Option<FocusCandidate> {
    let mut parent = focus.window_id;
    let mut selected = None;
    let mut visited = HashSet::new();
    visited.insert(parent);

    loop {
        let next = candidates
            .iter()
            .filter(|candidate| {
                candidate.useful()
                    && candidate.server_id == focus.server_id
                    && candidate.window_id != focus.window_id
                    && candidate.transient_for == Some(parent)
                    && (candidate.override_redirect || candidate.app_id == focus.app_id)
            })
            .max_by_key(|candidate| candidate.sequence)
            .copied();
        let Some(next) = next else { break };
        if !visited.insert(next.window_id) {
            break;
        }
        parent = next.window_id;
        selected = Some(next);
    }
    selected
}

/// Select the first managed X11 ancestor of a swapchain's drawable window.
/// Games commonly create a full-size Vulkan child below their WM-managed
/// toplevel, while the Gamescope WSI protocol identifies the child.
#[must_use]
pub fn select_managed_ancestor(ancestors: &[u32], managed: &[u32]) -> Option<u32> {
    ancestors
        .iter()
        .find(|ancestor| managed.contains(ancestor))
        .copied()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeEvent {
    WindowChanged(u32),
    FocusControlChanged,
    CreateXwayland(u32),
    DestroyXwayland(u32),
    GamesRunningChanged(u32),
    SteamMaxHeightChanged(u32),
    FpsLimitChanged(u32),
    ScreenScaleChanged,
    VrrEnabledChanged(bool),
    DisplayForceInternalChanged(bool),
    DisplayModeNudge,
    DynamicRefreshChanged { screen: ScreenType, refresh_hz: u32 },
    CompositeForceChanged(bool),
}

#[derive(Clone, Debug)]
pub struct BridgeInitialState {
    pub focus_control: FocusControl,
    pub games_running: u32,
    pub steam_max_height: u32,
    pub fps_limit: u32,
    pub screen_scale: f64,
    pub screen_magnification: f64,
    pub force_internal: bool,
    pub composite_force: bool,
}

#[derive(Clone, Debug)]
pub enum SteamWorkerEvent {
    Ready {
        server_id: u32,
        initial: BridgeInitialState,
    },
    WindowMetadata {
        server_id: u32,
        window: u32,
        metadata: WindowMetadata,
    },
    WindowAncestors {
        server_id: u32,
        window: u32,
        ancestors: Vec<u32>,
    },
    FocusControl {
        server_id: u32,
        control: FocusControl,
    },
    ScreenScale {
        server_id: u32,
        scale: f64,
        magnification: f64,
    },
    Event {
        server_id: u32,
        event: BridgeEvent,
    },
    Error {
        server_id: Option<u32>,
        message: String,
    },
}

#[derive(Clone, Debug)]
struct FocusPublication {
    focusable_apps: Vec<u32>,
    focusable_windows: Vec<u32>,
    focused_window: Option<u32>,
    focused_app: Option<u32>,
    focused_gfx_app: Option<u32>,
    focus_display: String,
    steam_mode: bool,
}

#[derive(Debug, Default)]
struct SteamWorkerShared {
    focus: Mutex<Option<FocusPublication>>,
    input_counter: Mutex<Option<u32>>,
    input_focus: Mutex<HashMap<u32, Option<u32>>>,
    direct_scanout_status: Mutex<Option<u32>>,
    vrr_feedback: Mutex<Option<(bool, bool, bool)>>,
    refresh_millihz: Mutex<Option<i32>>,
    shutdown: AtomicBool,
}

#[derive(Debug)]
enum SteamWorkerCommand {
    Register {
        display_number: u32,
        server_id: u32,
        refresh_millihz: i32,
    },
    Remove {
        server_id: u32,
    },
    WatchWindow {
        server_id: u32,
        window: u32,
        pid: Option<u32>,
    },
    ReadWindow {
        server_id: u32,
        window: u32,
        pid: Option<u32>,
    },
    ResolveWindow {
        server_id: u32,
        window: u32,
    },
    CreateFeedback {
        identifier: u32,
        server_id: u32,
        display_name: String,
    },
}

/// Blocking X11 property traffic is isolated on the same logical XWM/Main
/// worker used by Gamescope's policy layer. The Wayland thread only exchanges
/// snapshots and never waits for an X11 reply.
pub struct SteamBridgeWorker {
    shared: Arc<SteamWorkerShared>,
    commands: Sender<SteamWorkerCommand>,
    events: Receiver<SteamWorkerEvent>,
    thread: Option<JoinHandle<()>>,
}

impl SteamBridgeWorker {
    #[must_use]
    pub fn spawn() -> Self {
        let shared = Arc::new(SteamWorkerShared::default());
        let (commands, command_rx) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("gamescope-xwm".into())
            .spawn(move || run_steam_worker(worker_shared, command_rx, event_tx))
            .expect("gamescope-xwm worker thread must start");
        Self {
            shared,
            commands,
            events,
            thread: Some(thread),
        }
    }

    pub fn register(&self, display_number: u32, server_id: u32, refresh_millihz: i32) {
        let _ = self.commands.send(SteamWorkerCommand::Register {
            display_number,
            server_id,
            refresh_millihz,
        });
    }

    pub fn remove(&self, server_id: u32) {
        let _ = self.commands.send(SteamWorkerCommand::Remove { server_id });
    }

    pub fn watch_window(&self, server_id: u32, window: u32, pid: Option<u32>) {
        let _ = self.commands.send(SteamWorkerCommand::WatchWindow {
            server_id,
            window,
            pid,
        });
    }

    pub fn read_window(&self, server_id: u32, window: u32, pid: Option<u32>) {
        let _ = self.commands.send(SteamWorkerCommand::ReadWindow {
            server_id,
            window,
            pid,
        });
    }

    /// Resolve a Vulkan drawable's X11 parent chain on the isolated XWM
    /// worker, never on the latency-sensitive Wayland thread.
    pub fn resolve_window(&self, server_id: u32, window: u32) {
        let _ = self
            .commands
            .send(SteamWorkerCommand::ResolveWindow { server_id, window });
    }

    pub fn publish_create_feedback(&self, identifier: u32, server_id: u32, display_name: String) {
        let _ = self.commands.send(SteamWorkerCommand::CreateFeedback {
            identifier,
            server_id,
            display_name,
        });
    }

    pub fn publish_input_counter(&self, counter: u32) {
        *self
            .shared
            .input_counter
            .lock()
            .expect("Steam input-counter mailbox poisoned") = Some(counter);
    }

    /// Gamescope explicitly sets X input focus instead of relying on
    /// WM_TAKE_FOCUS clients to do so. Keep the X request on the XWM worker.
    pub fn set_input_focus(&self, server_id: u32, window: Option<u32>) {
        self.shared
            .input_focus
            .lock()
            .expect("Steam input-focus mailbox poisoned")
            .insert(server_id, window);
    }

    pub fn publish_vrr(&self, capable: bool, enabled: bool, in_use: bool) {
        *self
            .shared
            .vrr_feedback
            .lock()
            .expect("Steam VRR mailbox poisoned") = Some((capable, enabled, in_use));
    }

    pub fn publish_direct_scanout_status(&self, status: u32) {
        *self
            .shared
            .direct_scanout_status
            .lock()
            .expect("direct-scanout status mailbox poisoned") = Some(status);
    }

    pub fn publish_refresh(&self, refresh_millihz: i32) {
        *self
            .shared
            .refresh_millihz
            .lock()
            .expect("Steam refresh mailbox poisoned") = Some(refresh_millihz);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn publish_focus(
        &self,
        focusable_apps: Vec<u32>,
        focusable_windows: Vec<u32>,
        focused_window: Option<u32>,
        focused_app: Option<u32>,
        focused_gfx_app: Option<u32>,
        focus_display: String,
        steam_mode: bool,
    ) {
        *self
            .shared
            .focus
            .lock()
            .expect("Steam focus mailbox poisoned") = Some(FocusPublication {
            focusable_apps,
            focusable_windows,
            focused_window,
            focused_app,
            focused_gfx_app,
            focus_display,
            steam_mode,
        });
    }

    pub fn drain_events(&self) -> Vec<SteamWorkerEvent> {
        self.events.try_iter().collect()
    }
}

impl Drop for SteamBridgeWorker {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Debug)]
struct SteamAtoms {
    cardinal: Atom,
    utf8_string: Atom,
    steam_game: Atom,
    steam_big_picture: Atom,
    steam_overlay: Atom,
    external_overlay: Atom,
    steam_input_focus: Atom,
    steam_streaming_client: Atom,
    steam_streaming_client_video: Atom,
    net_wm_opacity: Atom,
    games_running: Atom,
    screen_scale: Atom,
    screen_magnification: Atom,
    steam_max_height: Atom,
    fps_limit: Atom,
    focus_control_app_ids: Atom,
    focus_control_window: Atom,
    focusable_apps: Atom,
    focusable_windows: Atom,
    focused_app: Atom,
    focused_app_gfx: Atom,
    focused_window: Atom,
    focus_display: Atom,
    mouse_focus_display: Atom,
    keyboard_focus_display: Atom,
    xwayland_server_id: Atom,
    gamescope_pid: Atom,
    vr_overlay_forwarding: Atom,
    vrr_capable: Atom,
    vrr_enabled: Atom,
    vrr_feedback: Atom,
    hdr_capable: Atom,
    hdr_enabled: Atom,
    refresh_feedback: Atom,
    fsr_feedback: Atom,
    input_counter: Atom,
    direct_scanout_status: Atom,
    create_xwayland: Atom,
    create_xwayland_feedback: Atom,
    destroy_xwayland: Atom,
    display_force_internal: Atom,
    display_mode_nudge: Atom,
    dynamic_refresh_internal: Atom,
    dynamic_refresh_external: Atom,
    composite_force: Atom,
    net_active_window: Atom,
}

impl SteamAtoms {
    fn new(connection: &RustConnection) -> Result<Self, Box<dyn Error>> {
        let intern = |name: &str| -> Result<Atom, Box<dyn Error>> {
            Ok(connection
                .intern_atom(false, name.as_bytes())?
                .reply()?
                .atom)
        };
        Ok(Self {
            cardinal: AtomEnum::CARDINAL.into(),
            utf8_string: intern("UTF8_STRING")?,
            steam_game: intern("STEAM_GAME")?,
            steam_big_picture: intern("STEAM_BIGPICTURE")?,
            steam_overlay: intern("STEAM_OVERLAY")?,
            external_overlay: intern("GAMESCOPE_EXTERNAL_OVERLAY")?,
            steam_input_focus: intern("STEAM_INPUT_FOCUS")?,
            steam_streaming_client: intern("STEAM_STREAMING_CLIENT")?,
            steam_streaming_client_video: intern("STEAM_STREAMING_CLIENT_VIDEO")?,
            net_wm_opacity: intern("_NET_WM_WINDOW_OPACITY")?,
            games_running: intern("STEAM_GAMES_RUNNING")?,
            screen_scale: intern("STEAM_SCREEN_SCALE")?,
            screen_magnification: intern("STEAM_SCREEN_MAGNIFICATION")?,
            steam_max_height: intern("GAMESCOPE_STEAM_MAX_HEIGHT")?,
            fps_limit: intern("GAMESCOPE_FPS_LIMIT")?,
            focus_control_app_ids: intern("GAMESCOPECTRL_BASELAYER_APPID")?,
            focus_control_window: intern("GAMESCOPECTRL_BASELAYER_WINDOW")?,
            focusable_apps: intern("GAMESCOPE_FOCUSABLE_APPS")?,
            focusable_windows: intern("GAMESCOPE_FOCUSABLE_WINDOWS")?,
            focused_app: intern("GAMESCOPE_FOCUSED_APP")?,
            focused_app_gfx: intern("GAMESCOPE_FOCUSED_APP_GFX")?,
            focused_window: intern("GAMESCOPE_FOCUSED_WINDOW")?,
            focus_display: intern("GAMESCOPE_FOCUS_DISPLAY")?,
            mouse_focus_display: intern("GAMESCOPE_MOUSE_FOCUS_DISPLAY")?,
            keyboard_focus_display: intern("GAMESCOPE_KEYBOARD_FOCUS_DISPLAY")?,
            xwayland_server_id: intern("GAMESCOPE_XWAYLAND_SERVER_ID")?,
            gamescope_pid: intern("GAMESCOPE_PID")?,
            vr_overlay_forwarding: intern("GAMESCOPE_VROVERLAY_FORWARDING")?,
            vrr_capable: intern("GAMESCOPE_VRR_CAPABLE")?,
            vrr_enabled: intern("GAMESCOPE_VRR_ENABLED")?,
            vrr_feedback: intern("GAMESCOPE_VRR_FEEDBACK")?,
            hdr_capable: intern("GAMESCOPE_DISPLAY_SUPPORTS_HDR")?,
            hdr_enabled: intern("GAMESCOPE_DISPLAY_HDR_ENABLED")?,
            refresh_feedback: intern("GAMESCOPE_DISPLAY_REFRESH_RATE_FEEDBACK")?,
            fsr_feedback: intern("GAMESCOPE_FSR_FEEDBACK")?,
            input_counter: intern("GAMESCOPE_INPUT_COUNTER")?,
            direct_scanout_status: intern("GAMESCOPE_DIRECT_SCANOUT_STATUS")?,
            create_xwayland: intern("GAMESCOPE_CREATE_XWAYLAND_SERVER")?,
            create_xwayland_feedback: intern("GAMESCOPE_CREATE_XWAYLAND_SERVER_FEEDBACK")?,
            destroy_xwayland: intern("GAMESCOPE_DESTROY_XWAYLAND_SERVER")?,
            display_force_internal: intern("GAMESCOPE_DISPLAY_FORCE_INTERNAL")?,
            display_mode_nudge: intern("GAMESCOPE_DISPLAY_MODE_NUDGE")?,
            dynamic_refresh_internal: intern("GAMESCOPE_DYNAMIC_REFRESH")?,
            dynamic_refresh_external: intern("GAMESCOPE_DYNAMIC_REFRESH_EXTERNAL")?,
            composite_force: intern("GAMESCOPE_COMPOSITE_FORCE")?,
            net_active_window: intern("_NET_ACTIVE_WINDOW")?,
        })
    }

    fn is_window_property(&self, atom: Atom) -> bool {
        [
            self.steam_game,
            self.steam_big_picture,
            self.steam_overlay,
            self.external_overlay,
            self.steam_input_focus,
            self.steam_streaming_client,
            self.steam_streaming_client_video,
            self.net_wm_opacity,
        ]
        .contains(&atom)
    }
}

/// A side-channel connection to one Xwayland server.
#[derive(Debug)]
pub struct SteamX11Bridge {
    connection: RustConnection,
    atoms: SteamAtoms,
    root: Window,
    display_name: String,
    pub server_id: u32,
}

impl SteamX11Bridge {
    pub fn connect(
        display_number: u32,
        server_id: u32,
        output_refresh_millihz: i32,
    ) -> Result<Self, Box<dyn Error>> {
        let display_name = format!(":{display_number}");
        let (connection, screen) = x11rb::connect(Some(&display_name))?;
        let root = connection.setup().roots[screen].root;
        let atoms = SteamAtoms::new(&connection)?;
        connection.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;

        let bridge = Self {
            connection,
            atoms,
            root,
            display_name,
            server_id,
        };
        bridge.set_cardinal(bridge.atoms.xwayland_server_id, &[server_id])?;
        bridge.set_cardinal(bridge.atoms.gamescope_pid, &[process::id()])?;
        bridge.set_cardinal(bridge.atoms.vr_overlay_forwarding, &[0])?;
        bridge.set_cardinal(bridge.atoms.vrr_capable, &[0])?;
        bridge.set_cardinal(bridge.atoms.vrr_enabled, &[0])?;
        bridge.set_cardinal(bridge.atoms.vrr_feedback, &[0])?;
        bridge.set_cardinal(bridge.atoms.hdr_capable, &[0])?;
        bridge.set_cardinal(bridge.atoms.hdr_enabled, &[0])?;
        bridge.set_cardinal(bridge.atoms.fsr_feedback, &[0])?;
        bridge.set_cardinal(bridge.atoms.input_counter, &[0])?;
        bridge.set_cardinal(bridge.atoms.direct_scanout_status, &[1])?;
        let refresh_hz = u32::try_from((output_refresh_millihz + 500) / 1000).unwrap_or(0);
        bridge.set_cardinal(bridge.atoms.refresh_feedback, &[refresh_hz])?;
        bridge.connection.flush()?;
        Ok(bridge)
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn watch_window(&self, window: Window) -> Result<(), Box<dyn Error>> {
        self.connection.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        self.connection.flush()?;
        Ok(())
    }

    pub fn read_window(
        &self,
        window: Window,
        smithay_pid: Option<u32>,
    ) -> Result<WindowMetadata, Box<dyn Error>> {
        let pid = smithay_pid.or_else(|| self.window_pid(window)).unwrap_or(0);
        let property_app_id = self
            .get_cardinal(window, self.atoms.steam_game)?
            .unwrap_or(0);
        Ok(WindowMetadata {
            app_id: if property_app_id != 0 {
                property_app_id
            } else {
                steam_app_id_from_pid(pid)
            },
            pid,
            legacy_big_picture: self
                .get_cardinal(window, self.atoms.steam_big_picture)?
                .is_some_and(|value| value != 0),
            overlay: self
                .get_cardinal(window, self.atoms.steam_overlay)?
                .is_some_and(|value| value != 0),
            external_overlay: self
                .get_cardinal(window, self.atoms.external_overlay)?
                .is_some_and(|value| value != 0),
            streaming_client: self
                .get_cardinal(window, self.atoms.steam_streaming_client)?
                .is_some_and(|value| value != 0),
            streaming_client_video: self
                .get_cardinal(window, self.atoms.steam_streaming_client_video)?
                .is_some_and(|value| value != 0),
            input_focus_mode: self
                .get_cardinal(window, self.atoms.steam_input_focus)?
                .unwrap_or(0),
            opacity: self
                .get_cardinal(window, self.atoms.net_wm_opacity)?
                .unwrap_or(OPAQUE),
        })
    }

    /// Return the drawable and each non-root parent. X11 round trips stay on
    /// the XWM worker because games can issue the override from a child window
    /// which Smithay does not expose as a managed `X11Surface`.
    pub fn window_ancestors(&self, window: Window) -> Result<Vec<Window>, Box<dyn Error>> {
        let mut ancestors = Vec::new();
        let mut visited = HashSet::new();
        let mut current = window;
        while current != self.root && visited.insert(current) {
            ancestors.push(current);
            let tree = self.connection.query_tree(current)?.reply()?;
            if tree.parent == 0 || tree.parent == current || tree.parent == self.root {
                break;
            }
            current = tree.parent;
        }
        Ok(ancestors)
    }

    pub fn focus_control(&self) -> Result<FocusControl, Box<dyn Error>> {
        Ok(FocusControl {
            window: self.get_cardinal(self.root, self.atoms.focus_control_window)?,
            app_ids: self.get_cardinals(self.root, self.atoms.focus_control_app_ids)?,
        })
    }

    pub fn games_running(&self) -> Result<u32, Box<dyn Error>> {
        Ok(self
            .get_cardinal(self.root, self.atoms.games_running)?
            .unwrap_or(0))
    }

    pub fn screen_scale(&self) -> Result<f64, Box<dyn Error>> {
        Ok(f64::from(
            self.get_cardinal(self.root, self.atoms.screen_scale)?
                .unwrap_or(u32::MAX),
        ) / f64::from(u32::MAX))
    }

    pub fn screen_magnification(&self) -> Result<f64, Box<dyn Error>> {
        Ok(f64::from(
            self.get_cardinal(self.root, self.atoms.screen_magnification)?
                .unwrap_or(u16::MAX.into()),
        ) / f64::from(u16::MAX))
    }

    pub fn steam_max_height(&self) -> Result<u32, Box<dyn Error>> {
        Ok(self
            .get_cardinal(self.root, self.atoms.steam_max_height)?
            .unwrap_or(0))
    }

    pub fn fps_limit(&self) -> Result<u32, Box<dyn Error>> {
        Ok(self
            .get_cardinal(self.root, self.atoms.fps_limit)?
            .unwrap_or(0))
    }

    pub fn vrr_enabled(&self) -> Result<bool, Box<dyn Error>> {
        Ok(self
            .get_cardinal(self.root, self.atoms.vrr_enabled)?
            .is_some_and(|value| value != 0))
    }

    pub fn force_internal(&self) -> Result<bool, Box<dyn Error>> {
        Ok(self
            .get_cardinal(self.root, self.atoms.display_force_internal)?
            .is_some_and(|value| value != 0))
    }

    pub fn composite_force(&self) -> Result<bool, Box<dyn Error>> {
        Ok(self
            .get_cardinal(self.root, self.atoms.composite_force)?
            .is_some_and(|value| value != 0))
    }

    pub fn poll_events(&self) -> Result<Vec<BridgeEvent>, Box<dyn Error>> {
        let mut events = Vec::new();
        while let Some(event) = self.connection.poll_for_event()? {
            let Event::PropertyNotify(event) = event else {
                continue;
            };
            if event.window != self.root {
                if self.atoms.is_window_property(event.atom) {
                    events.push(BridgeEvent::WindowChanged(event.window));
                }
                continue;
            }
            if [
                self.atoms.focus_control_app_ids,
                self.atoms.focus_control_window,
            ]
            .contains(&event.atom)
            {
                events.push(BridgeEvent::FocusControlChanged);
            } else if event.atom == self.atoms.create_xwayland {
                if let Some(identifier) = self.get_cardinal(self.root, event.atom)?
                    && identifier != 0
                {
                    events.push(BridgeEvent::CreateXwayland(identifier));
                }
            } else if event.atom == self.atoms.destroy_xwayland {
                if let Some(server_id) = self.get_cardinal(self.root, event.atom)? {
                    events.push(BridgeEvent::DestroyXwayland(server_id));
                }
            } else if event.atom == self.atoms.games_running {
                events.push(BridgeEvent::GamesRunningChanged(self.games_running()?));
            } else if event.atom == self.atoms.steam_max_height {
                events.push(BridgeEvent::SteamMaxHeightChanged(self.steam_max_height()?));
            } else if event.atom == self.atoms.fps_limit {
                events.push(BridgeEvent::FpsLimitChanged(self.fps_limit()?));
            } else if [self.atoms.screen_scale, self.atoms.screen_magnification]
                .contains(&event.atom)
            {
                events.push(BridgeEvent::ScreenScaleChanged);
            } else if event.atom == self.atoms.vrr_enabled {
                events.push(BridgeEvent::VrrEnabledChanged(self.vrr_enabled()?));
            } else if event.atom == self.atoms.display_force_internal {
                events.push(BridgeEvent::DisplayForceInternalChanged(
                    self.force_internal()?,
                ));
            } else if event.atom == self.atoms.display_mode_nudge {
                self.connection
                    .delete_property(self.root, self.atoms.display_mode_nudge)?;
                self.connection.flush()?;
                events.push(BridgeEvent::DisplayModeNudge);
            } else if event.atom == self.atoms.dynamic_refresh_internal {
                events.push(BridgeEvent::DynamicRefreshChanged {
                    screen: ScreenType::Internal,
                    refresh_hz: self
                        .get_cardinal(self.root, event.atom)?
                        .unwrap_or_default(),
                });
            } else if event.atom == self.atoms.dynamic_refresh_external {
                events.push(BridgeEvent::DynamicRefreshChanged {
                    screen: ScreenType::External,
                    refresh_hz: self
                        .get_cardinal(self.root, event.atom)?
                        .unwrap_or_default(),
                });
            } else if event.atom == self.atoms.composite_force {
                events.push(BridgeEvent::CompositeForceChanged(self.composite_force()?));
            }
        }
        Ok(events)
    }

    pub fn publish_focus(
        &self,
        focusable_apps: &[u32],
        focusable_windows: &[u32],
        focused_window: Option<u32>,
        focused_app: Option<u32>,
        focused_gfx_app: Option<u32>,
        focus_display: &str,
        steam_mode: bool,
    ) -> Result<(), Box<dyn Error>> {
        self.set_cardinal(self.atoms.focusable_apps, focusable_apps)?;
        self.set_cardinal(self.atoms.focusable_windows, focusable_windows)?;
        self.set_optional_cardinal(self.atoms.focused_window, focused_window)?;
        if steam_mode {
            self.set_optional_cardinal(self.atoms.focused_app, focused_app)?;
            self.set_optional_cardinal(self.atoms.focused_app_gfx, focused_gfx_app)?;
        }
        self.set_text(self.atoms.focus_display, focus_display)?;
        self.set_text(self.atoms.mouse_focus_display, focus_display)?;
        self.set_text(self.atoms.keyboard_focus_display, focus_display)?;
        self.set_optional_cardinal(self.atoms.net_active_window, focused_window)?;
        self.connection.flush()?;
        Ok(())
    }

    pub fn publish_input_counter(&self, counter: u32) -> Result<(), Box<dyn Error>> {
        self.set_cardinal(self.atoms.input_counter, &[counter])?;
        self.connection.flush()?;
        Ok(())
    }

    pub fn set_input_focus(&self, window: Option<u32>) -> Result<(), Box<dyn Error>> {
        use x11rb::protocol::xproto::InputFocus;
        self.connection.set_input_focus(
            InputFocus::NONE,
            window.unwrap_or(x11rb::NONE),
            x11rb::CURRENT_TIME,
        )?;
        self.connection.flush()?;
        Ok(())
    }

    pub fn publish_direct_scanout_status(&self, status: u32) -> Result<(), Box<dyn Error>> {
        self.set_cardinal(self.atoms.direct_scanout_status, &[status])?;
        self.connection.flush()?;
        Ok(())
    }

    pub fn publish_vrr(
        &self,
        capable: bool,
        enabled: bool,
        in_use: bool,
    ) -> Result<(), Box<dyn Error>> {
        self.set_cardinal(self.atoms.vrr_capable, &[u32::from(capable)])?;
        self.set_cardinal(self.atoms.vrr_enabled, &[u32::from(enabled)])?;
        self.set_cardinal(self.atoms.vrr_feedback, &[u32::from(in_use)])?;
        self.connection.flush()?;
        Ok(())
    }

    pub fn publish_refresh(&self, refresh_millihz: i32) -> Result<(), Box<dyn Error>> {
        let refresh_hz = u32::try_from((refresh_millihz + 500) / 1000).unwrap_or(0);
        self.set_cardinal(self.atoms.refresh_feedback, &[refresh_hz])?;
        self.connection.flush()?;
        Ok(())
    }

    pub fn publish_create_feedback(
        &self,
        identifier: u32,
        server_id: u32,
        display_name: &str,
    ) -> Result<(), Box<dyn Error>> {
        self.set_text(
            self.atoms.create_xwayland_feedback,
            &format!("{identifier} {server_id} {display_name}"),
        )?;
        self.connection.flush()?;
        Ok(())
    }

    fn set_optional_cardinal(&self, atom: Atom, value: Option<u32>) -> Result<(), Box<dyn Error>> {
        let values = value.into_iter().collect::<Vec<_>>();
        self.set_cardinal(atom, &values)
    }

    fn set_cardinal(&self, atom: Atom, values: &[u32]) -> Result<(), Box<dyn Error>> {
        self.connection.change_property32(
            PropMode::REPLACE,
            self.root,
            atom,
            self.atoms.cardinal,
            values,
        )?;
        Ok(())
    }

    fn set_text(&self, atom: Atom, value: &str) -> Result<(), Box<dyn Error>> {
        self.connection.change_property8(
            PropMode::REPLACE,
            self.root,
            atom,
            self.atoms.utf8_string,
            value.as_bytes(),
        )?;
        Ok(())
    }

    fn get_cardinal(&self, window: Window, atom: Atom) -> Result<Option<u32>, Box<dyn Error>> {
        Ok(self.get_cardinals(window, atom)?.into_iter().next())
    }

    fn get_cardinals(&self, window: Window, atom: Atom) -> Result<Vec<u32>, Box<dyn Error>> {
        let reply = self
            .connection
            .get_property(false, window, atom, AtomEnum::ANY, 0, u32::MAX)?
            .reply()?;
        Ok(reply.value32().map(Iterator::collect).unwrap_or_default())
    }

    fn window_pid(&self, window: Window) -> Option<u32> {
        let reply = self
            .connection
            .res_query_client_ids(&[ClientIdSpec {
                client: window,
                mask: ClientIdMask::LOCAL_CLIENT_PID,
            }])
            .ok()?
            .reply()
            .ok()?;
        reply
            .ids
            .into_iter()
            .find_map(|id| id.value.into_iter().next())
            .filter(|pid| *pid != 0)
    }
}

fn bridge_initial_state(bridge: &SteamX11Bridge) -> Result<BridgeInitialState, Box<dyn Error>> {
    Ok(BridgeInitialState {
        focus_control: bridge.focus_control()?,
        games_running: bridge.games_running()?,
        steam_max_height: bridge.steam_max_height()?,
        fps_limit: bridge.fps_limit()?,
        screen_scale: bridge.screen_scale()?,
        screen_magnification: bridge.screen_magnification()?,
        force_internal: bridge.force_internal()?,
        composite_force: bridge.composite_force()?,
    })
}

fn send_worker_error(
    events: &Sender<SteamWorkerEvent>,
    server_id: Option<u32>,
    error: impl ToString,
) {
    let _ = events.send(SteamWorkerEvent::Error {
        server_id,
        message: error.to_string(),
    });
}

fn read_worker_window(
    bridge: &SteamX11Bridge,
    server_id: u32,
    window: u32,
    pid: Option<u32>,
    events: &Sender<SteamWorkerEvent>,
) {
    match bridge.read_window(window, pid) {
        Ok(metadata) => {
            let _ = events.send(SteamWorkerEvent::WindowMetadata {
                server_id,
                window,
                metadata,
            });
        }
        Err(error) => send_worker_error(events, Some(server_id), error),
    }
}

fn process_worker_command(
    command: SteamWorkerCommand,
    bridges: &mut HashMap<u32, SteamX11Bridge>,
    window_pids: &mut HashMap<(u32, u32), Option<u32>>,
    events: &Sender<SteamWorkerEvent>,
) {
    match command {
        SteamWorkerCommand::Register {
            display_number,
            server_id,
            refresh_millihz,
        } => match SteamX11Bridge::connect(display_number, server_id, refresh_millihz) {
            Ok(bridge) => match bridge_initial_state(&bridge) {
                Ok(initial) => {
                    bridges.insert(server_id, bridge);
                    let _ = events.send(SteamWorkerEvent::Ready { server_id, initial });
                }
                Err(error) => send_worker_error(events, Some(server_id), error),
            },
            Err(error) => send_worker_error(events, Some(server_id), error),
        },
        SteamWorkerCommand::Remove { server_id } => {
            bridges.remove(&server_id);
            window_pids.retain(|(candidate, _), _| *candidate != server_id);
        }
        SteamWorkerCommand::WatchWindow {
            server_id,
            window,
            pid,
        } => {
            window_pids.insert((server_id, window), pid);
            if let Some(bridge) = bridges.get(&server_id) {
                if let Err(error) = bridge.watch_window(window) {
                    send_worker_error(events, Some(server_id), error);
                }
                read_worker_window(bridge, server_id, window, pid, events);
            }
        }
        SteamWorkerCommand::ReadWindow {
            server_id,
            window,
            pid,
        } => {
            window_pids.insert((server_id, window), pid);
            if let Some(bridge) = bridges.get(&server_id) {
                read_worker_window(bridge, server_id, window, pid, events);
            }
        }
        SteamWorkerCommand::ResolveWindow { server_id, window } => {
            if let Some(bridge) = bridges.get(&server_id) {
                match bridge.window_ancestors(window) {
                    Ok(ancestors) => {
                        let _ = events.send(SteamWorkerEvent::WindowAncestors {
                            server_id,
                            window,
                            ancestors,
                        });
                    }
                    Err(error) => send_worker_error(events, Some(server_id), error),
                }
            }
        }
        SteamWorkerCommand::CreateFeedback {
            identifier,
            server_id,
            display_name,
        } => {
            if let Some(root) = bridges.get(&0)
                && let Err(error) =
                    root.publish_create_feedback(identifier, server_id, &display_name)
            {
                send_worker_error(events, Some(0), error);
            }
        }
    }
}

fn run_steam_worker(
    shared: Arc<SteamWorkerShared>,
    commands: Receiver<SteamWorkerCommand>,
    events: Sender<SteamWorkerEvent>,
) {
    let mut bridges = HashMap::<u32, SteamX11Bridge>::new();
    let mut window_pids = HashMap::<(u32, u32), Option<u32>>::new();
    while !shared.shutdown.load(Ordering::Acquire) {
        match commands.recv_timeout(Duration::from_millis(2)) {
            Ok(command) => {
                process_worker_command(command, &mut bridges, &mut window_pids, &events);
                while let Ok(command) = commands.try_recv() {
                    process_worker_command(command, &mut bridges, &mut window_pids, &events);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if let Some(root) = bridges.get(&0) {
            if let Some(publication) = shared
                .focus
                .lock()
                .expect("Steam focus mailbox poisoned")
                .take()
                && let Err(error) = root.publish_focus(
                    &publication.focusable_apps,
                    &publication.focusable_windows,
                    publication.focused_window,
                    publication.focused_app,
                    publication.focused_gfx_app,
                    &publication.focus_display,
                    publication.steam_mode,
                )
            {
                send_worker_error(&events, Some(0), error);
            }
            if let Some(counter) = shared
                .input_counter
                .lock()
                .expect("Steam input-counter mailbox poisoned")
                .take()
                && let Err(error) = root.publish_input_counter(counter)
            {
                send_worker_error(&events, Some(0), error);
            }
            if let Some(status) = shared
                .direct_scanout_status
                .lock()
                .expect("direct-scanout status mailbox poisoned")
                .take()
                && let Err(error) = root.publish_direct_scanout_status(status)
            {
                send_worker_error(&events, Some(0), error);
            }
            if let Some((capable, enabled, in_use)) = shared
                .vrr_feedback
                .lock()
                .expect("Steam VRR mailbox poisoned")
                .take()
                && let Err(error) = root.publish_vrr(capable, enabled, in_use)
            {
                send_worker_error(&events, Some(0), error);
            }
            if let Some(refresh_millihz) = shared
                .refresh_millihz
                .lock()
                .expect("Steam refresh mailbox poisoned")
                .take()
                && let Err(error) = root.publish_refresh(refresh_millihz)
            {
                send_worker_error(&events, Some(0), error);
            }
        }
        let focus_updates = std::mem::take(
            &mut *shared
                .input_focus
                .lock()
                .expect("Steam input-focus mailbox poisoned"),
        );
        let mut pending_focus = HashMap::new();
        for (server_id, target) in focus_updates {
            if let Some(bridge) = bridges.get(&server_id)
                && let Err(error) = bridge.set_input_focus(target)
            {
                send_worker_error(&events, Some(server_id), error);
            } else if !bridges.contains_key(&server_id) {
                pending_focus.insert(server_id, target);
            }
        }
        shared
            .input_focus
            .lock()
            .expect("Steam input-focus mailbox poisoned")
            .extend(pending_focus);

        for (&server_id, bridge) in &bridges {
            let bridge_events = match bridge.poll_events() {
                Ok(events) => events,
                Err(error) => {
                    send_worker_error(&events, Some(server_id), error);
                    continue;
                }
            };
            for event in bridge_events {
                match event {
                    BridgeEvent::WindowChanged(window) => read_worker_window(
                        bridge,
                        server_id,
                        window,
                        window_pids.get(&(server_id, window)).copied().flatten(),
                        &events,
                    ),
                    BridgeEvent::FocusControlChanged => match bridge.focus_control() {
                        Ok(control) => {
                            let _ =
                                events.send(SteamWorkerEvent::FocusControl { server_id, control });
                        }
                        Err(error) => send_worker_error(&events, Some(server_id), error),
                    },
                    BridgeEvent::ScreenScaleChanged => {
                        match (bridge.screen_scale(), bridge.screen_magnification()) {
                            (Ok(scale), Ok(magnification)) => {
                                let _ = events.send(SteamWorkerEvent::ScreenScale {
                                    server_id,
                                    scale,
                                    magnification,
                                });
                            }
                            (scale, magnification) => send_worker_error(
                                &events,
                                Some(server_id),
                                format!(
                                    "failed to read Steam screen scale: {scale:?}, {magnification:?}"
                                ),
                            ),
                        }
                    }
                    event => {
                        let _ = events.send(SteamWorkerEvent::Event { server_id, event });
                    }
                }
            }
        }
    }
}

fn steam_app_id_from_pid(pid: u32) -> u32 {
    let mut next_pid = pid;
    let mut visited = HashSet::new();
    while next_pid != 0 && visited.insert(next_pid) {
        let Ok(stat) = fs::read_to_string(format!("/proc/{next_pid}/stat")) else {
            break;
        };
        let Some(open) = stat.find('(') else { break };
        let Some(close) = stat.rfind(')') else { break };
        let process_name = &stat[open + 1..close];
        let mut fields = stat[close + 1..].split_whitespace();
        let _state = fields.next();
        let parent_pid = fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);

        if process_name == "reaper"
            && let Ok(command_line) = fs::read(format!("/proc/{next_pid}/cmdline"))
            && let Some(app_id) = app_id_from_reaper_command_line(&command_line)
        {
            return app_id;
        }
        next_pid = parent_pid;
    }
    0
}

fn app_id_from_reaper_command_line(command_line: &[u8]) -> Option<u32> {
    let mut steam_launch = false;
    for argument in command_line
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
    {
        if argument == b"--" {
            break;
        }
        if argument == b"SteamLaunch" {
            steam_launch = true;
            continue;
        }
        if steam_launch
            && let Some(value) = argument.strip_prefix(b"AppId=")
            && let Ok(value) = std::str::from_utf8(value)
            && let Ok(app_id) = value.parse::<u32>()
            && app_id != 0
        {
            return Some(app_id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        FocusCandidate, FocusControl, STEAM_APP_ID, WindowMetadata,
        app_id_from_reaper_command_line, select_focus, select_managed_ancestor, select_override,
    };

    fn candidate(window_id: u32, app_id: u32, sequence: u64) -> FocusCandidate {
        FocusCandidate {
            server_id: 0,
            window_id,
            app_id,
            mapped: true,
            override_redirect: false,
            transient_for: None,
            width: 1280,
            height: 720,
            sequence,
        }
    }

    #[test]
    fn steam_control_window_and_app_order_override_recency() {
        let candidates = [
            candidate(10, 100, 1),
            candidate(20, 200, 3),
            candidate(30, 100, 4),
        ];
        assert_eq!(
            select_focus(
                &candidates,
                &FocusControl {
                    window: Some(10),
                    app_ids: vec![200],
                }
            )
            .map(|window| window.window_id),
            Some(10)
        );
        assert_eq!(
            select_focus(
                &candidates,
                &FocusControl {
                    window: None,
                    app_ids: vec![100, 200],
                }
            )
            .map(|window| window.window_id),
            Some(30)
        );
    }

    #[test]
    fn ordinary_game_beats_newer_override_and_useless_windows() {
        let mut popup = candidate(20, 100, 5);
        popup.override_redirect = true;
        let mut useless = candidate(30, 200, 10);
        useless.width = 1;
        let candidates = [candidate(10, 100, 1), popup, useless];
        assert_eq!(
            select_focus(&candidates, &FocusControl::default()).map(|window| window.window_id),
            Some(10)
        );
    }

    #[test]
    fn transient_override_chain_follows_focused_window() {
        let focus = candidate(10, 100, 1);
        let mut first = candidate(20, 100, 2);
        first.override_redirect = true;
        first.transient_for = Some(10);
        let mut second = candidate(30, 100, 3);
        second.override_redirect = true;
        second.transient_for = Some(20);
        assert_eq!(
            select_override(focus, &[focus, first, second]).map(|window| window.window_id),
            Some(30)
        );
    }

    #[test]
    fn wsi_child_resolves_to_the_first_managed_ancestor() {
        let ancestors = [0x0460_002e, 0x04a0_0001, 0x0020_0008];
        assert_eq!(
            select_managed_ancestor(&ancestors, &[0x04a0_0001, 0x0200_0035]),
            Some(0x04a0_0001)
        );
    }

    #[test]
    fn role_metadata_matches_gamescope_app_id_rules() {
        let legacy = WindowMetadata {
            legacy_big_picture: true,
            app_id: 42,
            ..WindowMetadata::default()
        };
        assert_eq!(legacy.effective_app_id(true, 5), STEAM_APP_ID);

        let external = WindowMetadata {
            external_overlay: true,
            app_id: 42,
            ..WindowMetadata::default()
        };
        assert_eq!(external.effective_app_id(true, 5), 0);
        assert_eq!(WindowMetadata::default().effective_app_id(false, 5), 5);
    }

    #[test]
    fn steam_reaper_command_line_infers_app_id_only_before_separator() {
        assert_eq!(
            app_id_from_reaper_command_line(b"reaper\0SteamLaunch\0AppId=1234\0--\0game\0"),
            Some(1234)
        );
        assert_eq!(
            app_id_from_reaper_command_line(b"reaper\0--\0SteamLaunch\0AppId=1234\0"),
            None
        );
    }
}
