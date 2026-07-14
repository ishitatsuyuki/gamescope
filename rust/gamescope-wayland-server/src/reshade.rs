use gamescope_protocols::reshade::server::gamescope_reshade::{GamescopeReshade, Request};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, backend::GlobalId,
};

use crate::{Command, GamescopeDispatch, GamescopeHandler, ReshadeReply};

pub(super) fn register<D>(handle: &DisplayHandle) -> GlobalId
where
    D: GlobalDispatch<GamescopeReshade, ()> + 'static,
{
    handle.create_global::<D, GamescopeReshade, _>(1, ())
}

impl<D> GlobalDispatch<GamescopeReshade, (), D> for GamescopeDispatch
where
    D: Dispatch<GamescopeReshade, ()> + GamescopeHandler + 'static,
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<GamescopeReshade>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

impl<D> Dispatch<GamescopeReshade, (), D> for GamescopeDispatch
where
    D: Dispatch<GamescopeReshade, ()> + GamescopeHandler + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &GamescopeReshade,
        request: Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let state = state.gamescope_state();
        match request {
            Request::Destroy => {}
            Request::SetEffect { path } => state.commands.push(Command::SetReshadeEffect {
                reply: ReshadeReply(resource.downgrade()),
                path,
            }),
            Request::EnableEffect => state.commands.push(Command::EnableReshadeEffect),
            Request::SetUniformVariable { key, value } => {
                state
                    .commands
                    .push(Command::SetReshadeUniform { key, value });
            }
            Request::DisableEffect => state.commands.push(Command::DisableReshadeEffect),
            _ => unreachable!("unknown gamescope_reshade request"),
        }
    }
}
