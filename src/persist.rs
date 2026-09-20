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
use crate::windows::Windows;

/// Bumped when the on-disk shape changes. An unreadable or NEWER file is
/// discarded rather than guessed at - a wrong layout is worse than none.
///
/// v2 replaced the single `root`/`focus` pair with a list of windows.
pub const FORMAT_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,
    pub name: String,
    pub windows: Windows,
    /// Pane id -> last reported working directory (Windows form).
    pub cwds: HashMap<u32, String>,
}

/// The v1 shape: one layout, no windows.
///
/// Kept so an upgrade does not silently discard the arrangement someone has
/// been using. This is a mechanical translation, not a guess - a v1 session is
/// exactly a v2 session with one window - which is why it is worth doing when
/// discarding a file we cannot interpret is otherwise the rule.
#[derive(Deserialize)]
struct SnapshotV1 {
    name: String,
    root: Node,
    focus: PaneId,
    cwds: HashMap<u32, String>,
}

impl SnapshotV1 {
    fn upgrade(self) -> SessionSnapshot {
        SessionSnapshot {
            version: FORMAT_VERSION,
            name: self.name,
            windows: Windows::new(Layout::from_saved(self.root, self.focus)),
            cwds: self.cwds,
        }
    }
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
    // that we would later try to restore from. The temp name includes the
    // thread id because two savers sharing one .tmp path would interleave and
    // rename each other's partial writes.
    let tmp = path.with_extension(format!("{:?}.tmp", std::thread::current().id()).replace(['(', ')', ' '], ""));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load(name: &str) -> Option<SessionSnapshot> {
    let raw = std::fs::read_to_string(path_for(name)).ok()?;
    // Read the version FIRST and dispatch on it. Trying each shape in turn and
    // taking whichever parses would let a half-matching file be read as the
    // wrong format, which is precisely the guessing this module avoids.
    let version = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()?
        .get("version")?
        .as_u64()? as u32;
    let snap = match version {
        FORMAT_VERSION => serde_json::from_str::<SessionSnapshot>(&raw).ok()?,
        1 => serde_json::from_str::<SnapshotV1>(&raw).ok()?.upgrade(),
        _ => return None, // a FUTURE version; we cannot know what it means
    };
    // Everything above came off disk, so it is repaired - not trusted - before
    // anyone indexes into it or allocates a pane id.
    Some(SessionSnapshot { windows: snap.windows.repaired()?, ..snap })
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
            windows: Windows::new(Layout::from_saved(root, PaneId(2))),
            cwds,
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let s = snapshot("zmux-unit-roundtrip");
        save(&s).expect("save");
        let back = load(&s.name).expect("load");
        assert_eq!(back.windows.current().focus(), PaneId(2));
        assert_eq!(back.cwds.get(&1).map(String::as_str), Some(r"C:\work"));
        assert_eq!(back.windows.current().panes().len(), 2);
        remove(&s.name);
    }

    #[test]
    fn several_windows_round_trip() {
        let name = "zmux-unit-windows";
        let mut s = snapshot(name);
        s.windows.push(Layout::new(PaneId(7)));
        s.windows.select_previous();
        save(&s).expect("save");
        let back = load(name).expect("load");
        assert_eq!(back.windows.len(), 2);
        assert_eq!(back.windows.index(), 0, "the current window is part of the state");
        assert_eq!(back.windows.panes().len(), 3);
        remove(name);
    }

    /// Upgrading must not silently discard the arrangement someone was using.
    #[test]
    fn a_v1_file_is_migrated_to_one_window() {
        let name = "zmux-unit-v1";
        std::fs::create_dir_all(state_dir()).ok();
        let v1 = format!(
            r#"{{"version":1,"name":"{name}","root":{{"Leaf":3}},"focus":3,"cwds":{{"3":"C:\\v1"}}}}"#
        );
        std::fs::write(path_for(name), v1).expect("write v1 file");
        let back = load(name).expect("a v1 file must still be readable");
        assert_eq!(back.windows.len(), 1, "a v1 session is one window");
        assert_eq!(back.windows.current().panes(), vec![PaneId(3)]);
        assert_eq!(back.cwds.get(&3).map(String::as_str), Some(r"C:\v1"),
                   "directories are the point of persistence; they must survive the upgrade");
        remove(name);
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

    /// A file can name a window that does not exist. Indexing it would panic
    /// on the first frame, taking the daemon down at startup.
    #[test]
    fn an_out_of_range_current_window_is_repaired_not_trusted() {
        let name = "zmux-unit-badcurrent";
        std::fs::create_dir_all(state_dir()).ok();
        let bad = format!(
            r#"{{"version":2,"name":"{name}","windows":{{"list":[{{"root":{{"Leaf":4}},"focus":4}}],"current":9}},"cwds":{{}}}}"#
        );
        std::fs::write(path_for(name), bad).expect("write");
        let back = load(name).expect("repairable, not discarded");
        assert_eq!(back.windows.index(), 0);
        remove(name);
    }

    #[test]
    fn session_names_cannot_escape_the_state_directory() {
        let p = super::path_for("../../evil");
        assert!(p.starts_with(state_dir()), "path traversal via session name: {p:?}");
        assert!(!p.to_string_lossy().contains(".."));
    }
}
