//! Which keys a shell should hand to the popup.
//!
//! The desktop keeps this state and pushes the result to the shell whenever it
//! changes, so the shell decides each key locally with no round trip. The rules
//! match tabterm's `KeyInterceptor`.

use std::collections::BTreeMap;

use tab_proto::figterm::Action;
use tab_settings::keybindings::{
    KeyBinding,
    KeyBindings,
};

// TODO: remove hardcoded list of global actions and use `availability`
const GLOBAL_ACTIONS: &[&str] = &["toggleAutocomplete", "showAutocomplete"];

const IGNORE_ACTION: &str = "ignore";

/// The name the shell uses for a key binding, e.g. `control+r` or `shift+tab`.
///
/// Modifiers come in the order control, option, shift. Returns `None` for keys a
/// shell can't see, such as anything with `command`.
pub fn canonical_key(binding: &str) -> Option<String> {
    let (mut control, mut option, mut shift) = (false, false, false);
    let mut remaining = binding;
    let key = loop {
        match remaining.split_once('+') {
            Some(("", "")) | None => break remaining,
            Some((modifier, rest)) => {
                match modifier {
                    "ctrl" | "control" => control = true,
                    "alt" | "option" => option = true,
                    "shift" => shift = true,
                    _ => return None,
                }
                remaining = rest;
            },
        }
    };

    let key = match key {
        "arrowup" | "up" => "up".to_owned(),
        "arrowdown" | "down" => "down".to_owned(),
        "arrowleft" | "left" => "left".to_owned(),
        "arrowright" | "right" => "right".to_owned(),
        "esc" | "escape" => "esc".to_owned(),
        "enter" | "tab" | "backspace" | "delete" | "home" | "end" | "pageup" | "pagedown" => key.to_owned(),
        f_key if f_key.len() > 1 && f_key.starts_with('f') && f_key[1..].parse::<u8>().is_ok() => f_key.to_owned(),
        c => {
            let mut chars = c.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            // Terminals send the same bytes for control+r and control+R
            if control || option {
                first.to_ascii_lowercase().to_string()
            } else if shift {
                shift = false;
                first.to_ascii_uppercase().to_string()
            } else {
                first.to_string()
            }
        },
    };

    let mut name = String::new();
    if control {
        name.push_str("control+");
    }
    if option {
        name.push_str("option+");
    }
    if shift {
        name.push_str("shift+");
    }
    name.push_str(&key);
    Some(name)
}

#[derive(Debug, Clone, Default)]
pub struct KeyState {
    intercept: bool,
    intercept_global: bool,
    visible: bool,
    only_show_on_tab: bool,
    mappings: BTreeMap<String, String>,
}

impl KeyState {
    /// Starts from the default bindings
    pub fn new(only_show_on_tab: bool) -> Self {
        let mut state = Self {
            only_show_on_tab,
            ..Default::default()
        };
        for KeyBinding { identifier, binding } in KeyBindings::load_hardcoded() {
            state.insert(&binding, identifier);
        }
        state
    }

    fn insert(&mut self, binding: &str, action: String) {
        if let Some(key) = canonical_key(binding) {
            self.mappings.insert(key, action);
        }
    }

    pub fn set_intercepts(&mut self, intercept: bool, intercept_global: bool, actions: &[Action], replace: bool) {
        self.intercept = intercept;
        self.intercept_global = intercept_global;
        if replace {
            self.mappings.clear();
        }
        for Action { identifier, bindings } in actions {
            for binding in bindings {
                self.insert(binding, identifier.clone());
            }
        }
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    pub fn reset(&mut self) {
        self.intercept = false;
        self.intercept_global = false;
    }

    /// Every key the popup may take, whatever the current state
    pub fn bindable(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .mappings
            .iter()
            .filter(|(_, action)| *action != IGNORE_ACTION)
            .map(|(key, _)| key.clone())
            .collect();
        if self.only_show_on_tab && !keys.iter().any(|key| key == "tab") {
            keys.push("tab".into());
            keys.sort();
        }
        keys
    }

    /// The keys the popup takes now, with their actions
    pub fn active(&self) -> Vec<(String, String)> {
        let usable =
            |(key, action): (&String, &String)| (action != IGNORE_ACTION).then(|| (key.clone(), action.clone()));

        match (self.intercept_global, self.intercept) {
            (true, false) => {
                let mut active: BTreeMap<String, String> = self
                    .mappings
                    .iter()
                    .filter(|(_, action)| GLOBAL_ACTIONS.contains(&action.as_str()))
                    .filter_map(usable)
                    .collect();
                // TODO: only show on tab should be encoded in AE
                if self.only_show_on_tab {
                    active.insert("tab".into(), "showAutocomplete".into());
                }
                active.into_iter().collect()
            },
            (_, true) if self.visible => self.mappings.iter().filter_map(usable).collect(),
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_names() {
        let cases = [
            ("tab", Some("tab")),
            ("shift+tab", Some("shift+tab")),
            ("ctrl+r", Some("control+r")),
            ("control+R", Some("control+r")),
            ("arrowup", Some("up")),
            ("escape", Some("esc")),
            ("option+shift+a", Some("option+shift+a")),
            ("shift+a", Some("A")),
            ("control+1", Some("control+1")),
            ("f5", Some("f5")),
            ("command+i", None),
            ("command+=", None),
            ("hyper+x", None),
            ("ab", None),
        ];
        for (binding, expected) in cases {
            assert_eq!(canonical_key(binding).as_deref(), expected, "{binding}");
        }
    }

    fn state() -> KeyState {
        let mut state = KeyState::default();
        state.insert("tab", "insertCommonPrefix".into());
        state.insert("esc", "hideAutocomplete".into());
        state.insert("control+x", IGNORE_ACTION.into());
        state
    }

    #[test]
    fn nothing_active_by_default() {
        assert!(state().active().is_empty());
    }

    #[test]
    fn bound_keys_need_a_visible_popup() {
        let mut state = state();
        state.set_intercepts(true, false, &[], false);
        assert!(state.active().is_empty());

        state.set_visible(true);
        assert_eq!(state.active(), vec![
            ("esc".to_owned(), "hideAutocomplete".to_owned()),
            ("tab".to_owned(), "insertCommonPrefix".to_owned()),
        ]);
    }

    #[test]
    fn global_only_takes_global_actions() {
        let mut state = state();
        state.insert("control+t", "toggleAutocomplete".into());
        state.set_intercepts(false, true, &[], false);
        assert_eq!(state.active(), vec![(
            "control+t".to_owned(),
            "toggleAutocomplete".to_owned()
        )]);

        state.only_show_on_tab = true;
        assert!(
            state
                .active()
                .contains(&("tab".to_owned(), "showAutocomplete".to_owned()))
        );
    }

    #[test]
    fn actions_add_or_replace_bindings() {
        let mut state = state();
        state.set_visible(true);
        let actions = [Action {
            identifier: "navigateDown".into(),
            bindings: vec!["control+n".into(), "command+n".into()],
        }];

        state.set_intercepts(true, false, &actions, false);
        assert_eq!(state.active().len(), 3);

        state.set_intercepts(true, false, &actions, true);
        assert_eq!(state.active(), vec![(
            "control+n".to_owned(),
            "navigateDown".to_owned()
        )]);
    }

    #[test]
    fn bindable_skips_ignored_keys() {
        let mut state = state();
        assert_eq!(state.bindable(), vec!["esc".to_owned(), "tab".to_owned()]);
        state.mappings.clear();
        state.only_show_on_tab = true;
        assert_eq!(state.bindable(), vec!["tab".to_owned()]);
    }

    #[test]
    fn reset_drops_intercepts() {
        let mut state = state();
        state.set_visible(true);
        state.set_intercepts(true, true, &[], false);
        state.reset();
        assert!(state.active().is_empty());
    }
}
