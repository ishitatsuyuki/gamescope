//! Gamescope action-binding matching and arming behavior.

use std::collections::BTreeSet;

use xkeysym::{Keysym, key};

use crate::wire::split_u64;

pub const ARM_ONE_SHOT: u32 = 0x1;
pub const ARM_NO_BLOCK: u32 = 0x2;
pub const TRIGGER_KEYBOARD: u32 = 0x1;

/// Normalize case and the aliases handled by
/// `NormalizeKeysymForHotkey` in Gamescope.
#[must_use]
pub fn normalize_keysym(raw: u32) -> u32 {
    let keysym = Keysym::new(raw);
    let uppercase = keysym.key_char().and_then(|character| {
        let mut uppercase = character.to_uppercase();
        let first = uppercase.next()?;
        uppercase
            .next()
            .is_none()
            .then_some(Keysym::from_char(first).raw())
    });

    match uppercase.unwrap_or(raw) {
        key::ISO_Left_Tab => key::Tab,
        key::ISO_Enter => key::Return,
        key::Meta_L => key::Super_L,
        key::Meta_R => key::Super_R,
        key::ISO_Level3_Shift => key::Alt_R,
        normalized => normalized,
    }
}

/// Event fields in the order declared by the XML protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TriggeredEvent {
    pub sequence: u32,
    pub time_low: u32,
    pub time_high: u32,
    pub trigger_flags: u32,
}

/// Result of executing an armed action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionExecution {
    pub event: TriggeredEvent,
    /// Current C++ behavior returned to `wlserver_process_hotkeys`.
    pub blocks_input: bool,
}

/// A protocol action with any number of exact pressed-key-set triggers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ActionBinding {
    description: String,
    keyboard_triggers: Vec<BTreeSet<u32>>,
    arm_flags: Option<u32>,
}

impl ActionBinding {
    pub fn set_description(&mut self, description: impl Into<String>) {
        self.description = description.into();
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Add an exact key combination, deduplicating aliases within that trigger.
    pub fn add_keyboard_trigger(&mut self, keysyms: impl IntoIterator<Item = u32>) {
        self.keyboard_triggers
            .push(keysyms.into_iter().map(normalize_keysym).collect());
    }

    pub fn clear_triggers(&mut self) {
        self.keyboard_triggers.clear();
    }

    pub const fn arm(&mut self, flags: u32) {
        self.arm_flags = Some(flags);
    }

    pub const fn disarm(&mut self) {
        self.arm_flags = None;
    }

    #[must_use]
    pub const fn is_armed(&self) -> bool {
        self.arm_flags.is_some()
    }

    /// Whether any trigger exactly equals the current normalized pressed set.
    #[must_use]
    pub fn matches_pressed(&self, pressed: impl IntoIterator<Item = u32>) -> bool {
        self.matching_trigger_count(pressed) != 0
    }

    /// Number of triggers exactly equal to the current normalized pressed set.
    /// Duplicate triggers remain observable because the C++ loop executes each
    /// matching entry until one-shot disarming or input blocking stops it.
    #[must_use]
    pub fn matching_trigger_count(&self, pressed: impl IntoIterator<Item = u32>) -> usize {
        if !self.is_armed() {
            return 0;
        }
        let pressed: BTreeSet<_> = pressed.into_iter().map(normalize_keysym).collect();
        self.keyboard_triggers
            .iter()
            .filter(|trigger| **trigger == pressed)
            .count()
    }

    /// Execute an action and apply one-shot disarming.
    ///
    /// `blocks_input` deliberately reflects the current implementation, which
    /// returns true when the protocol's `no_block` bit is present. This is the
    /// inverse of the XML prose and is recorded as a compatibility quirk.
    pub fn execute(&mut self, sequence: u32, monotonic_time_ns: u64) -> Option<ActionExecution> {
        let flags = self.arm_flags?;
        let (time_high, time_low) = split_u64(monotonic_time_ns);
        let result = ActionExecution {
            event: TriggeredEvent {
                sequence,
                time_low,
                time_high,
                trigger_flags: TRIGGER_KEYBOARD,
            },
            blocks_input: flags & ARM_NO_BLOCK != 0,
        };

        if flags & ARM_ONE_SHOT != 0 {
            self.disarm();
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use xkeysym::key;

    use super::{ARM_NO_BLOCK, ARM_ONE_SHOT, ActionBinding, normalize_keysym};

    #[test]
    fn normalization_matches_gamescope_aliases_and_case() {
        assert_eq!(normalize_keysym(key::a), key::A);
        assert_eq!(normalize_keysym(key::ISO_Left_Tab), key::Tab);
        assert_eq!(normalize_keysym(key::Meta_L), key::Super_L);
        assert_eq!(normalize_keysym(key::ISO_Level3_Shift), key::Alt_R);
    }

    #[test]
    fn triggers_compare_the_entire_pressed_set() {
        let mut binding = ActionBinding::default();
        binding.add_keyboard_trigger([key::Control_L, key::a, key::a]);
        binding.add_keyboard_trigger([key::Control_L, key::A]);
        binding.arm(0);

        assert!(binding.matches_pressed([key::A, key::Control_L]));
        assert_eq!(binding.matching_trigger_count([key::A, key::Control_L]), 2);
        assert!(!binding.matches_pressed([key::A]));
        assert!(!binding.matches_pressed([key::A, key::Control_L, key::Shift_L]));
    }

    #[test]
    fn one_shot_disarms_and_event_uses_xml_word_order() {
        let mut binding = ActionBinding::default();
        binding.arm(ARM_ONE_SHOT | ARM_NO_BLOCK);
        let execution = binding.execute(7, 0x0123_4567_89ab_cdef).unwrap();

        assert_eq!(execution.event.sequence, 7);
        assert_eq!(execution.event.time_low, 0x89ab_cdef);
        assert_eq!(execution.event.time_high, 0x0123_4567);
        assert!(execution.blocks_input);
        assert!(!binding.is_armed());
        assert_eq!(binding.execute(8, 0), None);
    }
}
