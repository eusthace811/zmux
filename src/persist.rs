//! Session layout persistence.
//!
//! Processes do not survive a daemon restart; directories do. We save the
//! layout tree plus each pane's last reported cwd, so restarting recreates the
//! same arrangement of shells in the same places (DESIGN.md §4).

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::layout::{Layout, Node, PaneId};

/// Bumped when the on-disk shape changes. An unreadable or older file is
/// discarded rather than guessed at - a wrong layout is worse than none.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,
    pub name: String,
    pub root: Node,
    pub focus: PaneId,
    /// Pane id -> last reported working directory (Windows form).
    pub cwds: HashMap<u32, String>,
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("zmux")
}

fn path_for(name: &str) -> PathBuf {
    // Session names reach the filesystem, so keep them to something safe.
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    state_dir().join(format!("session-{safe}.json"))
}

pub fn save(snap: &SessionSnapshot) -> Result<()> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = path_for(&snap.name);
    let json = serde_json::to_string_pretty(snap)?;
    // Write-then-rename: a crash mid-write must not leave a truncated file
    // that we would later try to restore from.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load(name: &str) -> Option<SessionSnapshot> {
    let raw = std::fs::read_to_string(path_for(name)).ok()?;
    let snap: SessionSnapshot = serde_json::from_str(&raw).ok()?;
    if snap.version != FORMAT_VERSION {
        return None;
    }
    // A layout with no panes would panic on restore.
    let l = Layout::from_saved(snap.root.clone(), snap.focus);
    if l.panes().is_empty() {
        return None;
    }
    Some(snap)
}

pub fn remove(name: &str) {
    let _ = std::fs::remove_file(path_for(name));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Dir;

    fn snapshot(name: &str) -> SessionSnapshot {
        let root = Node::Split {
            dir: Dir::Horizontal,
            ratio: 0.5,
            a: Box::new(Node::Leaf(PaneId(1))),
            b: Box::new(Node::Leaf(PaneId(2))),
        };
        let mut cwds = HashMap::new();
        cwds.insert(1u32, r"C:\work".to_string());
        SessionSnapshot {
            version: FORMAT_VERSION,
            name: name.into(),
            root,
            focus: PaneId(2),
            cwds,
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let s = snapshot("zmux-unit-roundtrip");
        save(&s).expect("save");
        let back = load(&s.name).expect("load");
        assert_eq!(back.focus, PaneId(2));
        assert_eq!(back.cwds.get(&1).map(String::as_str), Some(r"C:\work"));
        let l = Layout::from_saved(back.root, back.focus);
        assert_eq!(l.panes().len(), 2);
        remove(&s.name);
    }

    #[test]
    fn a_future_version_is_discarded_rather_than_guessed_at() {
        let mut s = snapshot("zmux-unit-version");
        s.version = FORMAT_VERSION + 1;
        save(&s).expect("save");
        assert!(load(&s.name).is_none(), "a newer format must not be restored");
        remove(&s.name);
    }

    #[test]
    fn corrupt_state_is_ignored() {
        let name = "zmux-unit-corrupt";
        std::fs::create_dir_all(state_dir()).ok();
        std::fs::write(super::path_for(name), b"{ this is not json").ok();
        assert!(load(name).is_none());
        remove(name);
    }

    #[test]
    fn session_names_cannot_escape_the_state_directory() {
        let p = super::path_for("../../evil");
        assert!(p.starts_with(state_dir()), "path traversal via session name: {p:?}");
        assert!(!p.to_string_lossy().contains(".."));
    }
}
