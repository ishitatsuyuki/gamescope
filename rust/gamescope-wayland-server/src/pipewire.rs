use gamescope_protocols::pipewire::server::gamescope_pipewire::{GamescopePipewire, Request};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, backend::GlobalId,
};

use crate::{GamescopeDispatch, GamescopeHandler};

pub(super) fn register<D>(handle: &DisplayHandle, node_id: u32) -> GlobalId
where
    D: GlobalDispatch<GamescopePipewire, u32> + 'static,
{
    handle.create_global::<D, GamescopePipewire, _>(1, node_id)
}

impl<D> GlobalDispatch<GamescopePipewire, u32, D> for GamescopeDispatch
where
    D: Dispatch<GamescopePipewire, ()> + GamescopeHandler + 'static,
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<GamescopePipewire>,
        node_id: &u32,
        data_init: &mut DataInit<'_, D>,
    ) {
        let resource = data_init.init(resource, ());
        resource.stream_node(*node_id);
    }
}

impl<D> Dispatch<GamescopePipewire, (), D> for GamescopeDispatch
where
    D: Dispatch<GamescopePipewire, ()> + GamescopeHandler + 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &GamescopePipewire,
        request: Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            Request::Destroy => {}
            _ => unreachable!("unknown gamescope_pipewire request"),
        }
    }
}
