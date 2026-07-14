use std::sync::{Arc, Mutex};

use gamescope_core::{
    swapchain::{
        Chromaticity, CommitMetadata, HdrMetadata, PastPresentTiming, SwapchainFeedback,
        SwapchainState,
    },
    wire::split_u64,
};
use gamescope_protocols::swapchain::server::{
    gamescope_swapchain::{GamescopeSwapchain, Request as SwapchainRequest},
    gamescope_swapchain_factory_v2::{GamescopeSwapchainFactoryV2, Request as FactoryRequest},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak,
    backend::GlobalId, protocol::wl_surface::WlSurface,
};

use crate::{Command, GamescopeDispatch, GamescopeHandler, GamescopeState};

#[doc(hidden)]
#[derive(Debug)]
pub struct SwapchainData {
    surface: Weak<WlSurface>,
    state: Arc<Mutex<SwapchainState>>,
}

#[derive(Debug)]
pub(crate) struct SurfaceEntry {
    surface: Weak<WlSurface>,
    state: Arc<Mutex<SwapchainState>>,
    resources: Vec<Weak<GamescopeSwapchain>>,
    last_refresh_cycle_ns: Option<u64>,
}

pub(super) fn register<D>(handle: &DisplayHandle) -> GlobalId
where
    D: GlobalDispatch<GamescopeSwapchainFactoryV2, ()> + 'static,
{
    handle.create_global::<D, GamescopeSwapchainFactoryV2, _>(1, ())
}

impl<D> GlobalDispatch<GamescopeSwapchainFactoryV2, (), D> for GamescopeDispatch
where
    D: Dispatch<GamescopeSwapchainFactoryV2, ()> + GamescopeHandler + 'static,
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<GamescopeSwapchainFactoryV2>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

impl<D> Dispatch<GamescopeSwapchainFactoryV2, (), D> for GamescopeDispatch
where
    D: Dispatch<GamescopeSwapchainFactoryV2, ()>
        + Dispatch<GamescopeSwapchain, SwapchainData>
        + GamescopeHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &GamescopeSwapchainFactoryV2,
        request: FactoryRequest,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            FactoryRequest::Destroy => {}
            FactoryRequest::CreateSwapchain { surface, callback } => {
                let gamescope = state.gamescope_state();
                gamescope
                    .swapchain_surfaces
                    .retain(|entry| entry.surface.is_alive());

                let state = gamescope
                    .swapchain_surfaces
                    .iter()
                    .find(|entry| same_surface(&entry.surface, &surface))
                    .map_or_else(
                        || Arc::new(Mutex::new(SwapchainState::default())),
                        |entry| Arc::clone(&entry.state),
                    );
                let resource = data_init.init(
                    callback,
                    SwapchainData {
                        surface: surface.downgrade(),
                        state: Arc::clone(&state),
                    },
                );

                if let Some(entry) = gamescope
                    .swapchain_surfaces
                    .iter_mut()
                    .find(|entry| same_surface(&entry.surface, &surface))
                {
                    entry.resources.push(resource.downgrade());
                } else {
                    gamescope.swapchain_surfaces.push(SurfaceEntry {
                        surface: surface.downgrade(),
                        state,
                        resources: vec![resource.downgrade()],
                        last_refresh_cycle_ns: None,
                    });
                }
            }
            _ => unreachable!("unknown gamescope_swapchain_factory_v2 request"),
        }
    }
}

impl<D> Dispatch<GamescopeSwapchain, SwapchainData, D> for GamescopeDispatch
where
    D: Dispatch<GamescopeSwapchain, SwapchainData> + GamescopeHandler + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &GamescopeSwapchain,
        request: SwapchainRequest,
        data: &SwapchainData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            SwapchainRequest::Destroy => {}
            SwapchainRequest::OverrideWindowContent {
                gamescope_xwayland_server_id,
                x11_window,
            } => {
                if let Ok(surface) = data.surface.upgrade() {
                    state
                        .gamescope_state()
                        .commands
                        .push(Command::OverrideWindowContent {
                            surface,
                            xwayland_server_id: gamescope_xwayland_server_id,
                            x11_window,
                        });
                }
            }
            SwapchainRequest::SwapchainFeedback {
                image_count,
                vk_format,
                vk_colorspace,
                vk_composite_alpha,
                vk_pre_transform,
                vk_clipped,
                vk_engine_name,
            } => with_state(data, |state| {
                state.set_feedback(SwapchainFeedback {
                    image_count,
                    vk_format,
                    vk_colorspace,
                    vk_composite_alpha,
                    vk_pre_transform,
                    vk_clipped,
                    vk_engine_name,
                    hdr_metadata: None,
                });
            }),
            SwapchainRequest::SetPresentMode { vk_present_mode } => {
                with_state(data, |state| state.set_present_mode(vk_present_mode));
            }
            SwapchainRequest::SetHdrMetadata {
                display_primary_red_x,
                display_primary_red_y,
                display_primary_green_x,
                display_primary_green_y,
                display_primary_blue_x,
                display_primary_blue_y,
                white_point_x,
                white_point_y,
                max_display_mastering_luminance,
                min_display_mastering_luminance,
                max_cll,
                max_fall,
            } => with_state(data, |state| {
                let _result = state.set_hdr_metadata(HdrMetadata {
                    display_primary_red: Chromaticity {
                        x: display_primary_red_x,
                        y: display_primary_red_y,
                    },
                    display_primary_green: Chromaticity {
                        x: display_primary_green_x,
                        y: display_primary_green_y,
                    },
                    display_primary_blue: Chromaticity {
                        x: display_primary_blue_x,
                        y: display_primary_blue_y,
                    },
                    white_point: Chromaticity {
                        x: white_point_x,
                        y: white_point_y,
                    },
                    max_display_mastering_luminance,
                    min_display_mastering_luminance,
                    max_cll,
                    max_fall,
                });
            }),
            SwapchainRequest::SetPresentTime {
                present_id,
                desired_present_time_hi,
                desired_present_time_lo,
            } => with_state(data, |state| {
                state.set_present_time(
                    present_id,
                    desired_present_time_hi,
                    desired_present_time_lo,
                );
            }),
            _ => unreachable!("unknown gamescope_swapchain request"),
        }
    }
}

fn with_state(data: &SwapchainData, f: impl FnOnce(&mut SwapchainState)) {
    if let Ok(mut state) = data.state.lock() {
        f(&mut state);
    }
}

fn same_surface(weak: &Weak<WlSurface>, surface: &WlSurface) -> bool {
    weak.upgrade()
        .is_ok_and(|candidate| candidate.id() == surface.id())
}

pub(super) fn prepare_surface_commit(
    gamescope: &mut GamescopeState,
    surface: &WlSurface,
) -> Option<CommitMetadata> {
    gamescope
        .swapchain_surfaces
        .iter()
        .find(|entry| same_surface(&entry.surface, surface))
        .and_then(|entry| {
            entry
                .state
                .lock()
                .ok()
                .map(|mut state| state.prepare_commit())
        })
}

pub(super) fn surface_presented(
    gamescope: &mut GamescopeState,
    surface: &WlSurface,
    commit: &CommitMetadata,
    timing: PastPresentTiming,
    refresh_cycle_ns: u64,
) {
    let Some(entry) = gamescope
        .swapchain_surfaces
        .iter_mut()
        .find(|entry| same_surface(&entry.surface, surface))
    else {
        return;
    };
    entry.resources.retain(Weak::is_alive);

    let send_refresh = entry.last_refresh_cycle_ns != Some(refresh_cycle_ns);
    if send_refresh {
        entry.last_refresh_cycle_ns = Some(refresh_cycle_ns);
    }
    let (refresh_hi, refresh_lo) = split_u64(refresh_cycle_ns);
    let times = timing.wire_time_words();
    for weak in &entry.resources {
        let Ok(resource) = weak.upgrade() else {
            continue;
        };
        if send_refresh {
            resource.refresh_cycle(refresh_hi, refresh_lo);
        }
        if commit.present_id.is_some() {
            resource.past_present_timing(
                timing.present_id,
                times[0].0,
                times[0].1,
                times[1].0,
                times[1].1,
                times[2].0,
                times[2].1,
                times[3].0,
                times[3].1,
            );
        }
    }
}
