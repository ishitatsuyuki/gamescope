//! Wayland dispatch for Gamescope-specific protocols.
//!
//! Backends consume [`Command`] values and publish asynchronous completions
//! through methods on [`GamescopeState`]. This keeps protocol object lifetimes
//! in the Wayland thread while DRM, rendering, `PipeWire`, and screenshot work can
//! be performed by their owning subsystems.

#![forbid(unsafe_code)]

mod action_binding;
mod control;
mod input_method;
mod pipewire;
mod private;
mod reshade;
mod swapchain;
mod xwayland;

use std::os::fd::OwnedFd;

use gamescope_core::control::{DisplayPowerOperation, RefreshCycleOverride};
use gamescope_core::input_method::InputMethodCommit;
use gamescope_core::swapchain::{CommitMetadata, PastPresentTiming};
use gamescope_core::wire::split_u64;
use gamescope_protocols::{
    action_binding::server::gamescope_action_binding::{GamescopeActionBinding, TriggerFlag},
    control::server::gamescope_control::GamescopeControl,
    private::server::gamescope_private::GamescopePrivate,
    reshade::server::gamescope_reshade::GamescopeReshade,
};
use wayland_server::protocol::wl_surface::WlSurface;
use wayland_server::{DisplayHandle, Resource, Weak, backend::GlobalId};

#[doc(hidden)]
pub mod reexports {
    pub use gamescope_protocols;
    pub use wayland_server;
}

/// Static information sent when clients bind globals.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ServerConfig {
    pub pipewire_node_id: Option<u32>,
    pub active_display: Option<ActiveDisplayInfo>,
}

/// Contents of `gamescope_control.active_display_info`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveDisplayInfo {
    pub connector_name: String,
    pub display_make: String,
    pub display_model: String,
    pub flags: u32,
    pub valid_refresh_rates_hz: Vec<u32>,
}

/// Token used to deliver `gamescope_private.command_executed` after a backend
/// successfully executes a console command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrivateReply(Weak<GamescopePrivate>);

/// Token used to deliver `gamescope_reshade.effect_ready` to the requester.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReshadeReply(Weak<GamescopeReshade>);

/// Work emitted by protocol dispatch and consumed by compositor subsystems.
#[derive(Debug)]
pub enum Command {
    SetRefreshCycle(RefreshCycleOverride),
    TakeScreenshot {
        path: String,
        screenshot_type: u32,
        flags: u32,
    },
    SetDisplayPower(DisplayPowerOperation),
    SetLook {
        gamma_22_lut: OwnedFd,
        pq_lut: OwnedFd,
        flags: u32,
    },
    UnsetLook,
    ExecutePrivate {
        reply: PrivateReply,
        command: String,
        value: String,
    },
    SetReshadeEffect {
        reply: ReshadeReply,
        path: String,
    },
    EnableReshadeEffect,
    SetReshadeUniform {
        key: String,
        value: Vec<u8>,
    },
    DisableReshadeEffect,
    OverrideWindowContent {
        surface: WlSurface,
        xwayland_server_id: u32,
        x11_window: u32,
    },
    InputMethod(InputMethodCommand),
}

/// Input synthesized by the private Gamescope input-method protocol.
#[derive(Clone, Debug, PartialEq)]
pub enum InputMethodCommand {
    Commit(InputMethodCommit),
    PointerMotion {
        dx: f64,
        dy: f64,
        time_msec: u32,
    },
    PointerWarp {
        x: f64,
        y: f64,
        time_msec: u32,
    },
    PointerWheel {
        horizontal: f64,
        vertical: f64,
        time_msec: u32,
    },
    PointerButton {
        button: u32,
        pressed: bool,
        time_msec: u32,
    },
}

/// IDs returned when Gamescope globals are registered.
#[derive(Debug)]
pub struct GamescopeGlobals {
    pub action_binding: GlobalId,
    pub control: GlobalId,
    pub pipewire: Option<GlobalId>,
    pub private: GlobalId,
    pub reshade: GlobalId,
    pub swapchain: GlobalId,
    pub input_method: GlobalId,
    pub xwayland: GlobalId,
}

/// Delegate target used by [`delegate_gamescope!`] to embed the protocol state
/// in a larger compositor state.
#[doc(hidden)]
pub struct GamescopeDispatch;

/// Gives delegated protocol dispatch access to the Gamescope-specific state.
pub trait GamescopeHandler {
    fn gamescope_state(&mut self) -> &mut GamescopeState;

    /// Resolve the Xwayland server associated with a private protocol client.
    /// Ordinary Wayland clients and single-Xwayland compositors use server 0.
    fn xwayland_server_id(&self, _client: &wayland_server::Client) -> u32 {
        0
    }
}

/// Protocol state owned by the compositor's Wayland event-loop thread.
#[derive(Debug, Default)]
pub struct GamescopeState {
    commands: Vec<Command>,
    control_resources: Vec<Weak<GamescopeControl>>,
    active_display: Option<ActiveDisplayInfo>,
    performance_requests: std::collections::HashMap<u32, Vec<Weak<GamescopeControl>>>,
    private_resources: Vec<Weak<GamescopePrivate>>,
    action_bindings: Vec<Weak<GamescopeActionBinding>>,
    action_sequence: u32,
    swapchain_surfaces: Vec<swapchain::SurfaceEntry>,
}

impl GamescopeState {
    /// Seed protocol state with the display information used for future binds.
    #[must_use]
    pub fn with_config(config: &ServerConfig) -> Self {
        Self {
            active_display: config.active_display.clone(),
            ..Self::default()
        }
    }

    /// Advertise the Gamescope globals implemented by this slice.
    #[must_use]
    pub fn register_globals(handle: &DisplayHandle, config: &ServerConfig) -> GamescopeGlobals {
        Self::register_globals_for::<Self>(handle, config)
    }

    /// Advertise Gamescope globals on a compositor state that delegates with
    /// [`delegate_gamescope!`].
    #[must_use]
    pub fn register_globals_for<D>(
        handle: &DisplayHandle,
        config: &ServerConfig,
    ) -> GamescopeGlobals
    where
        D: wayland_server::GlobalDispatch<
                gamescope_protocols::action_binding::server::gamescope_action_binding_manager::GamescopeActionBindingManager,
                (),
            > + wayland_server::GlobalDispatch<
                gamescope_protocols::control::server::gamescope_control::GamescopeControl,
                Option<ActiveDisplayInfo>,
            > + wayland_server::GlobalDispatch<
                gamescope_protocols::pipewire::server::gamescope_pipewire::GamescopePipewire,
                u32,
            > + wayland_server::GlobalDispatch<
                gamescope_protocols::private::server::gamescope_private::GamescopePrivate,
                (),
            > + wayland_server::GlobalDispatch<
                gamescope_protocols::reshade::server::gamescope_reshade::GamescopeReshade,
                (),
            > + wayland_server::GlobalDispatch<
                gamescope_protocols::swapchain::server::gamescope_swapchain_factory_v2::GamescopeSwapchainFactoryV2,
                (),
            > + wayland_server::GlobalDispatch<
                gamescope_protocols::input_method::server::gamescope_input_method_manager::GamescopeInputMethodManager,
                (),
            > + wayland_server::GlobalDispatch<
                gamescope_protocols::xwayland::server::gamescope_xwayland::GamescopeXwayland,
                (),
            > + 'static,
    {
        GamescopeGlobals {
            action_binding: action_binding::register::<D>(handle),
            control: control::register::<D>(handle, config.active_display.clone()),
            pipewire: config
                .pipewire_node_id
                .map(|node_id| pipewire::register::<D>(handle, node_id)),
            private: private::register::<D>(handle),
            reshade: reshade::register::<D>(handle),
            swapchain: swapchain::register::<D>(handle),
            input_method: input_method::register::<D>(handle),
            xwayland: xwayland::register::<D>(handle),
        }
    }

    /// Drain backend work in request order.
    pub fn drain_commands(&mut self) -> impl Iterator<Item = Command> + '_ {
        self.commands.drain(..)
    }

    /// Update every existing control resource and the snapshot used by future
    /// clients. Gamescope sends this again after connector and mode changes.
    pub fn set_active_display(&mut self, display: Option<ActiveDisplayInfo>) {
        self.active_display = display;
        let Some(display) = self.active_display.as_ref() else {
            return;
        };
        self.control_resources.retain(|resource| {
            resource.upgrade().is_ok_and(|resource| {
                control::send_active_display(&resource, display);
                true
            })
        });
    }

    /// Notify every control binding after a Wayland-requested screenshot was
    /// successfully written, matching Gamescope's broadcast behavior.
    pub fn screenshot_taken(&mut self, path: impl Into<String>) {
        let path = path.into();
        self.control_resources.retain(|resource| {
            resource.upgrade().is_ok_and(|resource| {
                resource.screenshot_taken(path.clone());
                true
            })
        });
    }

    /// Deliver the next frame-time response to all controls waiting for this
    /// app ID, then clear those one-shot requests.
    pub fn app_presented(&mut self, app_id: u32, frametime_ns: u64) {
        let Some(resources) = self.performance_requests.remove(&app_id) else {
            return;
        };
        let (high, low) = split_u64(frametime_ns);
        for resource in resources {
            if let Ok(resource) = resource.upgrade() {
                resource.app_performance_stats(app_id, low, high);
            }
        }
    }

    /// Complete a successfully executed private command.
    pub fn private_command_executed(reply: &PrivateReply) {
        if let Ok(resource) = reply.0.upgrade() {
            resource.command_executed();
        }
    }

    /// Broadcast a console log line to currently bound private clients.
    pub fn broadcast_log(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.private_resources.retain(|resource| {
            if let Ok(resource) = resource.upgrade() {
                resource.log(text.clone());
                true
            } else {
                false
            }
        });
    }

    /// Complete asynchronous `ReShade` effect loading for its requester.
    pub fn reshade_effect_ready(reply: &ReshadeReply, path: impl Into<String>) {
        if let Ok(resource) = reply.0.upgrade() {
            resource.effect_ready(path.into());
        }
    }

    /// Match a normalized set of currently pressed keysyms against armed
    /// protocol bindings. Returns whether current Gamescope behavior blocks the
    /// key event from reaching the focused client.
    pub fn process_pressed_keysyms(
        &mut self,
        pressed: impl IntoIterator<Item = u32>,
        monotonic_time_ns: u64,
    ) -> bool {
        let pressed: Vec<_> = pressed.into_iter().collect();
        self.action_bindings.retain(Weak::is_alive);

        for weak in &self.action_bindings {
            let Ok(resource) = weak.upgrade() else {
                continue;
            };
            let Some(data) = resource.data::<action_binding::BindingData>() else {
                continue;
            };
            let Ok(mut binding) = data.0.lock() else {
                continue;
            };
            let matching_triggers = binding.matching_trigger_count(pressed.iter().copied());
            for _ in 0..matching_triggers {
                if !binding.is_armed() {
                    break;
                }
                if let Some(execution) = binding.execute(self.action_sequence, monotonic_time_ns) {
                    self.action_sequence = self.action_sequence.wrapping_add(1);
                    resource.triggered(
                        execution.event.sequence,
                        execution.event.time_low,
                        execution.event.time_high,
                        TriggerFlag::from_bits_retain(execution.event.trigger_flags),
                    );
                    if execution.blocks_input {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Snapshot the Vulkan-layer metadata associated with one surface commit.
    pub fn prepare_surface_commit(&mut self, surface: &WlSurface) -> Option<CommitMetadata> {
        swapchain::prepare_surface_commit(self, surface)
    }

    /// Publish timing for a commit after the backend has presented it.
    pub fn surface_presented(
        &mut self,
        surface: &WlSurface,
        commit: &CommitMetadata,
        actual_present_time_ns: u64,
        earliest_present_time_ns: u64,
        present_margin_ns: u64,
        refresh_cycle_ns: u64,
    ) {
        swapchain::surface_presented(
            self,
            surface,
            commit,
            PastPresentTiming {
                present_id: commit.present_id.unwrap_or_default(),
                desired_present_time_ns: commit.desired_present_time_ns,
                actual_present_time_ns,
                earliest_present_time_ns,
                present_margin_ns,
            },
            refresh_cycle_ns,
        );
    }
}

impl GamescopeHandler for GamescopeState {
    fn gamescope_state(&mut self) -> &mut GamescopeState {
        self
    }
}

/// Implement all Gamescope protocol dispatch traits for a compositor state.
#[macro_export]
macro_rules! delegate_gamescope {
    ($ty:ty) => {
        $crate::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::action_binding::server::gamescope_action_binding_manager::GamescopeActionBindingManager: ()
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::action_binding::server::gamescope_action_binding_manager::GamescopeActionBindingManager: ()
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::action_binding::server::gamescope_action_binding::GamescopeActionBinding: $crate::BindingData
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::control::server::gamescope_control::GamescopeControl: Option<$crate::ActiveDisplayInfo>
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::control::server::gamescope_control::GamescopeControl: ()
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::pipewire::server::gamescope_pipewire::GamescopePipewire: u32
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::pipewire::server::gamescope_pipewire::GamescopePipewire: ()
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::private::server::gamescope_private::GamescopePrivate: ()
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::private::server::gamescope_private::GamescopePrivate: ()
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::reshade::server::gamescope_reshade::GamescopeReshade: ()
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::reshade::server::gamescope_reshade::GamescopeReshade: ()
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::swapchain::server::gamescope_swapchain_factory_v2::GamescopeSwapchainFactoryV2: ()
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::swapchain::server::gamescope_swapchain_factory_v2::GamescopeSwapchainFactoryV2: ()
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::swapchain::server::gamescope_swapchain::GamescopeSwapchain: $crate::SwapchainData
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::input_method::server::gamescope_input_method_manager::GamescopeInputMethodManager: ()
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::input_method::server::gamescope_input_method_manager::GamescopeInputMethodManager: ()
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::input_method::server::gamescope_input_method::GamescopeInputMethod: $crate::InputMethodData
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::xwayland::server::gamescope_xwayland::GamescopeXwayland: ()
        ] => $crate::GamescopeDispatch);
        $crate::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::reexports::gamescope_protocols::xwayland::server::gamescope_xwayland::GamescopeXwayland: ()
        ] => $crate::GamescopeDispatch);
    };
}

pub use action_binding::BindingData;
pub use input_method::InputMethodData;
pub use swapchain::SwapchainData;

delegate_gamescope!(GamescopeState);
