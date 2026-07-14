use gamescope_protocols::private::server::gamescope_private::{GamescopePrivate, Request};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, backend::GlobalId,
};

use crate::{Command, GamescopeDispatch, GamescopeHandler, PrivateReply};

pub(super) fn register<D>(handle: &DisplayHandle) -> GlobalId
where
    D: GlobalDispatch<GamescopePrivate, ()> + 'static,
{
    handle.create_global::<D, GamescopePrivate, _>(1, ())
}

impl<D> GlobalDispatch<GamescopePrivate, (), D> for GamescopeDispatch
where
    D: Dispatch<GamescopePrivate, ()> + GamescopeHandler + 'static,
{
    fn bind(
        state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<GamescopePrivate>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        let resource = data_init.init(resource, ());
        state
            .gamescope_state()
            .private_resources
            .push(resource.downgrade());
    }
}

impl<D> Dispatch<GamescopePrivate, (), D> for GamescopeDispatch
where
    D: Dispatch<GamescopePrivate, ()> + GamescopeHandler + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &GamescopePrivate,
        request: Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let state = state.gamescope_state();
        match request {
            Request::Destroy => {}
            Request::Execute { cvar_name, value } => state.commands.push(Command::ExecutePrivate {
                reply: PrivateReply(resource.downgrade()),
                command: cvar_name,
                value,
            }),
            _ => unreachable!("unknown gamescope_private request"),
        }
    }
}
