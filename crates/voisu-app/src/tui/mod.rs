//! The compact interface bare `voisu` opens on a real terminal.
//!
//! Four flat tabs, one key table, and a pure model. See [`model::update`] for
//! the behaviour; this file only wires terminal events to it.

pub mod keys;
pub mod look;
pub mod model;
pub mod render;
pub mod terminal;
pub mod text;
pub mod worker;

use std::io::{self, IsTerminal};
use std::process::ExitCode;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use look::Look;
use model::{App, Effect, Msg, Now, update};
use terminal::Screen;
use worker::Workers;

/// How long the loop may sleep waiting for input. Worker replies are picked up
/// at this rate, which is fast to a person and costs next to nothing.
const TICK: Duration = Duration::from_millis(100);

/// Bare `voisu` on two real terminals opens the interface. Anything else keeps
/// today's behaviour, so scripts and pipes never see a change.
pub fn should_open() -> bool {
    opens_interface(
        std::env::args_os().len() <= 1,
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        std::env::var_os("TERM")
            .as_deref()
            .and_then(|term| term.to_str()),
    )
}

fn opens_interface(no_args: bool, stdin_tty: bool, stdout_tty: bool, term: Option<&str>) -> bool {
    no_args && stdin_tty && stdout_tty && term != Some("dumb")
}

struct Clock(Instant);

impl Clock {
    fn now(&self) -> Now {
        Now {
            mono_ms: self.0.elapsed().as_millis() as u64,
            wall_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis() as u64),
        }
    }
}

/// Runs the interface until the user quits or a signal ends it.
pub fn run() -> ExitCode {
    terminal::install_panic_hook();
    terminal::install_signal_handlers();
    let code = match terminal::enter() {
        Ok(mut screen) => {
            let result = event_loop(&mut screen);
            terminal::leave();
            match result {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("the interface stopped: {error}");
                    1
                }
            }
        }
        Err(error) => {
            terminal::leave();
            eprintln!("cannot open the interface: {error}");
            1
        }
    };
    ExitCode::from(code)
}

fn event_loop(screen: &mut Screen) -> io::Result<u8> {
    let look = Look::from_env();
    let clock = Clock(Instant::now());
    let size = screen.size()?;
    let mut app = App::new((size.width, size.height), clock.now());
    let (reply_tx, replies) = channel();
    let workers = Workers::start(reply_tx);

    let effects = app.start();
    perform(effects, &mut app, screen, &workers, &clock)?;

    loop {
        if let Some(signal) = terminal::pending_signal() {
            return Ok(128 + signal as u8);
        }
        if app.take_dirty() {
            screen.draw(|frame| render::draw(frame, &app, &look))?;
        }

        // Input first, so typing feels immediate. Polling with a timeout keeps
        // stdin free for the Setup wizard, which an input thread would fight.
        if event::poll(TICK)? {
            loop {
                let msg = match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => Some(Msg::Key(key)),
                    Event::Paste(text) => Some(Msg::Paste(text)),
                    Event::Resize(width, height) => Some(Msg::Resize(width, height)),
                    _ => None,
                };
                if let Some(msg) = msg {
                    let effects = update(&mut app, msg, clock.now());
                    perform(effects, &mut app, screen, &workers, &clock)?;
                }
                if app.should_quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        while let Ok(reply) = replies.try_recv() {
            let effects = update(&mut app, Msg::Reply(reply), clock.now());
            perform(effects, &mut app, screen, &workers, &clock)?;
        }
        let effects = update(&mut app, Msg::Tick, clock.now());
        perform(effects, &mut app, screen, &workers, &clock)?;

        if app.should_quit {
            return Ok(0);
        }
    }
}

fn perform(
    effects: Vec<Effect>,
    app: &mut App,
    screen: &mut Screen,
    workers: &Workers,
    clock: &Clock,
) -> io::Result<()> {
    for effect in effects {
        match effect {
            Effect::Job(job) => workers.submit(job),
            Effect::RunSetup => {
                let completed = hand_off_to_setup(screen)?;
                let effects = update(app, Msg::SetupReturned { completed }, clock.now());
                for follow_up in effects {
                    if let Effect::Job(job) = follow_up {
                        workers.submit(job);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Leaves the screen, runs the wizard on this terminal with inherited stdio, and
/// comes back. Setup stays the one place that performs a Consented Install.
fn hand_off_to_setup(screen: &mut Screen) -> io::Result<bool> {
    terminal::leave();
    let status = std::env::current_exe()
        .and_then(|exe| std::process::Command::new(exe).arg("setup").status());
    terminal::forget_interrupt();
    terminal::reenter(screen)?;
    Ok(status.is_ok_and(|status| status.success()))
}

#[cfg(test)]
mod tests {
    use super::opens_interface;

    #[test]
    fn only_bare_voisu_on_two_real_terminals_opens_the_interface() {
        assert!(opens_interface(true, true, true, Some("xterm-256color")));
        assert!(opens_interface(true, true, true, None));
        assert!(!opens_interface(false, true, true, Some("xterm")));
        assert!(!opens_interface(true, false, true, Some("xterm")));
        assert!(!opens_interface(true, true, false, Some("xterm")));
        assert!(!opens_interface(true, true, true, Some("dumb")));
    }
}
