//! The UCI option model.
//!
//! An option is a typed value with a default, a range and a name. The table below is the
//! ENGINE'S PUBLIC SURFACE: the `uci` handshake prints it verbatim, a GUI configures the
//! engine through it, and a golden pins it byte for byte — so adding, renaming or
//! reordering an entry is a protocol change, not a refactor.
//!
//! Golden: `Stockfish/src/ucioption.cpp`.

use std::collections::BTreeMap;
use std::fmt;

/// What kind of value an option holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OptionValue {
    /// A boolean. UCI spells the type `check`.
    Check { value: bool, default: bool },
    /// A bounded integer. UCI spells the type `spin`.
    Spin { value: i64, default: i64, min: i64, max: i64 },
    /// A free string.
    Text { value: String, default: String },
    /// One of a fixed set.
    ///
    /// Declared because the protocol has the type and a later option will need it —
    /// upstream's `Skill` surface is a combo. Nothing constructs one yet.
    #[allow(dead_code)]
    Combo { value: String, default: String, choices: Vec<String> },
    /// A trigger with no value, such as `Clear Hash`.
    Button,
}

/// One option: its value and where in the handshake it appears.
#[derive(Clone, Debug)]
pub(crate) struct UciOption {
    /// The declaration order, which is the order the handshake prints. A `BTreeMap` keyed
    /// by name would print alphabetically, and the golden pins upstream's order instead.
    pub(crate) index: usize,
    pub(crate) value: OptionValue,
}

impl fmt::Display for UciOption {
    /// One `option name ...` line, exactly as the protocol specifies it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.value {
            OptionValue::Check { default, .. } => write!(f, "type check default {default}"),
            OptionValue::Spin { default, min, max, .. } => {
                write!(f, "type spin default {default} min {min} max {max}")
            }
            OptionValue::Text { default, .. } => {
                // An empty string must be sent as `<empty>`: a bare `default` with nothing
                // after it is what several GUIs choke on.
                if default.is_empty() {
                    f.write_str("type string default <empty>")
                } else {
                    write!(f, "type string default {default}")
                }
            }
            OptionValue::Combo { default, choices, .. } => {
                write!(f, "type combo default {default}")?;
                for c in choices {
                    write!(f, " var {c}")?;
                }
                Ok(())
            }
            OptionValue::Button => f.write_str("type button"),
        }
    }
}

/// The engine's whole option surface.
#[derive(Clone, Debug)]
pub(crate) struct Options {
    map: BTreeMap<String, UciOption>,
    next_index: usize,
}

impl Default for Options {
    fn default() -> Options {
        let mut o = Options { map: BTreeMap::new(), next_index: 0 };
        // DECLARATION ORDER IS THE HANDSHAKE ORDER. Keep it identical to upstream's, or
        // the handshake golden fails -- which is the point of having one.
        o.add_text("Debug Log File", "");
        o.add_text("NumaPolicy", "auto");
        o.add_spin("Threads", 1, 1, 1024);
        o.add_spin("Hash", 16, 1, 33_554_432);
        o.add_button("Clear Hash");
        o.add_check("Ponder", false);
        o.add_spin("MultiPV", 1, 1, 256);
        o.add_spin("Skill Level", 20, 0, 20);
        o.add_spin("Move Overhead", 10, 0, 5000);
        o.add_spin("nodestime", 0, 0, 10000);
        o.add_check("UCI_Chess960", false);
        o.add_check("UCI_LimitStrength", false);
        o.add_spin("UCI_Elo", 1320, 1320, 3190);
        o.add_check("UCI_ShowWDL", false);
        o.add_text("SyzygyPath", "");
        o.add_spin("SyzygyProbeDepth", 1, 1, 100);
        o.add_check("Syzygy50MoveRule", true);
        o.add_spin("SyzygyProbeLimit", 7, 0, 7);
        o.add_text("EvalFile", rfish_engine::eval::nnue::DEFAULT_NET);
        o
    }
}

impl Options {
    fn insert(&mut self, name: &str, value: OptionValue) {
        self.map.insert(name.to_string(), UciOption { index: self.next_index, value });
        self.next_index += 1;
    }

    fn add_check(&mut self, name: &str, default: bool) {
        self.insert(name, OptionValue::Check { value: default, default });
    }

    fn add_spin(&mut self, name: &str, default: i64, min: i64, max: i64) {
        self.insert(name, OptionValue::Spin { value: default, default, min, max });
    }

    fn add_text(&mut self, name: &str, default: &str) {
        self.insert(
            name,
            OptionValue::Text { value: default.to_string(), default: default.to_string() },
        );
    }

    fn add_button(&mut self, name: &str) {
        self.insert(name, OptionValue::Button);
    }

    /// The options in handshake order.
    pub(crate) fn iter_declared(&self) -> impl Iterator<Item = (&str, &UciOption)> {
        let mut v: Vec<(&str, &UciOption)> =
            self.map.iter().map(|(k, o)| (k.as_str(), o)).collect();
        v.sort_by_key(|(_, o)| o.index);
        v.into_iter()
    }

    /// True when an option of that name exists. Names are matched case-insensitively,
    /// because GUIs are inconsistent about them and upstream accepts either.
    ///
    /// Used by the tests to assert that an unknown name stays unknown; the engine itself
    /// goes through `set`, which answers the same question and does the work.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.lookup(name).is_some()
    }

    fn lookup(&self, name: &str) -> Option<&String> {
        self.map.keys().find(|k| k.eq_ignore_ascii_case(name))
    }

    /// Set an option from the protocol's string form.
    ///
    /// Returns `false` when the name is unknown or the value does not parse. An unknown
    /// option is not an error the engine should die on — GUIs send options the engine never
    /// declared — so the caller reports and continues.
    pub(crate) fn set(&mut self, name: &str, value: &str) -> bool {
        let Some(key) = self.lookup(name).cloned() else { return false };
        let entry = self.map.get_mut(&key).expect("the key came from this map");
        match &mut entry.value {
            OptionValue::Check { value: v, .. } => match value.trim() {
                "true" => *v = true,
                "false" => *v = false,
                _ => return false,
            },
            OptionValue::Spin { value: v, min, max, .. } => match value.trim().parse::<i64>() {
                // Clamp rather than reject: a GUI that sends a too-large Hash should get
                // the largest table the engine allows, which is what upstream does.
                Ok(n) => *v = n.clamp(*min, *max),
                Err(_) => return false,
            },
            OptionValue::Text { value: v, .. } => {
                *v = if value == "<empty>" { String::new() } else { value.to_string() };
            }
            OptionValue::Combo { value: v, choices, .. } => {
                if !choices.iter().any(|c| c == value) {
                    return false;
                }
                *v = value.to_string();
            }
            OptionValue::Button => {}
        }
        true
    }

    /// A boolean option's value, or its default when the name is unknown.
    #[must_use]
    pub(crate) fn check(&self, name: &str) -> bool {
        match self.lookup(name).and_then(|k| self.map.get(k)).map(|o| &o.value) {
            Some(OptionValue::Check { value, .. }) => *value,
            _ => false,
        }
    }

    /// An integer option's value, or 0 when the name is unknown.
    #[must_use]
    pub(crate) fn spin(&self, name: &str) -> i64 {
        match self.lookup(name).and_then(|k| self.map.get(k)).map(|o| &o.value) {
            Some(OptionValue::Spin { value, .. }) => *value,
            _ => 0,
        }
    }

    /// A string option's value, or the empty string when the name is unknown.
    #[must_use]
    pub(crate) fn text(&self, name: &str) -> &str {
        match self.lookup(name).and_then(|k| self.map.get(k)).map(|o| &o.value) {
            Some(OptionValue::Text { value, .. }) => value.as_str(),
            _ => "",
        }
    }

    /// Every option's `option name ...` line, in handshake order.
    #[must_use]
    pub(crate) fn handshake_lines(&self) -> Vec<String> {
        self.iter_declared().map(|(name, o)| format!("option name {name} {o}")).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_declares_every_option_in_declaration_order() {
        let o = Options::default();
        let lines = o.handshake_lines();
        // The first three, in upstream's order. The whole list is pinned byte for byte by
        // the handshake golden; this only guards the head against a careless insert.
        assert!(lines[0].starts_with("option name Debug Log File type string default"));
        assert!(lines[1].starts_with("option name NumaPolicy type string default auto"));
        assert!(lines[2].starts_with("option name Threads type spin default 1 min 1 max 1024"));
        assert!(lines[3].starts_with("option name Hash type spin default 16"));
        assert_eq!(lines[4], "option name Clear Hash type button");
        // Every declared option appears exactly once.
        assert_eq!(lines.len(), o.iter_declared().count());
    }

    #[test]
    fn an_empty_string_default_is_sent_as_the_empty_marker() {
        let o = Options::default();
        let line = o
            .handshake_lines()
            .into_iter()
            .find(|l| l.starts_with("option name SyzygyPath"))
            .expect("SyzygyPath is declared");
        assert_eq!(line, "option name SyzygyPath type string default <empty>");
    }

    #[test]
    fn setting_is_case_insensitive_in_the_name() {
        let mut o = Options::default();
        assert!(o.set("threads", "4"));
        assert_eq!(o.spin("Threads"), 4);
        assert!(o.set("HASH", "64"));
        assert_eq!(o.spin("Hash"), 64);
    }

    #[test]
    fn a_spin_out_of_range_is_clamped_rather_than_rejected() {
        let mut o = Options::default();
        assert!(o.set("Hash", "99999999999"));
        assert_eq!(o.spin("Hash"), 33_554_432);
        assert!(o.set("Hash", "-5"));
        assert_eq!(o.spin("Hash"), 1);
    }

    #[test]
    fn a_malformed_value_is_rejected_without_changing_anything() {
        let mut o = Options::default();
        assert!(!o.set("Threads", "many"));
        assert_eq!(o.spin("Threads"), 1);
        assert!(!o.set("Ponder", "yes"));
        assert!(!o.check("Ponder"));
    }

    #[test]
    fn an_unknown_option_is_reported_but_not_fatal() {
        let mut o = Options::default();
        assert!(!o.set("NoSuchOption", "1"));
        assert!(!o.contains("NoSuchOption"));
    }

    #[test]
    fn the_empty_marker_clears_a_string_option() {
        let mut o = Options::default();
        assert!(o.set("SyzygyPath", "/tables"));
        assert_eq!(o.text("SyzygyPath"), "/tables");
        assert!(o.set("SyzygyPath", "<empty>"));
        assert_eq!(o.text("SyzygyPath"), "");
    }

    #[test]
    fn a_button_accepts_any_value_and_holds_none() {
        let mut o = Options::default();
        assert!(o.set("Clear Hash", ""));
        assert!(o.set("Clear Hash", "anything"));
    }
}
