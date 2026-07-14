//! Transactional state for `gamescope_input_method`.

/// Stable action values from the protocol XML.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u32)]
pub enum InputMethodAction {
    #[default]
    None = 0,
    Submit = 1,
    DeleteLeft = 2,
    DeleteRight = 3,
    MoveLeft = 4,
    MoveRight = 5,
    MoveUp = 6,
    MoveDown = 7,
}

impl TryFrom<u32> for InputMethodAction {
    type Error = UnknownInputMethodAction;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::None),
            1 => Ok(Self::Submit),
            2 => Ok(Self::DeleteLeft),
            3 => Ok(Self::DeleteRight),
            4 => Ok(Self::MoveLeft),
            5 => Ok(Self::MoveRight),
            6 => Ok(Self::MoveUp),
            7 => Ok(Self::MoveDown),
            _ => Err(UnknownInputMethodAction(value)),
        }
    }
}

/// An action not known by this protocol version.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnknownInputMethodAction(pub u32);

/// State atomically applied by a matching `commit` request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputMethodCommit {
    pub text: Option<String>,
    pub action: InputMethodAction,
}

/// Double-buffered IME state. Gamescope currently starts at serial 1.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputMethodState {
    serial: u32,
    pending_text: Option<String>,
    pending_action: InputMethodAction,
    fake_pointer_timestamp: u32,
}

impl Default for InputMethodState {
    fn default() -> Self {
        Self {
            serial: 1,
            pending_text: None,
            pending_action: InputMethodAction::None,
            fake_pointer_timestamp: 0,
        }
    }
}

impl InputMethodState {
    #[must_use]
    pub const fn serial(&self) -> u32 {
        self.serial
    }

    /// Update the compositor serial for a future `done` event.
    pub const fn set_serial(&mut self, serial: u32) {
        self.serial = serial;
    }

    /// Replace the pending string.
    pub fn set_string(&mut self, text: impl Into<String>) {
        self.pending_text = Some(text.into());
    }

    /// Replace the pending action.
    pub const fn set_action(&mut self, action: InputMethodAction) {
        self.pending_action = action;
    }

    /// Apply pending state only when the client echoes the current serial.
    /// A stale commit preserves pending state, matching `ime.cpp`.
    pub fn commit(&mut self, serial: u32) -> Option<InputMethodCommit> {
        if serial != self.serial {
            return None;
        }

        Some(InputMethodCommit {
            text: self.pending_text.take(),
            action: std::mem::take(&mut self.pending_action),
        })
    }

    /// Allocate the incrementing synthetic timestamp used by pointer requests.
    pub const fn next_pointer_timestamp(&mut self) -> u32 {
        self.fake_pointer_timestamp = self.fake_pointer_timestamp.wrapping_add(1);
        self.fake_pointer_timestamp
    }

    /// Convert the protocol's wheel units to Gamescope's logical wheel delta.
    #[must_use]
    pub fn wheel_delta(x: i32, y: i32) -> (f64, f64) {
        (f64::from(x) / 120.0, f64::from(y) / 120.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{InputMethodAction, InputMethodCommit, InputMethodState};

    #[test]
    fn stale_serial_preserves_double_buffered_state() {
        let mut state = InputMethodState::default();
        state.set_string("hello");
        state.set_action(InputMethodAction::Submit);

        assert_eq!(state.commit(0), None);
        assert_eq!(
            state.commit(1),
            Some(InputMethodCommit {
                text: Some("hello".into()),
                action: InputMethodAction::Submit,
            })
        );
        assert_eq!(
            state.commit(1),
            Some(InputMethodCommit {
                text: None,
                action: InputMethodAction::None,
            })
        );
    }

    #[test]
    fn pointer_timestamps_wrap_and_wheel_uses_120_units() {
        let mut state = InputMethodState::default();
        assert_eq!(state.next_pointer_timestamp(), 1);
        assert_eq!(state.next_pointer_timestamp(), 2);
        assert_eq!(InputMethodState::wheel_delta(-120, 240), (-1.0, 2.0));
    }
}
