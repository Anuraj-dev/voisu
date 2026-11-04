//! The compact interface bare `voisu` opens on a real terminal.
//!
//! Four flat tabs, one key table, and a pure model. See [`model::update`] for
//! the behaviour; this file only wires terminal events to it.

pub mod keys;
pub mod look;
pub mod model;
pub mod render;
pub mod setup;
mod setup_bridge;
pub mod terminal;
pub mod text;
pub mod worker;

use std::io::{self, IsTerminal};
use std::process::ExitCode;
use std::sync::mpsc::{Sender, channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use look::Look;
use model::{App, Effect, Msg, Now, Reply, update};
use setup::SetupAnswer;
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

/// `voisu setup` on two real terminals opens the setup screens. Anything else
/// keeps the text wizard, so scripts and pipes never see a change. Tests that
/// isolate the wizard ask for it by name.
pub fn should_open_setup() -> bool {
    std::env::var_os("VOISU_TEST_SETUP_WIZARD_ONLY").is_none()
        && opens_interface(
            true,
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
    run_from(false)
}

/// Runs the interface on the setup screens, and exits with how setup went.
pub fn run_setup() -> ExitCode {
    run_from(true)
}

fn run_from(setup: bool) -> ExitCode {
    terminal::install_panic_hook();
    terminal::install_signal_handlers();
    let code = match terminal::enter() {
        Ok(mut screen) => {
            let result = event_loop(&mut screen, setup);
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

fn event_loop(screen: &mut Screen, setup: bool) -> io::Result<u8> {
    let look = Look::from_env();
    let clock = Clock(Instant::now());
    let size = screen.size()?;
    let mut app = App::new((size.width, size.height), clock.now());
    let (reply_tx, replies) = channel();
    let workers = Workers::start(reply_tx.clone());
    let mut flow = Flow {
        replies: reply_tx,
        answers: None,
    };

    let mut effects = app.start();
    if setup {
        effects.extend(app.start_setup_from_cli());
    }
    perform(effects, &workers, &mut flow);

    loop {
        if let Some(signal) = terminal::pending_signal() {
            return Ok(128 + signal as u8);
        }
        if app.take_dirty() {
            screen.draw(|frame| render::draw(frame, &app, &look))?;
        }

        // Input first, so typing feels immediate.
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
                    perform(effects, &workers, &mut flow);
                }
                if app.should_quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        while let Ok(reply) = replies.try_recv() {
            let effects = update(&mut app, Msg::Reply(reply), clock.now());
            perform(effects, &workers, &mut flow);
        }
        let effects = update(&mut app, Msg::Tick, clock.now());
        perform(effects, &workers, &mut flow);

        if app.should_quit {
            return Ok(app.setup_exit.unwrap_or(0));
        }
    }
}

/// The setup flow's thread, once it runs.
struct Flow {
    replies: Sender<Reply>,
    answers: Option<Sender<SetupAnswer>>,
}

fn perform(effects: Vec<Effect>, workers: &Workers, flow: &mut Flow) {
    for effect in effects {
        match effect {
            Effect::Job(job) => workers.submit(job),
            Effect::StartSetup => {
                flow.answers = Some(setup_bridge::spawn(flow.replies.clone()));
            }
            Effect::Setup(answer) => {
                if let Some(answers) = &flow.answers {
                    let _ = answers.send(answer);
                }
            }
        }
    }
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
