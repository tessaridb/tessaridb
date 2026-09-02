//! The prompt as somebody at a keyboard meets it.
//!
//! # Why this test exists at all
//!
//! Everything the line editor does, it does in answer to a keystroke on a
//! terminal, and `cargo test` has no terminal. So the unit tests next to the
//! editor can cover the history walk and the word delete — the parts that are
//! ordinary functions — and can cover **none** of what the editor is for: that a
//! key produces a character on screen, that the arrows move a cursor, that
//! `Ctrl-C` throws away a statement instead of killing the process, and that the
//! terminal is put back the way it was found.
//!
//! That last one is the one worth a whole test file. A terminal left in raw mode
//! is broken for the *shell that comes after this program* — no echo, no line
//! discipline, `reset` typed blind — and nothing about the failure points at the
//! program that caused it. It is exactly the class of defect that ships.
//!
//! # How
//!
//! A pty is opened, the shipped binary is started on it, keystrokes are written
//! to the master, and what the child drew is read back. The restore check is
//! made against the pty's **own settings**, read before the child starts and
//! again after it exits — the terminal itself is the witness, rather than the
//! program's account of what it did to it.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The binary this crate builds, which is the one an operator installs.
const TESSARIDB: &str = env!("CARGO_BIN_EXE_tessaridb");

/// How long any one expected thing is waited for.
///
/// Generous, because it is only ever reached when something is actually wrong:
/// the assertions are on text arriving, and text arrives in milliseconds.
const PATIENCE: Duration = Duration::from_secs(10);

/// The three flags that say whether a terminal is in raw mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Discipline {
    /// Lines are gathered by the terminal rather than by the program.
    canonical: bool,
    /// The terminal draws what is typed.
    echoing: bool,
    /// `Ctrl-C` is a signal rather than a byte.
    signalling: bool,
}

impl Discipline {
    /// How the terminal behind `fd` is set up right now.
    fn of(fd: i32) -> Self {
        let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(fd, settings.as_mut_ptr()) },
            0,
            "the pty would not say how it is set up"
        );
        let settings = unsafe { settings.assume_init() };
        Self {
            canonical: settings.c_lflag & libc::ICANON != 0,
            echoing: settings.c_lflag & libc::ECHO != 0,
            signalling: settings.c_lflag & libc::ISIG != 0,
        }
    }
}

/// A pty, and the two ends of it.
fn pty() -> (OwnedFd, OwnedFd) {
    let mut master = 0;
    let mut slave = 0;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0,
        "no pty could be opened"
    );
    // SAFETY: both descriptors were just created by `openpty` and are owned by
    // nothing else.
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

/// Everything the child writes, gathered by a thread until the pty closes.
///
/// A thread rather than a poll loop because the master reports the child's exit
/// as a read error on one platform and as end-of-input on another, and a thread
/// that simply stops is the one shape both agree on.
fn gather(master: File) -> Arc<Mutex<Vec<u8>>> {
    let held = Arc::new(Mutex::new(Vec::new()));
    let writing = Arc::clone(&held);
    std::thread::spawn(move || {
        let mut master = master;
        let mut chunk = [0_u8; 4096];
        while let Ok(read) = master.read(&mut chunk) {
            if read == 0 {
                break;
            }
            writing.lock().unwrap().extend_from_slice(&chunk[..read]);
        }
    });
    held
}

/// Wait until the transcript holds `needle` at least `times`, or say what it held.
///
/// The count is the whole point of the parameter. Waiting for the *presence* of
/// text that is already on screen returns instantly and asserts nothing, which
/// is how a test for the up arrow passes without an up arrow ever working.
fn wait_for_times(held: &Arc<Mutex<Vec<u8>>>, needle: &str, times: usize) -> String {
    let started = Instant::now();
    loop {
        let so_far = String::from_utf8_lossy(&held.lock().unwrap()).into_owned();
        if so_far.matches(needle).count() >= times {
            return so_far;
        }
        assert!(
            started.elapsed() < PATIENCE,
            "waited for {times} × {needle:?} and it never came. what did arrive:\n{}",
            so_far.replace('\x1b', "<ESC>")
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_prompt_edits_a_line_and_hands_the_terminal_back() {
    let (master, slave) = pty();
    let before = Discipline::of(slave.as_raw_fd());
    assert!(
        before.canonical && before.echoing && before.signalling,
        "a fresh pty should be an ordinary terminal, and this one is {before:?}"
    );

    let mut child = Command::new(TESSARIDB)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave))
        .spawn()
        .expect("the binary this crate builds");

    let master = File::from(master);
    let mut keyboard = master.try_clone().unwrap();
    let screen = gather(master);

    // The greeting is how we know the child has reached the prompt; typing
    // before it has is a race that would fail here once a week.
    wait_for_times(&screen, "tessaridb>", 1);

    // 1. An ordinary statement. `ECHO` is off, so every character that appears
    //    was drawn by the editor rather than by the terminal.
    keyboard.write_all(b"DEFINE NAMESPACE prod;\r").unwrap();
    wait_for_times(&screen, "DEFINE NAMESPACE prod;", 1);
    wait_for_times(&screen, "ok", 1);

    // 2. The up arrow recalls it. Recall is told apart from *running twice* by
    //    throwing the recalled line away with Ctrl-C instead of pressing Return:
    //    the text has to be on screen a second time without having run again.
    keyboard.write_all(b"\x1b[A").unwrap();
    wait_for_times(&screen, "DEFINE NAMESPACE prod;", 2);

    // 3. Ctrl-C throws it away and the session stays. A process that died here
    //    would fail the next step rather than this one, so both are asserted.
    keyboard.write_all(b"\x03").unwrap();
    wait_for_times(&screen, "^C", 1);

    // 4. The cursor moves, and typing happens where it is. `abcd` with two steps
    //    left and `XY` typed is `abXYcd` — and is `abcdXY` if the arrows were
    //    swallowed, which is the failure this spells out rather than checks for
    //    the absence of.
    keyboard.write_all(b"abcd").unwrap();
    wait_for_times(&screen, "abcd", 1);
    keyboard.write_all(b"\x1b[D\x1b[D").unwrap();
    keyboard.write_all(b"XY").unwrap();
    wait_for_times(&screen, "abXYcd", 1);
    keyboard.write_all(b"\x03").unwrap();

    // 5. And it leaves when asked.
    keyboard.write_all(b".exit\r").unwrap();
    let status = child.wait().expect("the child to be waitable");
    assert!(status.success(), "the prompt exited as {status}");

    // 6. The obligation. Read from the terminal itself, after the program that
    //    changed it is gone.
    let after = Discipline::of(keyboard.as_raw_fd());
    assert_eq!(
        after, before,
        "the terminal was left changed. a shell started after this program would \
         have no echo and no line editing, and nothing would say why"
    );
}
