use std::sync::Mutex;

use gamescope_core::input_method::{InputMethodAction, InputMethodState};
use gamescope_protocols::input_method::server::{
    gamescope_input_method::{GamescopeInputMethod, Request as InputMethodRequest},
    gamescope_input_method_manager::{GamescopeInputMethodManager, Request as ManagerRequest},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, backend::GlobalId,
};

use crate::{GamescopeDispatch, GamescopeHandler, InputMethodCommand};

#[doc(hidden)]
#[derive(Debug, Default)]
pub struct InputMethodData(Mutex<InputMethodState>);

pub(super) fn register<D>(handle: &DisplayHandle) -> GlobalId
where
    D: GlobalDispatch<GamescopeInputMethodManager, ()> + 'static,
{
    handle.create_global::<D, GamescopeInputMethodManager, _>(3, ())
}

impl<D> GlobalDispatch<GamescopeInputMethodManager, (), D> for GamescopeDispatch
where
    D: Dispatch<GamescopeInputMethodManager, ()> + GamescopeHandler + 'static,
{
    fn bind(
        _state: &mut D,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<GamescopeInputMethodManager>,
        _global_data: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(resource, ());
    }
}

impl<D> Dispatch<GamescopeInputMethodManager, (), D> for GamescopeDispatch
where
    D: Dispatch<GamescopeInputMethodManager, ()>
        + Dispatch<GamescopeInputMethod, InputMethodData>
        + GamescopeHandler
        + 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &GamescopeInputMethodManager,
        request: ManagerRequest,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ManagerRequest::Destroy => {}
            ManagerRequest::CreateInputMethod {
                seat: _,
                input_method,
            } => {
                let input_method = data_init.init(input_method, InputMethodData::default());
                input_method.done(1);
            }
            _ => unreachable!("unknown gamescope_input_method_manager request"),
        }
    }
}

impl<D> Dispatch<GamescopeInputMethod, InputMethodData, D> for GamescopeDispatch
where
    D: Dispatch<GamescopeInputMethod, InputMethodData> + GamescopeHandler + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &GamescopeInputMethod,
        request: InputMethodRequest,
        data: &InputMethodData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let Ok(mut input_method) = data.0.lock() else {
            return;
        };
        let command = match request {
            InputMethodRequest::Destroy => None,
            InputMethodRequest::Commit { serial } => {
                input_method.commit(serial).map(InputMethodCommand::Commit)
            }
            InputMethodRequest::SetString { text } => {
                input_method.set_string(text);
                None
            }
            InputMethodRequest::SetAction { action } => {
                if let Ok(action) = InputMethodAction::try_from(u32::from(action)) {
                    input_method.set_action(action);
                }
                None
            }
            InputMethodRequest::PointerMotion { dx, dy } => {
                Some(InputMethodCommand::PointerMotion {
                    dx,
                    dy,
                    time_msec: input_method.next_pointer_timestamp(),
                })
            }
            InputMethodRequest::PointerWarp { x, y } => Some(InputMethodCommand::PointerWarp {
                x,
                y,
                time_msec: input_method.next_pointer_timestamp(),
            }),
            InputMethodRequest::PointerWheel { x, y } => {
                let (horizontal, vertical) = InputMethodState::wheel_delta(x, y);
                Some(InputMethodCommand::PointerWheel {
                    horizontal,
                    vertical,
                    time_msec: input_method.next_pointer_timestamp(),
                })
            }
            InputMethodRequest::PointerButton { button, state } => {
                Some(InputMethodCommand::PointerButton {
                    button,
                    pressed: u32::from(state) == 1,
                    time_msec: input_method.next_pointer_timestamp(),
                })
            }
            _ => unreachable!("unknown gamescope_input_method request"),
        };
        drop(input_method);
        if let Some(command) = command {
            state
                .gamescope_state()
                .commands
                .push(crate::Command::InputMethod(command));
        }
    }
}
