//! Global hot-key registration for the resident shell.

#[cfg(windows)]
mod windows_impl {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN,
    };

    pub const TOGGLE_HOVER_ID: i32 = 0x7101;
    pub const TOGGLE_POPUP_ID: i32 = 0x7102;
    pub const CYCLE_PROFILES_ID: i32 = 0x7103;

    const FIXED_MODIFIERS: u32 = MOD_CONTROL.0 | MOD_ALT.0;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct ParsedHotkey {
        modifiers: HOT_KEY_MODIFIERS,
        virtual_key: u32,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ParseHotkeyError {
        Empty,
        MissingModifier,
        DuplicateModifier,
        UnknownModifier,
        UnknownKey,
    }

    impl std::fmt::Display for ParseHotkeyError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let message = match self {
                Self::Empty => "hotkey is empty",
                Self::MissingModifier => "hotkey must contain a modifier",
                Self::DuplicateModifier => "hotkey modifier is repeated",
                Self::UnknownModifier => "hotkey modifier is unknown",
                Self::UnknownKey => "hotkey key is unknown",
            };
            formatter.write_str(message)
        }
    }

    fn parse_cycle_hotkey(spec: &str) -> Result<ParsedHotkey, ParseHotkeyError> {
        let parts: Vec<_> = spec.split('+').map(str::trim).collect();
        if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
            return Err(ParseHotkeyError::Empty);
        }
        if parts.len() == 1 {
            return Err(ParseHotkeyError::MissingModifier);
        }
        let mut modifier_bits = 0_u32;
        for modifier in &parts[..parts.len() - 1] {
            let bit = match modifier.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => MOD_CONTROL.0,
                "alt" => MOD_ALT.0,
                "shift" => MOD_SHIFT.0,
                "win" | "windows" => MOD_WIN.0,
                _ => return Err(ParseHotkeyError::UnknownModifier),
            };
            if modifier_bits & bit != 0 {
                return Err(ParseHotkeyError::DuplicateModifier);
            }
            modifier_bits |= bit;
        }
        let key = parts.last().copied().ok_or(ParseHotkeyError::Empty)?;
        let virtual_key = parse_virtual_key(key).ok_or(ParseHotkeyError::UnknownKey)?;
        Ok(ParsedHotkey {
            modifiers: HOT_KEY_MODIFIERS(modifier_bits),
            virtual_key,
        })
    }

    fn parse_virtual_key(key: &str) -> Option<u32> {
        let normalized = key.trim().to_ascii_uppercase();
        let bytes = normalized.as_bytes();
        if bytes.len() == 1 && bytes[0].is_ascii_uppercase() {
            return Some(u32::from(bytes[0]));
        }
        if bytes.len() == 1 && bytes[0].is_ascii_digit() {
            return Some(u32::from(bytes[0]));
        }
        let suffix = normalized.strip_prefix('F')?;
        let number = suffix.parse::<u32>().ok()?;
        (1..=24).contains(&number).then_some(0x70 + number - 1)
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum CycleTransition {
        Unchanged,
        Replace {
            previous: Option<ParsedHotkey>,
            next: ParsedHotkey,
        },
    }

    fn plan_cycle_transition(
        previous: Option<ParsedHotkey>,
        requested: &str,
    ) -> Result<CycleTransition, ParseHotkeyError> {
        let next = parse_cycle_hotkey(requested)?;
        if previous == Some(next) {
            Ok(CycleTransition::Unchanged)
        } else {
            Ok(CycleTransition::Replace { previous, next })
        }
    }

    #[derive(Debug, Default)]
    pub struct Registrations {
        registered: Vec<i32>,
        pub conflicts: Vec<String>,
        manual_registered: bool,
        cycle: Option<ParsedHotkey>,
        cycle_spec: Option<String>,
        cycle_conflicts: Vec<String>,
    }

    impl Registrations {
        pub fn register(hwnd: HWND, cycle_profiles_hotkey: &str) -> Self {
            let mut registrations = Self::default();
            registrations.manual_registered = registrations.register_one(
                hwnd,
                TOGGLE_POPUP_ID,
                HOT_KEY_MODIFIERS(FIXED_MODIFIERS),
                u32::from(b'T'),
                "Ctrl+Alt+T",
            );
            registrations.register_one(
                hwnd,
                TOGGLE_HOVER_ID,
                HOT_KEY_MODIFIERS(FIXED_MODIFIERS),
                u32::from(b'H'),
                "Ctrl+Alt+H",
            );
            registrations.register_cycle(hwnd, cycle_profiles_hotkey);
            registrations
        }

        pub fn manual_available(&self) -> bool {
            self.manual_registered
        }

        pub fn reregister(&mut self, hwnd: HWND, cycle_profiles_hotkey: &str) {
            let plan = match plan_cycle_transition(self.cycle, cycle_profiles_hotkey) {
                Ok(plan) => plan,
                Err(error) => {
                    self.set_cycle_conflict(cycle_profiles_hotkey);
                    eprintln!("could not parse configured cycle-profiles hotkey: {error}");
                    return;
                }
            };
            let CycleTransition::Replace { previous, next } = plan else {
                self.clear_cycle_conflict();
                return;
            };

            self.clear_cycle_conflict();
            self.unregister_cycle(hwnd);
            if self.try_register(
                hwnd,
                CYCLE_PROFILES_ID,
                next.modifiers,
                next.virtual_key,
                cycle_profiles_hotkey,
            ) {
                self.cycle = Some(next);
                self.cycle_spec = Some(cycle_profiles_hotkey.to_owned());
                self.registered.push(CYCLE_PROFILES_ID);
                return;
            }

            self.set_cycle_conflict(cycle_profiles_hotkey);
            if let (Some(previous), Some(previous_spec)) = (previous, self.cycle_spec.clone()) {
                if self.try_register(
                    hwnd,
                    CYCLE_PROFILES_ID,
                    previous.modifiers,
                    previous.virtual_key,
                    &previous_spec,
                ) {
                    self.cycle = Some(previous);
                    self.registered.push(CYCLE_PROFILES_ID);
                    return;
                }
                self.set_cycle_conflict(&previous_spec);
            }
            self.cycle = None;
            self.cycle_spec = None;
        }

        fn register_cycle(&mut self, hwnd: HWND, cycle_profiles_hotkey: &str) {
            let parsed = match parse_cycle_hotkey(cycle_profiles_hotkey) {
                Ok(parsed) => parsed,
                Err(error) => {
                    self.set_cycle_conflict(cycle_profiles_hotkey);
                    eprintln!("could not parse configured cycle-profiles hotkey: {error}");
                    return;
                }
            };
            if self.try_register(
                hwnd,
                CYCLE_PROFILES_ID,
                parsed.modifiers,
                parsed.virtual_key,
                cycle_profiles_hotkey,
            ) {
                self.cycle = Some(parsed);
                self.cycle_spec = Some(cycle_profiles_hotkey.to_owned());
                self.registered.push(CYCLE_PROFILES_ID);
            } else {
                self.set_cycle_conflict(cycle_profiles_hotkey);
            }
        }

        fn unregister_cycle(&mut self, hwnd: HWND) {
            if self.cycle.is_some() {
                let _ = unsafe { UnregisterHotKey(Some(hwnd), CYCLE_PROFILES_ID) };
                self.registered.retain(|id| *id != CYCLE_PROFILES_ID);
                self.cycle = None;
            }
        }

        fn clear_cycle_conflict(&mut self) {
            for conflict in self.cycle_conflicts.drain(..) {
                if let Some(index) = self.conflicts.iter().rposition(|label| label == &conflict) {
                    self.conflicts.remove(index);
                }
            }
        }

        fn set_cycle_conflict(&mut self, label: &str) {
            if self
                .cycle_conflicts
                .iter()
                .any(|conflict| conflict == label)
            {
                return;
            }
            self.cycle_conflicts.push(label.to_owned());
            self.conflicts.push(label.to_owned());
        }

        fn register_one(
            &mut self,
            hwnd: HWND,
            id: i32,
            modifiers: HOT_KEY_MODIFIERS,
            virtual_key: u32,
            label: &str,
        ) -> bool {
            if self.try_register(hwnd, id, modifiers, virtual_key, label) {
                self.registered.push(id);
                true
            } else {
                self.conflicts.push(label.to_owned());
                false
            }
        }

        fn try_register(
            &self,
            hwnd: HWND,
            id: i32,
            modifiers: HOT_KEY_MODIFIERS,
            virtual_key: u32,
            _label: &str,
        ) -> bool {
            // RegisterHotKey returns FALSE for an existing registration. Keep
            // the resident alive and report the conflict locally instead of panicking.
            if unsafe { RegisterHotKey(Some(hwnd), id, modifiers, virtual_key).is_ok() } {
                true
            } else {
                eprintln!("Selection Translate could not register a configured hotkey");
                false
            }
        }

        pub fn unregister(&mut self, hwnd: HWND) {
            for id in self.registered.drain(..) {
                let _ = unsafe { UnregisterHotKey(Some(hwnd), id) };
            }
            self.cycle = None;
            self.cycle_spec = None;
            self.cycle_conflicts.clear();
            self.conflicts.clear();
            self.manual_registered = false;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{parse_cycle_hotkey, plan_cycle_transition, CycleTransition, ParseHotkeyError};

        #[test]
        fn parses_default_cycle_hotkey() {
            let parsed = parse_cycle_hotkey("Ctrl+Alt+P").expect("default hotkey must parse");
            assert_eq!(parsed.virtual_key, u32::from(b'P'));
            assert_eq!(parsed.modifiers.0, super::FIXED_MODIFIERS);
        }

        #[test]
        fn parses_function_key_case_insensitively() {
            let parsed = parse_cycle_hotkey("shift+win+f12").expect("function hotkey must parse");
            assert_eq!(parsed.virtual_key, 0x7b);
            assert_eq!(parsed.modifiers.0, super::MOD_SHIFT.0 | super::MOD_WIN.0);
        }

        #[test]
        fn rejects_ambiguous_or_unmodified_hotkeys() {
            assert_eq!(
                parse_cycle_hotkey("Ctrl+Ctrl+P"),
                Err(ParseHotkeyError::DuplicateModifier)
            );
            assert_eq!(
                parse_cycle_hotkey("P"),
                Err(ParseHotkeyError::MissingModifier)
            );
            assert_eq!(parse_cycle_hotkey("Ctrl+"), Err(ParseHotkeyError::Empty));
            assert_eq!(
                parse_cycle_hotkey("Ctrl+MediaPlay"),
                Err(ParseHotkeyError::UnknownKey)
            );
        }

        #[test]
        fn rejects_unknown_modifiers() {
            assert_eq!(
                parse_cycle_hotkey("Meta+P"),
                Err(ParseHotkeyError::UnknownModifier)
            );
        }

        #[test]
        fn transition_planning_preserves_equal_registration() {
            let current = parse_cycle_hotkey("Ctrl+Alt+P").ok();
            assert_eq!(
                plan_cycle_transition(current, "Ctrl+Alt+P"),
                Ok(CycleTransition::Unchanged)
            );
        }

        #[test]
        fn transition_planning_replaces_only_cycle_registration() {
            let current = parse_cycle_hotkey("Ctrl+Alt+P").ok();
            let next = parse_cycle_hotkey("Shift+F12").expect("new hotkey must parse");
            assert_eq!(
                plan_cycle_transition(current, "Shift+F12"),
                Ok(CycleTransition::Replace {
                    previous: current,
                    next,
                })
            );
        }
    }
}

#[cfg(windows)]
pub use windows_impl::*;

#[cfg(not(windows))]
pub mod windows_impl {
    #[derive(Debug, Default)]
    pub struct Registrations {
        pub conflicts: Vec<String>,
    }
}

#[cfg(not(windows))]
pub use windows_impl::*;
