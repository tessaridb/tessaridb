//! Putting this terminal into raw mode, and getting it back out.
//!
//! # Why there is `unsafe` here
//!
//! For `shutdown.rs`'s reason in a different key: `std` has no terminal API at
//! all, so the choice was between a handful of audited calls into `libc` — which
//! this crate already depends on — and a crate whose entire purpose is to make
//! the same calls. The same answer as last time: the dependency is a larger
//! surface and is not more correct.
//!
//! # The obligation raw mode creates
//!
//! A terminal left in raw mode is **broken for the shell that comes after** —
//! no echo, no line discipline, `reset` typed blind. So the settings are saved
//! in a static rather than in the editor's own value, and every way out of the
//! process restores them:
//!
//!   1. an ordinary return, through [`Restored`]'s `Drop`;
//!   2. a panic, through a hook installed at the same moment — necessary
//!      because this workspace builds release with `panic = "abort"`, so `Drop`
//!      does **not** run on the way down;
//!   3. `Ctrl-C`, which never becomes a signal at all: `ISIG` is cleared, so the
//!      key arrives as a byte the editor handles and no handler is involved.
//!
//! What is left uncovered is a `SIGTERM` from outside while a person is at a
//! prompt, which kills the process with the terminal still raw. It is stated
//! here rather than papered over: covering it means a handler, and a handler in
//! the interactive path would be a second mechanism for stopping next to the one
//! `shutdown.rs` installs for the serving path.
//!
//! `tcsetattr` is on POSIX's async-signal-safe list, so [`restore`] would be
//! legal from a handler if that trade is ever taken.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

/// What the terminal was before this process touched it.
///
/// A static because the panic hook has to reach it, and a hook outlives any
/// value the editor could hold.
struct Saved {
    /// The settings, meaningful only while `held` is true.
    settings: UnsafeCell<MaybeUninit<libc::termios>>,
    /// Whether `settings` holds something worth putting back.
    held: AtomicBool,
}

// SAFETY: `settings` is written exactly once, by `enter`, before `held` is set
// with `Release` ordering, and is read only after `held` reads true with
// `Acquire` ordering. No other code takes a reference to the cell, and this
// crate never runs two editors at once — `Restored` is constructed only by
// `attach`, which refuses while `held` is already true.
unsafe impl Sync for Saved {}

static SAVED: Saved = Saved {
    settings: UnsafeCell::new(MaybeUninit::uninit()),
    held: AtomicBool::new(false),
};

/// Installs the panic hook once, however many editors are attached.
static HOOK: Once = Once::new();

/// Proof the terminal is in raw mode, and the thing that ends it.
///
/// Deliberately carries no data: what it owns is a *state of the terminal*, not
/// a value, and the state lives in a static because the panic path needs it too.
pub struct Restored;

impl Drop for Restored {
    fn drop(&mut self) {
        restore();
    }
}

/// Put this terminal in raw mode, or answer `None` if it will not go.
///
/// `None` covers every reason honestly — not a terminal, a terminal that
/// refuses the settings, or an editor already attached — because each has the
/// same answer for the caller: read lines the ordinary way instead.
pub fn enter() -> Option<Restored> {
    if SAVED.held.load(Ordering::Acquire) {
        return None;
    }
    let mut current = MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `tcgetattr` fills the struct it is handed for a descriptor this
    // process holds, and reports failure rather than filling it partially.
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, current.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: `tcgetattr` returned success, so the value is initialised.
    let original = unsafe { current.assume_init() };

    let mut raw = original;
    // Character at a time, nothing echoed: the editor draws every character
    // that appears, which is the whole point — it cannot show a cursor in the
    // middle of a line the terminal is also drawing the end of.
    //
    // `ISIG` off is what turns `Ctrl-C` into a byte instead of a signal. That is
    // not a convenience: it is what keeps a keystroke from killing the process
    // with the terminal in this state.
    //
    // `IXON` off frees `Ctrl-S`/`Ctrl-Q`; `ICRNL` off is what lets Return be
    // told from `Ctrl-J`; `IEXTEN` off frees `Ctrl-V`.
    raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
    raw.c_iflag &= !(libc::IXON | libc::ICRNL);
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;

    // SAFETY: the settings were read from this same descriptor a moment ago and
    // differ from it only in the flags cleared above.
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
        return None;
    }

    // SAFETY: nothing else holds a reference to the cell, and `held` is false,
    // so no reader is looking at it. The store below publishes the write.
    unsafe { (*SAVED.settings.get()).write(original) };
    SAVED.held.store(true, Ordering::Release);

    arm();
    Some(Restored)
}

/// Put the terminal back the way it was found, if it was changed.
///
/// Idempotent, because it is reached from two places that do not know about
/// each other: a `Drop` on the ordinary path and a panic hook on the other. The
/// swap is what makes the second call a no-op rather than a second `tcsetattr`.
pub fn restore() {
    if !SAVED.held.swap(false, Ordering::AcqRel) {
        return;
    }
    // SAFETY: `held` read true, which only `enter` sets and only after writing
    // the value.
    let original = unsafe { (*SAVED.settings.get()).assume_init() };
    // SAFETY: the settings came from this descriptor and are being put back on
    // it unchanged.
    // The result is read by nobody on purpose: a terminal that refuses to be put
    // back leaves nothing to report it with, and every caller of this is already
    // on its way out of the process.
    let _ = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
}

/// Arrange for a panic to restore the terminal before it takes the process down.
///
/// The previous hook is kept and called, so this adds to whatever reporting is
/// already installed rather than replacing it.
fn arm() {
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panicked| {
            restore();
            previous(panicked);
        }));
    });
}

/// How many columns the terminal has, or 80 where it will not say.
///
/// 80 rather than a guess at something larger: the editor scrolls a long line
/// sideways to fit, and being wrong towards *narrow* scrolls a line that would
/// have fitted, while being wrong towards wide draws past the edge and leaves
/// the cursor somewhere the redraw cannot find.
pub fn columns() -> usize {
    let mut size = MaybeUninit::<libc::winsize>::uninit();
    // SAFETY: `TIOCGWINSZ` fills a `winsize` for a descriptor this process
    // holds, and reports failure rather than filling it partially.
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) } != 0 {
        return 80;
    }
    // SAFETY: the call returned success, so the value is initialised.
    let size = unsafe { size.assume_init() };
    match size.ws_col {
        0 => 80,
        columns => usize::from(columns),
    }
}
