//! User configuration, read from `%APPDATA%\zmux\config.toml`.
//!
//! Every field is optional and every default is the behaviour that shipped
//! before the file existed, so a missing or partial config changes nothing.
//! A malformed file is reported and ignored rather than being fatal - losing
//! your multiplexer because of a stray comma is not acceptable.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Prefix key, as a single character. Paired with Ctrl.
    pub prefix: char,
    /// Full path to the shell. Empty means "detect Git Bash".
    pub shell: String,
    /// Extra arguments appended to the shell command line.
    pub shell_args: Vec<String>,
    /// Lines of scrollback kept per pane.
    pub scrollback: usize,
    /// Emit OSC 7 shell integration for working-directory tracking.
    pub shell_integration: bool,
    /// Hours a daemon lingers with no clients attached before exiting.
    pub idle_timeout_hours: u64,
    /// Show the status bar. Disabling it gives panes one more row.
    pub status_bar: bool,
    /// Who the mouse belongs to.
    pub mouse: Mouse,
    /// Longest a control caller may ask the daemon to block in one `wait`.
    ///
    /// A wait holds a connection handler thread. A caller that asks to wait
    /// forever and then dies leaks one until the daemon exits, so there is a
    /// ceiling; the caller gets `timeout`, which it must handle anyway, and
    /// can simply ask again.
    pub wait_timeout_max_ms: u64,
}

/// Who gets the mouse: zmux, or the terminal zmux is running in.
///
/// Only one of them can have it. While zmux reports mouse events, the host
/// terminal stops interpreting them, which is why dragging out a selection
/// does nothing - Windows Terminal never sees the drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mouse {
    /// Whoever is using it. zmux holds the mouse only while the program in
    /// the focused pane has asked for it, so vim and lazygit get a mouse and
    /// a shell prompt leaves you free to select text.
    #[default]
    Auto,
    /// zmux takes the mouse always: click focuses a pane, the wheel scrolls
    /// history. Selecting text needs Shift held down.
    On,
    /// The terminal keeps the mouse always. Selection works as it does in any
    /// other window; nothing inside zmux sees a mouse, programs included.
    Off,
}

impl Mouse {
    /// Should zmux be reporting mouse events right now?
    ///
    /// `app` is whether the program in the focused pane has asked for the
    /// mouse - which only `Auto` cares about.
    pub fn captures(self, app: bool) -> bool {
        match self {
            Mouse::Auto => app,
            Mouse::On => true,
            Mouse::Off => false,
        }
    }

    /// What `C-b m` switches to. Cycles, because there are three of them.
    pub fn toggled(self) -> Self {
        match self {
            Mouse::Auto => Mouse::On,
            Mouse::On => Mouse::Off,
            Mouse::Off => Mouse::Auto,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            prefix: 'b',
            shell: String::new(),
            shell_args: Vec::new(),
            scrollback: 10_000,
            shell_integration: true,
            idle_timeout_hours: 8,
            status_bar: true,
            mouse: Mouse::default(),
            wait_timeout_max_ms: 600_000,
        }
    }
}

pub fn path() -> PathBuf {
    crate::persist::state_dir().join("config.toml")
}

impl Config {
    /// Load the config, falling back to defaults on anything unexpected.
    pub fn load() -> Self {
        let p = path();
        let raw = match std::fs::read_to_string(&p) {
            Ok(r) => r,
            Err(_) => return Self::default(), // absent is the normal case
        };
        match toml::from_str::<Config>(&raw) {
            Ok(c) => c.sanitised(),
            Err(e) => {
                eprintln!("[zmux] ignoring {}: {e}", p.display());
                Self::default()
            }
        }
    }

    /// Clamp values that would break the session if taken literally.
    fn sanitised(mut self) -> Self {
        let d = Config::default();
        // An unbounded scrollback is a memory leak with extra steps.
        self.scrollback = self.scrollback.min(1_000_000);
        if !self.prefix.is_ascii() || self.prefix.is_control() {
            eprintln!("[zmux] prefix must be a printable ASCII character; using Ctrl-{}", d.prefix);
            self.prefix = d.prefix;
        }
        if self.idle_timeout_hours == 0 {
            self.idle_timeout_hours = d.idle_timeout_hours;
        }
        self
    }

    /// The control byte the prefix key produces, e.g. 'b' -> 0x02.
    pub fn prefix_byte(&self) -> u8 {
        (self.prefix.to_ascii_uppercase() as u8).wrapping_sub(b'@')
    }

    /// Write a commented default config, without clobbering an existing one.
    pub fn write_default() -> std::io::Result<PathBuf> {
        let p = path();
        if p.exists() {
            return Ok(p);
        }
        std::fs::create_dir_all(crate::persist::state_dir())?;
        std::fs::write(&p, DEFAULT_TOML)?;
        Ok(p)
    }
}

pub const DEFAULT_TOML: &str = r#"# zmux configuration.
# Every setting is optional; the values shown are the defaults.

# Prefix key, combined with Ctrl. "b" means Ctrl-b, as in tmux.
prefix = "b"

# Shell to run in each pane. Empty means "find Git Bash automatically".
# shell = "C:\\Program Files\\Git\\bin\\bash.exe"
shell = ""

# Extra arguments appended to the shell command line.
shell_args = []

# Lines of scrollback kept per pane.
scrollback = 10000

# Emit OSC 7 so panes track their working directory and new panes inherit it.
# Turning this off launches a plain login shell instead.
shell_integration = true

# Hours an unattended daemon stays alive before exiting.
idle_timeout_hours = 8

# Show the status bar. Turning it off gives panes one more row.
status_bar = true

# Who the mouse belongs to. Only one of zmux and the terminal can have it:
# while zmux is reporting mouse events, dragging out a selection does
# nothing, because the terminal never sees the drag.
#   "auto"  zmux holds it only while the program in the focused pane has
#           asked for one, so vim gets a mouse and a shell prompt does not
#   "on"    zmux takes it: click focuses a pane, the wheel scrolls history
#   "off"   the terminal keeps it, and nothing inside zmux sees a mouse
# `C-b m` cycles through them for the session without editing this file.
mouse = "auto"

# Ceiling on how long `zmux wait` may block the daemon in one call.
#
# A wait holds one of the daemon's connection threads, so a caller that asks
# to wait forever and then dies would leak it. At the ceiling the caller is
# told `timeout` - which it has to handle anyway - and can ask again.
wait_timeout_max_ms = 600000
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_shipped_behaviour() {
        let c = Config::default();
        assert_eq!(c.prefix, 'b');
        assert_eq!(c.prefix_byte(), 0x02);
        assert_eq!(c.scrollback, 10_000);
        assert!(c.shell_integration);
    }

    /// Only `Auto` asks what the program wants; the other two have already
    /// decided, and must not start following a pane that turns mouse
    /// reporting on.
    #[test]
    fn only_auto_follows_the_program() {
        assert_eq!(Config::default().mouse, Mouse::Auto);
        for (mode, idle, app) in [
            (Mouse::Auto, false, true),
            (Mouse::On, true, true),
            (Mouse::Off, false, false),
        ] {
            assert_eq!(mode.captures(false), idle, "{mode:?} with nothing asking");
            assert_eq!(mode.captures(true), app, "{mode:?} with a program asking");
        }
    }

    #[test]
    fn the_toggle_gets_you_back_where_you_started() {
        for m in [Mouse::Auto, Mouse::On, Mouse::Off] {
            assert_ne!(m.toggled(), m);
            assert_eq!(m.toggled().toggled().toggled(), m);
        }
    }

    #[test]
    fn the_mouse_setting_is_spelled_the_way_the_comment_says() {
        let c: Config = toml::from_str("mouse = \"off\"").expect("lowercase names");
        assert_eq!(c.mouse, Mouse::Off);
        assert_eq!(toml::from_str::<Config>("mouse = \"auto\"").unwrap().mouse, Mouse::Auto);
        assert!(toml::from_str::<Config>("mouse = \"sometimes\"").is_err());
    }

    #[test]
    fn the_documented_default_file_actually_parses() {
        // A default config that does not round-trip would be a trap for the
        // first user who opens it.
        let c: Config = toml::from_str(DEFAULT_TOML).expect("shipped default must parse");
        assert_eq!(c.prefix, Config::default().prefix);
        assert_eq!(c.scrollback, Config::default().scrollback);
    }

    #[test]
    fn a_partial_file_keeps_the_other_defaults() {
        let c: Config = toml::from_str("scrollback = 42").expect("parse");
        assert_eq!(c.scrollback, 42);
        assert_eq!(c.prefix, 'b', "unset fields must keep their defaults");
    }

    #[test]
    fn an_unknown_key_is_reported_rather_than_silently_ignored() {
        // Silently accepting typos means a setting that "does nothing" with no
        // explanation - worse than an error.
        assert!(toml::from_str::<Config>("prefx = \"a\"").is_err());
    }

    #[test]
    fn absurd_values_are_clamped() {
        let c = Config { scrollback: usize::MAX, idle_timeout_hours: 0, ..Default::default() };
        let c = c.sanitised();
        assert!(c.scrollback <= 1_000_000, "unbounded scrollback is a memory leak");
        assert!(c.idle_timeout_hours > 0);
    }

    #[test]
    fn a_control_character_prefix_is_rejected() {
        let c = Config { prefix: '\u{1}', ..Default::default() }.sanitised();
        assert_eq!(c.prefix, 'b');
    }

    #[test]
    fn prefix_byte_maps_letters_to_control_codes() {
        assert_eq!(Config { prefix: 'a', ..Default::default() }.prefix_byte(), 0x01);
        assert_eq!(Config { prefix: 'b', ..Default::default() }.prefix_byte(), 0x02);
        assert_eq!(Config { prefix: 'z', ..Default::default() }.prefix_byte(), 0x1a);
    }
}
