//! A line editor for the prompt.
//!
//! # Why this exists rather than a dependency
//!
//! The help text used to say there was no line editing and that it was *"a
//! dependency not yet taken rather than an oversight"*. This is the third
//! answer: not the dependency, and not the absence either.
//!
//! The workspace has eight third-party crates in total, and `libc` — the only
//! thing this file needs — is already one of them, taken for the signal handler
//! in `shutdown.rs`. A line-editing crate would bring more crates than the whole
//! workspace has, for a prompt. So the editor is written here, against the two
//! interfaces it actually uses: `termios`, which has not changed in thirty
//! years, and the handful of ANSI sequences every terminal since 1979 answers.
//!
//! # What it is, and what it is not
//!
//! It is: history, the arrows, `Home`/`End`/`Delete`, the `readline` control
//! keys people's fingers already know (`Ctrl-A E B F K U W L P N`), `Ctrl-C` to
//! throw away a half-typed statement, and `Ctrl-D` to leave.
//!
//! It is not: completion, search, or reverse-i-search. Those want a model of the
//! language and of the catalog, and inventing half of one is worse than not
//! offering it — the tab key doing nothing is honest, and the tab key offering
//! the wrong four table names is not.
//!
//! # One line at a time, and the terminal handed back between them
//!
//! Raw mode is entered per keystroke-gathering call and left before the
//! statement runs. That is not tidiness. With `ISIG` cleared — which is what
//! stops `Ctrl-C` killing the process mid-edit — a query that takes a minute
//! would also be uninterruptible, and taking the terminal back for the duration
//! of the run is what keeps `Ctrl-C` meaning *stop this query* everywhere it
//! used to.
//!
//! # A character is not a column
//!
//! Cursor positions here are in `char`s, so a combining mark or a
//! double-width glyph draws where this file thinks one column is and the
//! terminal thinks otherwise. The cost is a redraw that looks wrong until the
//! next `Ctrl-L`, and the fix is a Unicode width table — a data dependency for a
//! prompt. Stated rather than fixed.

use std::io::{self, BufRead, Read, Write};

use crate::raw;
use crate::session::{Given, Lines};

/// How many finished lines are remembered.
///
/// A bound rather than none, because a session that runs for weeks against a
/// node is exactly the one somebody leaves open. Nothing is written to disk:
/// statements carry passwords (`DEFINE USER … PASSWORD '…'`), and a history file
/// is how one ends up on a backup nobody was thinking about.
const REMEMBERED: usize = 500;

/// A prompt with editing, reading this process's terminal.
pub struct Edited {
    /// Where keystrokes come from.
    input: io::StdinLock<'static>,
    /// The lines this session has finished, oldest first.
    history: Vec<String>,
}

impl Edited {
    /// A prompt for this terminal, or `None` where there is not one.
    pub fn attach() -> Option<Self> {
        // The probe is the check: a descriptor that answers `tcgetattr` is a
        // terminal, and one that does not is the same case as no terminal at
        // all. Asking twice, in two different ways, is how the two answers get
        // to disagree.
        let settled = raw::enter()?;
        drop(settled);
        Some(Self {
            input: io::stdin().lock(),
            history: Vec::new(),
        })
    }

    /// The next byte, or `None` at end of input.
    fn byte(&mut self) -> io::Result<Option<u8>> {
        let mut one = [0_u8; 1];
        match self.input.read(&mut one)? {
            0 => Ok(None),
            _ => Ok(Some(one[0])),
        }
    }

    /// Remember a finished line, unless it is empty or the one before it.
    fn remember(&mut self, line: &str) {
        if line.trim().is_empty() || self.history.last().is_some_and(|last| last == line) {
            return;
        }
        if self.history.len() >= REMEMBERED {
            self.history.remove(0);
        }
        self.history.push(line.to_owned());
    }

    /// Gather one line, with the terminal in raw mode for the duration.
    ///
    /// `masked` draws nothing where the characters go and remembers nothing
    /// afterwards. It is for a passphrase, and it is both halves or neither: a
    /// value hidden on screen and then put in the history is readable by the
    /// next person who presses the up arrow, which is the same room.
    fn edit(&mut self, prompt: &str, out: &mut dyn Write, masked: bool) -> io::Result<Given> {
        let mut line: Vec<char> = Vec::new();
        let mut at = 0_usize;
        // Where in the history the arrows have walked to, and the line that was
        // being typed before they started — without the second one, walking up
        // and back down loses what you had written.
        let mut browsing: Option<usize> = None;
        let mut stashed: Vec<char> = Vec::new();
        let mut pending: Option<u8> = None;

        draw(out, prompt, &line, at, masked)?;
        loop {
            let byte = match pending.take() {
                Some(byte) => byte,
                None => match self.byte()? {
                    Some(byte) => byte,
                    None => return Ok(Given::Ended),
                },
            };
            match byte {
                // Return. `ICRNL` is cleared, so this arrives as carriage
                // return and `Ctrl-J` arrives as its own byte; both end a line,
                // because a terminal that sends one and a paste that sends the
                // other mean the same thing.
                b'\r' | b'\n' => {
                    write!(out, "\r\n")?;
                    out.flush()?;
                    let text: String = line.iter().collect();
                    if !masked {
                        self.remember(&text);
                    }
                    return Ok(Given::Line(text + "\n"));
                }
                // Ctrl-C — the half-typed statement goes, the session stays.
                0x03 => {
                    write!(out, "^C\r\n")?;
                    out.flush()?;
                    return Ok(Given::Abandon);
                }
                // Ctrl-D — end of input on an empty line, delete-forward on one
                // that is not, which is what every other shell does.
                0x04 => {
                    if line.is_empty() {
                        return Ok(Given::Ended);
                    }
                    if at < line.len() {
                        line.remove(at);
                    }
                }
                // Backspace, and the terminals that send the other one.
                0x7f | 0x08 => {
                    if at > 0 {
                        at = at.saturating_sub(1);
                        line.remove(at);
                    }
                }
                0x01 => at = 0,
                0x05 => at = line.len(),
                0x02 => at = at.saturating_sub(1),
                0x06 => at = line.len().min(at.saturating_add(1)),
                0x0b => line.truncate(at),
                0x15 => {
                    line.drain(..at);
                    at = 0;
                }
                0x17 => at = word_back(&mut line, at),
                0x0c => {
                    write!(out, "\x1b[H\x1b[2J")?;
                }
                0x10 => back(
                    &self.history,
                    &mut browsing,
                    &mut line,
                    &mut stashed,
                    &mut at,
                ),
                0x0e => forward(
                    &self.history,
                    &mut browsing,
                    &mut line,
                    &mut stashed,
                    &mut at,
                ),
                0x1b => {
                    let Some(next) = self.byte()? else {
                        return Ok(Given::Ended);
                    };
                    if next != b'[' && next != b'O' {
                        // A bare Escape followed by an ordinary key: the key is
                        // the one that was meant, so it is handled rather than
                        // eaten.
                        pending = Some(next);
                        continue;
                    }
                    let Some(mut what) = self.byte()? else {
                        return Ok(Given::Ended);
                    };
                    if what.is_ascii_digit() {
                        let Some(tail) = self.byte()? else {
                            return Ok(Given::Ended);
                        };
                        // `\x1b[3~` and friends. Anything else with a number in
                        // it is a key this editor has no answer for.
                        if tail != b'~' {
                            continue;
                        }
                        what = match what {
                            b'1' | b'7' => b'H',
                            b'4' | b'8' => b'F',
                            b'3' => {
                                if at < line.len() {
                                    line.remove(at);
                                }
                                draw(out, prompt, &line, at, masked)?;
                                continue;
                            }
                            _ => continue,
                        };
                    }
                    match what {
                        b'A' => back(
                            &self.history,
                            &mut browsing,
                            &mut line,
                            &mut stashed,
                            &mut at,
                        ),
                        b'B' => {
                            forward(
                                &self.history,
                                &mut browsing,
                                &mut line,
                                &mut stashed,
                                &mut at,
                            );
                        }
                        b'C' => at = line.len().min(at.saturating_add(1)),
                        b'D' => at = at.saturating_sub(1),
                        b'H' => at = 0,
                        b'F' => at = line.len(),
                        _ => continue,
                    }
                }
                // Anything else printable, including the multi-byte ones.
                _ => {
                    if let Some(typed) = self.character(byte)? {
                        line.insert(at, typed);
                        at = at.saturating_add(1);
                    }
                }
            }
            draw(out, prompt, &line, at, masked)?;
        }
    }

    /// One character, gathering the continuation bytes a lead byte promises.
    ///
    /// `None` for a control byte this editor has no answer for — swallowed
    /// rather than inserted, because a stray `0x00` in a statement is a refusal
    /// somewhere far from where it was typed.
    fn character(&mut self, lead: u8) -> io::Result<Option<char>> {
        let following = match lead {
            0x00..=0x1f => return Ok(None),
            0x20..=0x7f => 0,
            0xc0..=0xdf => 1,
            0xe0..=0xef => 2,
            0xf0..=0xf7 => 3,
            _ => return Ok(None),
        };
        let mut bytes = [lead, 0, 0, 0];
        for slot in bytes.iter_mut().skip(1).take(following) {
            match self.byte()? {
                Some(byte) => *slot = byte,
                None => return Ok(None),
            }
        }
        let held = bytes.get(..following.saturating_add(1)).unwrap_or(&bytes);
        Ok(std::str::from_utf8(held)
            .ok()
            .and_then(|text| text.chars().next()))
    }
}

impl Lines for Edited {
    fn next(&mut self, prompt: &str, out: &mut dyn Write) -> io::Result<Given> {
        let Some(settled) = raw::enter() else {
            // The terminal stopped answering mid-session. Reading on without
            // editing is a worse prompt; refusing to read is no prompt at all.
            let mut text = String::new();
            write!(out, "{prompt}")?;
            out.flush()?;
            return Ok(match self.input.read_line(&mut text)? {
                0 => Given::Ended,
                _ => Given::Line(text),
            });
        };
        let gathered = self.edit(prompt, out, false);
        drop(settled);
        gathered
    }

    fn secret(&mut self, prompt: &str, out: &mut dyn Write) -> io::Result<Given> {
        let Some(settled) = raw::enter() else {
            // No raw mode means no way to stop the terminal echoing, so this
            // refuses rather than reading a passphrase onto the screen. The
            // caller says so and the statement can still be typed in full by
            // somebody who accepts that.
            return Ok(Given::Abandon);
        };
        let gathered = self.edit(prompt, out, true);
        drop(settled);
        gathered
    }
}

/// Draw the prompt and the line, and leave the cursor where it belongs.
///
/// A line too long for the terminal **scrolls sideways** rather than wrapping.
/// Wrapping would be nicer to look at and would need this function to know how
/// many rows it drew last time, which is a piece of state that goes wrong the
/// first time the window is resized between two keystrokes.
fn draw(
    out: &mut dyn Write,
    prompt: &str,
    line: &[char],
    at: usize,
    masked: bool,
) -> io::Result<()> {
    let width = prompt.chars().count();
    // One column is kept free at the right edge: writing into the last one makes
    // some terminals wrap and others not, and a redraw cannot tell which
    // happened.
    let room = raw::columns()
        .saturating_sub(width)
        .saturating_sub(1)
        .max(1);
    let start = at.saturating_sub(room.saturating_sub(1)).min(line.len());
    let end = line.len().min(start.saturating_add(room));
    let held = line.get(start..end).unwrap_or_default();
    // A row of dots rather than the characters, and one per character rather
    // than a fixed run: the editing keys still have to land where the caller
    // thinks they are, so the drawn width has to match the real one. The length
    // is disclosed and that is accepted — hiding it would mean the cursor and
    // the text disagreeing, and a passphrase you cannot correct is one people
    // paste from somewhere worse.
    let visible: String = if masked {
        "•".repeat(held.len())
    } else {
        held.iter().collect()
    };
    write!(out, "\r{prompt}{visible}\x1b[K\r")?;
    let column = width.saturating_add(at.saturating_sub(start));
    if column > 0 {
        write!(out, "\x1b[{column}C")?;
    }
    out.flush()
}

/// Delete the word before the cursor, and say where the cursor lands.
fn word_back(line: &mut Vec<char>, at: usize) -> usize {
    let mut start = at;
    while start > 0 && line.get(start.saturating_sub(1)).is_some_and(|c| *c == ' ') {
        start = start.saturating_sub(1);
    }
    while start > 0 && line.get(start.saturating_sub(1)).is_some_and(|c| *c != ' ') {
        start = start.saturating_sub(1);
    }
    line.drain(start..at);
    start
}

/// Walk one step further back through the history.
fn back(
    history: &[String],
    browsing: &mut Option<usize>,
    line: &mut Vec<char>,
    stashed: &mut Vec<char>,
    at: &mut usize,
) {
    let next = match *browsing {
        None => {
            if history.is_empty() {
                return;
            }
            *stashed = line.clone();
            history.len().saturating_sub(1)
        }
        Some(0) => return,
        Some(index) => index.saturating_sub(1),
    };
    *browsing = Some(next);
    *line = history
        .get(next)
        .map(|held| held.chars().collect())
        .unwrap_or_default();
    *at = line.len();
}

/// Walk one step forward, back towards the line that was being typed.
fn forward(
    history: &[String],
    browsing: &mut Option<usize>,
    line: &mut Vec<char>,
    stashed: &mut Vec<char>,
    at: &mut usize,
) {
    let Some(index) = *browsing else { return };
    let next = index.saturating_add(1);
    if next >= history.len() {
        *browsing = None;
        *line = core::mem::take(stashed);
    } else {
        *browsing = Some(next);
        *line = history
            .get(next)
            .map(|held| held.chars().collect())
            .unwrap_or_default();
    }
    *at = line.len();
}

#[cfg(test)]
mod tests {
    use super::{back, forward, word_back};

    fn chars(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    #[test]
    fn a_word_delete_takes_the_spaces_before_the_word_with_it() {
        let mut line = chars("SELECT * FROM   users");
        let at = line.len();
        let landed = word_back(&mut line, at);
        assert_eq!(line.iter().collect::<String>(), "SELECT * FROM   ");
        assert_eq!(landed, line.len());
        // And again, so the run of spaces is what the second one crosses.
        let landed = word_back(&mut line, landed);
        assert_eq!(line.iter().collect::<String>(), "SELECT * ");
        assert_eq!(landed, line.len());
    }

    #[test]
    fn walking_up_and_back_down_returns_the_line_that_was_being_typed() {
        // The failure this exists for is silent: the arrows work, the history
        // is right, and the half-typed statement is gone.
        let history = vec![
            "SELECT * FROM users;".to_owned(),
            "INFO FOR NODE;".to_owned(),
        ];
        let mut browsing = None;
        let mut line = chars("CREATE users:1 = {");
        let mut stashed = Vec::new();
        let mut at = line.len();

        back(&history, &mut browsing, &mut line, &mut stashed, &mut at);
        assert_eq!(line.iter().collect::<String>(), "INFO FOR NODE;");
        back(&history, &mut browsing, &mut line, &mut stashed, &mut at);
        assert_eq!(line.iter().collect::<String>(), "SELECT * FROM users;");
        // Past the oldest is a stop, not a wrap.
        back(&history, &mut browsing, &mut line, &mut stashed, &mut at);
        assert_eq!(line.iter().collect::<String>(), "SELECT * FROM users;");

        forward(&history, &mut browsing, &mut line, &mut stashed, &mut at);
        forward(&history, &mut browsing, &mut line, &mut stashed, &mut at);
        assert_eq!(line.iter().collect::<String>(), "CREATE users:1 = {");
        assert_eq!(at, line.len());
    }

    #[test]
    fn the_arrows_do_nothing_at_all_with_no_history() {
        let history: Vec<String> = Vec::new();
        let mut browsing = None;
        let mut line = chars("SELECT");
        let mut stashed = Vec::new();
        let mut at = line.len();
        back(&history, &mut browsing, &mut line, &mut stashed, &mut at);
        forward(&history, &mut browsing, &mut line, &mut stashed, &mut at);
        assert_eq!(line.iter().collect::<String>(), "SELECT");
        assert!(browsing.is_none());
    }
}
