use gamescope_protocols::xwayland::server::gamescope_xwayland::{GamescopeXwayland, Request};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, backend::GlobalId,
};

use crate::{Command, GamescopeDispatch, GamescopeHandler};

pub(super) fn register<D>(handle: &DisplayHandle) -> GlobalId
where
    D: GlobalDispatch<GamescopeXwayland, ()> + 'static,
{
    handle.create_global::<D, GamescopeXwayland, _>(1, ())
}

impl<D> GlobalDispatch<GamescopeXwayland, (), D> for GamescopeDispatch
where
    D: Dispatch<GamescopeXwayland, ()> + GamescopeHandler + 'static,
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<GamescopeXwayland>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

impl<D> Dispatch<GamescopeXwayland, (), D> for GamescopeDispatch
where
    D: Dispatch<GamescopeXwayland, ()> + GamescopeHandler + 'static,
{
    fn request(
        state: &mut D,
        client: &Client,
        _resource: &GamescopeXwayland,
        request: Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            Request::Destroy => {}
            Request::OverrideWindowContent {
                surface,
                x11_window,
            } => {
                let xwayland_server_id = state.xwayland_server_id(client);
                state
                    .gamescope_state()
                    .commands
                    .push(Command::OverrideWindowContent {
                        surface,
                        xwayland_server_id,
                        x11_window,
                    });
            }
            _ => unreachable!("unknown gamescope_xwayland request"),
        }
    }
}
