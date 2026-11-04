//! Entering and leaving the terminal, however the interface ends.
//!
//! Restoring is idempotent and callable from a panic hook, so every exit path
//! shares one routine. The interface owns the terminal while it runs, so stderr
//! is parked on `/dev/null`: a stray library diagnostic must not scribble over
//! the screen. It is given back before the Setup wizard runs and on every exit.

use std::io::{self, BufWriter, Stdout, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

pub type Screen = Terminal<CrosstermBackend<BufWriter<Stdout>>>;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static SAVED_STDERR: AtomicI32 = AtomicI32::new(-1);
static SIGNAL: AtomicI32 = AtomicI32::new(0);

/// How often the watchdog looks for a signal the main loop has not acted on.
const WATCH: Duration = Duration::from_millis(200);
/// Far longer than one trip around a healthy main loop.
const GRACE: Duration = Duration::from_millis(500);

extern "C" fn record_signal(signal: libc::c_int) {
    SIGNAL.store(signal, Ordering::Relaxed);
}

/// Turns terminating signals into a flag the main loop reads, so the screen can
/// be restored before the process ends.
pub fn install_signal_handlers() {
    for signal in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        let handler = record_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SAFETY: the handler only stores to an atomic, which is signal-safe.
        unsafe {
            libc::signal(signal, handler);
        }
    }
    std::thread::spawn(watchdog);
}

/// Ends the process when the main loop cannot. Once the terminal closes, the
/// crossterm reader spins forever: `read` fails with EIO, which it neither
/// returns nor breaks on, so the loop never sees the SIGHUP and a closed window
/// would leave a process burning a core. Only while this process owns the
/// terminal, because a signal during the Setup wizard is the main loop's to weigh
/// once the wizard returns.
fn watchdog() {
    let unhandled = || pending_signal().filter(|_| ACTIVE.load(Ordering::SeqCst));
    loop {
        std::thread::sleep(WATCH);
        if unhandled().is_some() {
            std::thread::sleep(GRACE);
            if let Some(signal) = unhandled() {
                leave();
                std::process::exit(128 + signal);
            }
        }
    }
}

/// The terminating signal received, if any.
pub fn pending_signal() -> Option<i32> {
    match SIGNAL.load(Ordering::Relaxed) {
        0 => None,
        signal => Some(signal),
    }
}

/// Forgets a Ctrl-C that the Setup wizard's own terminal delivered to this
/// process group as well: it was meant for the wizard.
pub fn forget_interrupt() {
    let _ = SIGNAL.compare_exchange(libc::SIGINT, 0, Ordering::Relaxed, Ordering::Relaxed);
}

/// Puts the terminal back and restores the original panic behaviour's output.
pub fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave();
        original(info);
        // A panic on a worker thread would otherwise leave a live interface
        // with a dead worker behind a restored terminal.
        std::process::exit(101);
    }));
}

fn silence_stderr() {
    // SAFETY: plain descriptor calls. A failure leaves stderr as it was, which
    // risks only a scribble on the screen.
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if null < 0 {
            return;
        }
        let saved = libc::fcntl(io::stderr().as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3);
        if saved >= 0 {
            SAVED_STDERR.store(saved, Ordering::SeqCst);
            libc::dup2(null, io::stderr().as_raw_fd());
        }
        libc::close(null);
    }
}

fn restore_stderr() {
    let saved = SAVED_STDERR.swap(-1, Ordering::SeqCst);
    if saved >= 0 {
        // SAFETY: `saved` is a descriptor this module duplicated and owns.
        unsafe {
            libc::dup2(saved, io::stderr().as_raw_fd());
            libc::close(saved);
        }
    }
}

fn arm() -> io::Result<()> {
    ACTIVE.store(true, Ordering::SeqCst);
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    silence_stderr();
    Ok(())
}

/// Takes over the terminal.
pub fn enter() -> io::Result<Screen> {
    arm()?;
    Terminal::new(CrosstermBackend::new(BufWriter::new(io::stdout())))
}

/// Gives the terminal back. Safe to call any number of times, from anywhere.
pub fn leave() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    restore_stderr();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen);
    let _ = execute!(stdout, ratatui::crossterm::cursor::Show);
    let _ = stdout.flush();
    let _ = disable_raw_mode();
}

/// Takes the terminal back after the Setup wizard and repaints from scratch.
pub fn reenter(screen: &mut Screen) -> io::Result<()> {
    arm()?;
    screen.clear()
}
