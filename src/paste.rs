//! Submitting text to a program that is reading it as a prompt.
//!
//! Typing a multi-line prompt into a TUI one byte at a time is not the same
//! thing as pasting it. Every embedded newline is a submit, so a three-line
//! prompt becomes three turns, the first two of them fragments. Bracketed
//! paste (DEC 2004) is the fix every terminal already implements: the text
//! arrives between markers and the program knows it is one block.

/// The markers. `200~` opens a paste, `201~` ends it.
const START: &[u8] = b"\x1b[200~";
const END: &[u8] = b"\x1b[201~";

/// Build the bytes to write for one submission.
///
/// `bracketed` must already be the ANSWER - whether this pane's program has
/// asked for bracketed paste - not the caller's wish for it. A program that
/// never asked would be handed the markers as literal text.
///
/// Enter goes OUTSIDE the paste and in the SAME buffer. Inside, it is part of
/// the pasted text and submits nothing; in a second write, the program can
/// observe the prompt without the keystroke that sends it, and a TUI that
/// redraws in between can lose it.
pub fn wrap(data: &[u8], bracketed: bool, enter: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + START.len() + END.len() + 1);
    if bracketed {
        out.extend_from_slice(START);
        out.extend_from_slice(&strip_end_marker(data));
        out.extend_from_slice(END);
    } else {
        out.extend_from_slice(data);
    }
    if enter {
        out.push(b'\r');
    }
    out
}

/// Remove any `ESC [ 201~` the payload contains.
///
/// Without this, a prompt that quotes an end marker - which is exactly what
/// "explain what \x1b[201~ does" looks like - closes the paste early and the
/// remainder is typed as live keystrokes. That is an injection: the tail of
/// somebody's text becomes commands in their shell. Dropped rather than
/// escaped, because there is no escape for it inside a paste.
fn strip_end_marker(data: &[u8]) -> Vec<u8> {
    if !contains(data, END) {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i..].starts_with(END) {
            i += END.len();
            continue;
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_that_did_not_ask_gets_the_text_unchanged() {
        assert_eq!(wrap(b"hello", false, false), b"hello".to_vec());
    }

    #[test]
    fn enter_rides_along_in_the_same_buffer() {
        assert_eq!(wrap(b"ls", false, true), b"ls\r".to_vec());
    }

    #[test]
    fn a_paste_is_bracketed_and_enter_stays_outside_it() {
        // Outside: inside, the newline is pasted text and submits nothing.
        assert_eq!(wrap(b"ls", true, true), b"\x1b[200~ls\x1b[201~\r".to_vec());
    }

    #[test]
    fn a_multi_line_prompt_survives_as_one_block() {
        let out = wrap(b"line one\nline two", true, true);
        assert_eq!(out, b"\x1b[200~line one\nline two\x1b[201~\r".to_vec());
    }

    /// The injection this exists to stop: text carrying an end marker would
    /// otherwise close the paste and run its own tail as keystrokes.
    #[test]
    fn an_end_marker_in_the_text_cannot_break_out_of_the_paste() {
        let out = wrap(b"what does \x1b[201~rm -rf / do", true, true);
        let body = &out[START.len()..out.len() - END.len() - 1];
        assert!(!contains(body, END), "the payload still closes the paste: {body:?}");
        assert_eq!(out.windows(END.len()).filter(|w| *w == END).count(), 1);
        assert!(contains(&out, b"rm -rf / do"), "the rest of the text was lost");
    }

    #[test]
    fn stripping_leaves_an_ordinary_escape_sequence_alone() {
        // Only the END marker goes. A start marker, or any other CSI, is
        // text as far as we are concerned.
        let out = wrap(b"\x1b[200~\x1b[31mred", true, false);
        assert!(contains(&out, b"\x1b[31mred"));
    }
}
