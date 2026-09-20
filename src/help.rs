//! The shortcut list, in one place.
//!
//! Three things have to agree about the bindings: the key table in the
//! client, `--help`, and the overlay `C-b ?` puts on screen. Two of them were
//! already out of step - `C-b [` and `C-b PageUp` worked for months without
//! appearing in `--help` - so the text lives here and the other two are
//! generated from it.

/// Every prefix shortcut: the key that follows the prefix, and what it does.
///
/// `{p}` stands in for the configured prefix character, since it is not
/// always `b`.
pub const SHORTCUTS: &[(&str, &str)] = &[
    ("d", "detach; the session keeps running"),
    ("%", "split left / right"),
    ("\"", "split top / bottom"),
    ("x", "close the pane, and its window if it was the last"),
    ("← → ↑ ↓", "move focus"),
    ("C-← → ↑ ↓", "resize the focused pane"),
    ("c", "new window"),
    ("n / p", "next / previous window"),
    ("0..9", "jump to that window, by the number on the status bar"),
    ("[", "scroll history; q or Esc leaves"),
    ("PageUp", "scroll back a page without entering scroll mode"),
    ("m", "cycle who owns the mouse: auto / zmux / the terminal"),
    ("?", "this list"),
    ("C-{p}", "send a literal Ctrl-{p}"),
];

/// How the prefix itself is written, e.g. `C-b`.
pub fn prefix_label(prefix: char) -> String {
    format!("C-{}", prefix.to_ascii_lowercase())
}

/// The shortcut rows, as `(key, what it does)` with the prefix filled in.
pub fn rows(prefix: char) -> Vec<(String, String)> {
    let p = prefix.to_ascii_lowercase().to_string();
    SHORTCUTS
        .iter()
        .map(|(k, d)| {
            (
                format!("{} {}", prefix_label(prefix), k.replace("{p}", &p)),
                d.replace("{p}", &p),
            )
        })
        .collect()
}

/// The lines of the on-screen list, already aligned and sized to fit.
///
/// `width` and `height` are the whole box, border included. A list that is
/// cut says so and points at `--help`, because a box that silently stops
/// after ten of fourteen bindings is worse than one that admits it: you would
/// go looking for `C-b ?` again rather than somewhere that has the rest.
pub fn lines(prefix: char, width: usize, height: usize) -> Vec<String> {
    let all = rows(prefix);
    // Two rows go to the border, and a header, a blank and a footer to us.
    let room = height.saturating_sub(6);
    if room == 0 {
        // No room to list anything. One line that says where they are beats
        // a box with a header and nothing under it.
        return vec![cut(format!("{} shortcuts: zmux --help", all.len()), width)];
    }
    let keys = all.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    let mut out = vec![
        format!("zmux shortcuts - {} is the prefix", prefix_label(prefix)),
        String::new(),
    ];
    let shown = if all.len() > room { room.saturating_sub(1) } else { all.len() };
    for (k, d) in all.iter().take(shown) {
        out.push(format!("{k:<keys$}   {d}"));
    }
    if shown < all.len() {
        out.push(format!("... and {} more: zmux --help", all.len() - shown));
    }
    out.push(String::new());
    out.push("any key closes this".to_string());
    for l in &mut out {
        *l = cut(std::mem::take(l), width);
    }
    out
}

/// Cut a line to `width`, saying it was cut rather than stopping mid-word as
/// though that were the whole line.
fn cut(line: String, width: usize) -> String {
    if line.chars().count() <= width {
        return line;
    }
    line.chars().take(width.saturating_sub(1)).collect::<String>() + "\u{2026}"
}

/// The `In a session` block of `--help`.
pub fn usage_block(prefix: char) -> String {
    let rows = rows(prefix);
    let keys = rows.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    let mut s = format!("In a session ({} is the prefix):\n", prefix_label(prefix));
    for (k, d) in rows {
        s.push_str(&format!("  {k:<keys$}   {d}\n"));
    }
    s.pop();
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The prefix is configurable, so help that says `C-b` to somebody who
    /// set `prefix = "a"` is worse than no help.
    #[test]
    fn the_help_speaks_of_the_prefix_you_actually_configured() {
        let rows = rows('a');
        assert!(rows.iter().all(|(k, _)| k.starts_with("C-a ")), "{rows:?}");
        let literal = rows.last().expect("the literal-prefix row");
        assert_eq!(literal.0, "C-a C-a");
        assert_eq!(literal.1, "send a literal Ctrl-a");
    }

    /// `--help` and the overlay describe the same bindings because they are
    /// the same list. This is the drift that let `C-b [` go undocumented.
    #[test]
    fn help_and_the_overlay_cannot_disagree() {
        let block = usage_block('b');
        for (k, d) in rows('b') {
            assert!(block.contains(&k), "{k} missing from --help");
            assert!(block.contains(&d), "{d} missing from --help");
        }
        let lines = lines('b', 200, 40).join("\n");
        for (k, _) in rows('b') {
            assert!(lines.contains(&k), "{k} missing from the overlay");
        }
    }

    /// A box that runs off the screen loses its own border.
    #[test]
    fn a_narrow_screen_cuts_the_lines_rather_than_the_box() {
        for line in lines('b', 20, 40) {
            assert!(line.chars().count() <= 20, "{line:?}");
        }
    }

    /// A short screen must not silently stop listing. Every size has to fit
    /// in the box it was given, and any size that cannot show everything has
    /// to say where the rest is.
    #[test]
    fn a_short_screen_says_what_it_left_out() {
        for height in 3..24usize {
            let l = lines('b', 200, height);
            assert!(l.len() + 2 <= height, "{height} rows: got {} lines", l.len());
            let complete = l.iter().filter(|s| s.starts_with("C-b ")).count() == SHORTCUTS.len();
            let points_elsewhere = l.iter().any(|s| s.contains("zmux --help"));
            assert_ne!(
                complete, points_elsewhere,
                "{height} rows: a cut list must say where the rest is, a whole one must not"
            );
        }
    }

    /// Every binding the client dispatches should be in here. Checked by
    /// hand against `read_prefix_command`, and pinned so adding one without
    /// documenting it fails.
    #[test]
    fn every_binding_is_listed() {
        let keys: Vec<&str> = SHORTCUTS.iter().map(|(k, _)| *k).collect();
        for want in ["d", "%", "\"", "x", "c", "n / p", "0..9", "[", "PageUp", "m", "?", "C-{p}"] {
            assert!(keys.contains(&want), "{want} is bound but not listed");
        }
        assert_eq!(keys.len(), 14, "a binding was added or removed; update the list");
    }
}
