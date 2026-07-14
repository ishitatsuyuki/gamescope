use gamescope_core::control::{
    FEATURE_ADVERTISEMENT, Feature as CoreFeature, decode_display_sleep,
    decode_refresh_cycle_override,
};
use gamescope_protocols::control::server::gamescope_control::{
    DisplayFlag, Feature, GamescopeControl, Request,
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, backend::GlobalId,
};

use crate::{ActiveDisplayInfo, Command, GamescopeDispatch, GamescopeHandler};

pub(super) fn register<D>(handle: &DisplayHandle, display: Option<ActiveDisplayInfo>) -> GlobalId
where
    D: GlobalDispatch<GamescopeControl, Option<ActiveDisplayInfo>> + 'static,
{
    handle.create_global::<D, GamescopeControl, _>(6, display)
}

impl<D> GlobalDispatch<GamescopeControl, Option<ActiveDisplayInfo>, D> for GamescopeDispatch
where
    D: Dispatch<GamescopeControl, ()> + GamescopeHandler + 'static,
{
    fn bind(
        state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<GamescopeControl>,
        display: &Option<ActiveDisplayInfo>,
        data_init: &mut DataInit<'_, D>,
    ) {
        let resource = data_init.init(resource, ());
        for support in FEATURE_ADVERTISEMENT {
            resource.feature_support(
                map_feature(support.feature).into(),
                support.version,
                support.flags,
            );
        }

        let state = state.gamescope_state();
        if state.active_display.is_none() {
            state.active_display.clone_from(display);
        }
        if let Some(display) = state.active_display.as_ref() {
            send_active_display(&resource, display);
        }
        state.control_resources.push(resource.downgrade());
    }
}

pub(super) fn send_active_display(resource: &GamescopeControl, display: &ActiveDisplayInfo) {
    let refresh_rates = display
        .valid_refresh_rates_hz
        .iter()
        .flat_map(|rate| rate.to_ne_bytes())
        .collect();
    resource.active_display_info(
        display.connector_name.clone(),
        display.display_make.clone(),
        display.display_model.clone(),
        DisplayFlag::from_bits_retain(display.flags),
        refresh_rates,
    );
}

impl<D> Dispatch<GamescopeControl, (), D> for GamescopeDispatch
where
    D: Dispatch<GamescopeControl, ()> + GamescopeHandler + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &GamescopeControl,
        request: Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let state = state.gamescope_state();
        match request {
            Request::Destroy => {}
            Request::SetAppTargetRefreshCycle { fps, flags } => {
                state
                    .commands
                    .push(Command::SetRefreshCycle(decode_refresh_cycle_override(
                        fps,
                        u32::from(flags),
                    )));
            }
            Request::TakeScreenshot {
                path,
                _type: screenshot_type,
                flags,
            } => {
                state.commands.push(Command::TakeScreenshot {
                    path,
                    screenshot_type: u32::from(screenshot_type),
                    flags: u32::from(flags),
                });
            }
            Request::DisplaySleep {
                display_type,
                flags,
            } => {
                for operation in decode_display_sleep(u32::from(display_type), u32::from(flags)) {
                    state.commands.push(Command::SetDisplayPower(operation));
                }
            }
            Request::SetLook {
                lut3d_g22,
                lut3d_pq,
                flags,
            } => state.commands.push(Command::SetLook {
                gamma_22_lut: lut3d_g22,
                pq_lut: lut3d_pq,
                flags: u32::from(flags),
            }),
            Request::UnsetLook => state.commands.push(Command::UnsetLook),
            Request::RequestAppPerformanceStats { app_id } => state
                .performance_requests
                .entry(app_id)
                .or_default()
                .push(resource.downgrade()),
            _ => unreachable!("unknown gamescope_control request"),
        }
    }
}

const fn map_feature(feature: CoreFeature) -> Feature {
    match feature {
        CoreFeature::Done => Feature::Done,
        CoreFeature::ReshadeShaders => Feature::ReshadeShaders,
        CoreFeature::DisplayInfo => Feature::DisplayInfo,
        CoreFeature::PixelFilter => Feature::PixelFilter,
        CoreFeature::RefreshCycleOnlyChangeRefreshRate => {
            Feature::RefreshCycleOnlyChangeRefreshRate
        }
        CoreFeature::MuraCorrection => Feature::MuraCorrection,
        CoreFeature::Look => Feature::Look,
        CoreFeature::PerfQuery => Feature::PerfQuery,
    }
}
