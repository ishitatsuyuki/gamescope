use std::sync::Mutex;

use gamescope_core::action_binding::ActionBinding;
use gamescope_protocols::action_binding::server::{
    gamescope_action_binding::{GamescopeActionBinding, Request as BindingRequest},
    gamescope_action_binding_manager::{GamescopeActionBindingManager, Request as ManagerRequest},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, backend::GlobalId,
};

use crate::{GamescopeDispatch, GamescopeHandler};

#[doc(hidden)]
#[derive(Debug, Default)]
pub struct BindingData(pub(crate) Mutex<ActionBinding>);

pub(super) fn register<D>(handle: &DisplayHandle) -> GlobalId
where
    D: GlobalDispatch<GamescopeActionBindingManager, ()> + 'static,
{
    handle.create_global::<D, GamescopeActionBindingManager, _>(1, ())
}

impl<D> GlobalDispatch<GamescopeActionBindingManager, (), D> for GamescopeDispatch
where
    D: Dispatch<GamescopeActionBindingManager, ()> + GamescopeHandler + 'static,
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<GamescopeActionBindingManager>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

impl<D> Dispatch<GamescopeActionBindingManager, (), D> for GamescopeDispatch
where
    D: Dispatch<GamescopeActionBindingManager, ()>
        + Dispatch<GamescopeActionBinding, BindingData>
        + GamescopeHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &GamescopeActionBindingManager,
        request: ManagerRequest,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ManagerRequest::Destroy => {}
            ManagerRequest::CreateActionBinding { callback } => {
                let binding = data_init.init(callback, BindingData::default());
                state
                    .gamescope_state()
                    .action_bindings
                    .push(binding.downgrade());
            }
            _ => unreachable!("unknown gamescope_action_binding_manager request"),
        }
    }
}

impl<D> Dispatch<GamescopeActionBinding, BindingData, D> for GamescopeDispatch
where
    D: Dispatch<GamescopeActionBinding, BindingData> + GamescopeHandler + 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &GamescopeActionBinding,
        request: BindingRequest,
        data: &BindingData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let Ok(mut binding) = data.0.lock() else {
            return;
        };
        match request {
            BindingRequest::Destroy => {}
            BindingRequest::SetDescription { description } => {
                binding.set_description(description);
            }
            BindingRequest::AddKeyboardTrigger { keysyms } => {
                let keysyms = keysyms
                    .chunks_exact(size_of::<u32>())
                    .map(|bytes| u32::from_ne_bytes(bytes.try_into().expect("four-byte chunk")));
                binding.add_keyboard_trigger(keysyms);
            }
            BindingRequest::ClearTriggers => binding.clear_triggers(),
            BindingRequest::Arm { arm_flags } => binding.arm(u32::from(arm_flags)),
            BindingRequest::Disarm => binding.disarm(),
            _ => unreachable!("unknown gamescope_action_binding request"),
        }
    }
}
