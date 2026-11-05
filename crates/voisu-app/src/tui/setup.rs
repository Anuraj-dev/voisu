//! The setup screens' state: what is on screen and what each key does there.
//! Pure, like the rest of the model. The questions come from
//! `setup_bridge`, which runs the one setup flow on its own thread.
//!
//! No secret ever enters a [`SetupEvent`], which is copied around freely: a key
//! the person typed leaves here once, inside a [`SetupAnswer`], and only its
//! length comes back.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use voisu_core::Provider;

use super::keys::{Action, Context};
use super::model::PromptText;

pub const STEPS: u8 = 4;
/// A moment after the last answer, until the flow asks something else.
pub const ONE_MOMENT: &str = "One moment…";
/// A time left means nothing until the download has run a little.
const ESTIMATE_AFTER_MS: u64 = 5_000;
const MS_PER_MINUTE: u64 = 60_000;

/// One row of a question.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChoiceRow {
    pub label: String,
    pub line: String,
}

impl ChoiceRow {
    pub fn new(label: &str, line: &str) -> Self {
        Self {
            label: label.to_owned(),
            line: line.to_owned(),
        }
    }
}

/// A question with a few answers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Choose {
    pub step: u8,
    pub title: String,
    pub body: Vec<String>,
    pub rows: Vec<ChoiceRow>,
    pub picked: usize,
    pub note: Option<String>,
}

/// Where a key stands. Holds words for the screen, never the key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyState {
    /// Nothing entered, or typing one in.
    Blank,
    /// A key is already stored.
    Saved,
    /// Typing a new key over a stored one, which stays until the new one works.
    Replacing,
    /// Set by an environment variable, which wins whatever is stored.
    Outside,
    Checking,
    /// Saving a key the check couldn't answer for. A slow keyring takes seconds.
    Saving,
    Works,
    Rejected,
    /// The check could not tell, in the Settings tab's words. `↵` saves anyway.
    Transient(String),
    /// Saved anyway, without an answer from the service.
    Unverified,
    /// The check passed but storing the key failed.
    Failed(String),
}

/// One key as the flow first reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyStart {
    pub provider: Provider,
    pub state: KeyState,
    pub note: Option<String>,
}

/// One stage of a Local install.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd)]
pub enum Stage {
    Downloading,
    GettingReady,
    Checking,
}

/// What setup says when it is done.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SummaryRow {
    pub ok: bool,
    pub label: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Summary {
    pub rows: Vec<SummaryRow>,
    pub try_it: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Finish {
    Ready(Summary),
    Failed {
        message: String,
        /// The flow's own words, for whoever has to fix it.
        detail: Option<String>,
    },
    /// The person left; there is nothing to say.
    Left,
}

/// What the flow tells the screen. Goes through [`super::model::Reply`], so it
/// never carries a secret.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupEvent {
    Ask(Choose),
    Keys {
        step: u8,
        fields: Vec<KeyStart>,
        /// Cloud cannot work without one key; after Local, none is fine.
        required: bool,
    },
    KeyUpdate {
        provider: Provider,
        state: KeyState,
        /// The entered key's length, when it changed.
        dots: Option<usize>,
        note: Option<String>,
    },
    /// The check of an already stored key came back.
    SavedWord {
        provider: Provider,
        word: String,
    },
    KeyNote {
        provider: Provider,
        note: String,
    },
    InstallStarted {
        step: u8,
        total: u64,
    },
    Downloaded {
        so_far: u64,
    },
    Stage(Stage),
    InstallFailed {
        message: String,
        esc: &'static str,
    },
    Busy {
        step: u8,
        title: String,
        line: String,
    },
    /// The desktop's own window is open, or about to be.
    Waiting {
        step: u8,
        title: String,
    },
    Finished {
        code: u8,
        finish: Finish,
    },
}

/// What the screen tells the flow. Debug is safe: [`PromptText`] redacts itself.
#[derive(Debug)]
pub enum SetupAnswer {
    Picked(usize),
    Left,
    CheckKey {
        provider: Provider,
        text: PromptText,
    },
    /// Read the key from the clipboard and check it.
    PasteClipboard {
        provider: Provider,
    },
    SaveAnyway {
        provider: Provider,
    },
    KeysDone,
    Retry,
    /// Esc after a failed install.
    GiveUpInstall,
    /// Esc while waiting for the desktop's window.
    Later,
}

/// One key on the keys screen.
#[derive(Debug)]
pub struct KeyField {
    pub provider: Provider,
    pub state: KeyState,
    /// What is being typed. Goes nowhere until `↵`.
    pub text: PromptText,
    /// The length of the key already sent to be checked.
    pub dots: usize,
    pub note: Option<String>,
    /// A key is stored, whatever happens to the one being typed.
    pub saved: bool,
    /// The stored key's check result.
    pub word: Option<String>,
}

impl KeyField {
    fn new(start: KeyStart) -> Self {
        let saved = matches!(start.state, KeyState::Saved | KeyState::Outside);
        Self {
            provider: start.provider,
            state: start.state,
            text: PromptText::default(),
            dots: 0,
            note: start.note,
            saved,
            word: None,
        }
    }

    /// How many dots to show: only ever the count.
    pub fn shown_dots(&self) -> usize {
        match self.text.chars() {
            0 => self.dots,
            typed => typed,
        }
    }

    /// Whether the terminal cursor belongs on this field.
    pub fn takes_typing(&self) -> bool {
        !self.busy() && self.state != KeyState::Outside
    }

    /// Whether Voisu is still working on this key.
    fn busy(&self) -> bool {
        matches!(self.state, KeyState::Checking | KeyState::Saving)
    }

    fn usable(&self) -> bool {
        self.saved || matches!(self.state, KeyState::Works | KeyState::Unverified)
    }

    /// Back to an empty field, dropping whatever was typed or checked.
    fn begin_typing(&mut self) {
        self.state = if self.saved {
            KeyState::Replacing
        } else {
            KeyState::Blank
        };
        self.text = PromptText::default();
        self.dots = 0;
        self.note = None;
    }

    fn enter(&self) -> Enter {
        match &self.state {
            KeyState::Checking | KeyState::Saving => Enter::Nothing,
            KeyState::Saved => Enter::Replace,
            KeyState::Outside | KeyState::Works | KeyState::Unverified => Enter::Continue,
            _ if self.text.chars() > 0 => Enter::Check,
            KeyState::Transient(_) => Enter::SaveAnyway,
            _ if self.saved => Enter::Keep,
            _ => Enter::Skip,
        }
    }

    /// Sends what was typed to be checked.
    fn start_check(&mut self) -> SetupAnswer {
        self.dots = self.text.chars();
        self.state = KeyState::Checking;
        self.note = None;
        SetupAnswer::CheckKey {
            provider: self.provider,
            text: std::mem::take(&mut self.text),
        }
    }

    fn keep(&mut self) {
        self.state = KeyState::Saved;
        self.text = PromptText::default();
        self.dots = 0;
        self.note = None;
    }
}

/// What `↵` does on a key field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Enter {
    Nothing,
    Replace,
    Continue,
    Check,
    SaveAnyway,
    Keep,
    Skip,
}

impl Enter {
    fn label(self) -> Option<&'static str> {
        match self {
            Enter::Nothing => None,
            Enter::Replace => Some("replace"),
            Enter::Continue => Some("continue"),
            Enter::Check => Some("check"),
            Enter::SaveAnyway => Some("save it"),
            Enter::Keep => Some("keep"),
            Enter::Skip => Some("skip"),
        }
    }
}

#[derive(Debug)]
pub struct KeysScreen {
    pub fields: Vec<KeyField>,
    pub selected: usize,
    pub required: bool,
    /// Why `↵` did not move on.
    pub notice: Option<&'static str>,
}

impl KeysScreen {
    pub fn field(&self) -> &KeyField {
        &self.fields[self.selected]
    }

    fn field_mut(&mut self) -> &mut KeyField {
        &mut self.fields[self.selected]
    }

    fn by_provider(&mut self, provider: Provider) -> Option<(usize, &mut KeyField)> {
        self.fields
            .iter_mut()
            .enumerate()
            .find(|(_, field)| field.provider == provider)
    }

    /// Moves to the next key, or finishes after the last one.
    fn advance(&mut self) -> Result<Outcome, ()> {
        if self.selected + 1 < self.fields.len() {
            self.selected += 1;
            return Ok(Outcome::Stay);
        }
        if self.fields.iter().any(KeyField::busy) {
            self.notice = Some("Still checking a key…");
            return Ok(Outcome::Stay);
        }
        if self.required && !self.fields.iter().any(KeyField::usable) {
            self.notice = Some("Cloud needs at least one key.");
            return Ok(Outcome::Stay);
        }
        Err(())
    }

    /// Moves between the keys. A key typed but not checked is checked on the way,
    /// so nothing typed is ever silently dropped.
    fn step(&mut self, by: isize) -> Outcome {
        let next = self
            .selected
            .saturating_add_signed(by)
            .min(self.fields.len() - 1);
        if next == self.selected {
            return Outcome::Stay;
        }
        let answer = (self.field().text.chars() > 0).then(|| self.field_mut().start_check());
        self.selected = next;
        answer.map_or(Outcome::Stay, Outcome::Answer)
    }
}

/// A Local install on screen.
#[derive(Debug)]
pub struct Installing {
    pub total: u64,
    pub so_far: u64,
    pub stage: Stage,
    pub started_ms: u64,
    /// Why it stopped, and what Esc does now.
    pub failed: Option<(String, &'static str)>,
}

impl Installing {
    /// "about 4 min left", once there is enough to estimate from.
    pub fn time_left(&self, now_ms: u64) -> Option<String> {
        let elapsed = now_ms.saturating_sub(self.started_ms);
        if elapsed < ESTIMATE_AFTER_MS || self.so_far == 0 || self.so_far >= self.total {
            return None;
        }
        let remaining =
            u128::from(elapsed) * u128::from(self.total - self.so_far) / u128::from(self.so_far);
        let minutes = (remaining as u64).div_ceil(MS_PER_MINUTE);
        Some(if minutes <= 1 {
            "under a minute left".to_owned()
        } else {
            format!("about {minutes} min left")
        })
    }
}

#[derive(Debug)]
pub enum Screen {
    Busy {
        title: String,
        line: String,
    },
    Choose(Choose),
    Keys(KeysScreen),
    Installing(Installing),
    Waiting {
        title: String,
    },
    Leaving,
    Done(Summary),
    Failed {
        message: String,
        detail: Option<String>,
    },
}

/// What a key press leaves for the model to do.
#[derive(Debug)]
pub enum Outcome {
    Stay,
    Answer(SetupAnswer),
    /// Setup is over: on to the tabs.
    Close,
}

/// What an event from the flow leaves for the model to do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Landed {
    Stay,
    /// The flow ended and its result is on screen.
    Finished {
        code: u8,
    },
    /// The flow ended because the person left: on to the tabs.
    Gone {
        code: u8,
    },
}

#[derive(Debug)]
pub struct SetupState {
    pub step: u8,
    pub screen: Screen,
    /// The flow's exit code, once it ended.
    pub code: Option<u8>,
}

fn one_moment() -> Screen {
    Screen::Busy {
        title: String::new(),
        line: ONE_MOMENT.to_owned(),
    }
}

impl SetupState {
    pub fn starting() -> Self {
        Self {
            step: 1,
            screen: one_moment(),
            code: None,
        }
    }

    /// Which keys mean what on this screen.
    pub fn context(&self) -> Context {
        match &self.screen {
            Screen::Choose(_) => Context::SetupChoice,
            Screen::Keys(keys) => {
                let field = keys.field();
                Context::SetupKey {
                    enter: field.enter().label(),
                    // Just saved, `↵ continue` already keeps it.
                    keep: field.saved
                        && !field.busy()
                        && !matches!(
                            field.state,
                            KeyState::Outside | KeyState::Works | KeyState::Unverified
                        ),
                    editable: field.takes_typing(),
                }
            }
            Screen::Installing(Installing {
                failed: Some((_, esc)),
                ..
            }) => Context::SetupInstallFailed { esc },
            Screen::Busy { .. } | Screen::Installing(_) | Screen::Leaving => Context::SetupBusy,
            Screen::Waiting { .. } => Context::SetupWaiting,
            Screen::Done(_) | Screen::Failed { .. } => Context::SetupDone,
        }
    }

    /// A bound key, on the screen it was bound for.
    pub fn act(&mut self, action: Action) -> Outcome {
        match (&mut self.screen, action) {
            (Screen::Choose(choose), Action::Up) => {
                choose.picked = choose.picked.saturating_sub(1);
                Outcome::Stay
            }
            (Screen::Choose(choose), Action::Down) => {
                choose.picked = (choose.picked + 1).min(choose.rows.len().saturating_sub(1));
                Outcome::Stay
            }
            (Screen::Choose(choose), Action::Submit) => {
                let picked = choose.picked;
                self.screen = one_moment();
                Outcome::Answer(SetupAnswer::Picked(picked))
            }
            (Screen::Keys(keys), Action::Up) => {
                keys.notice = None;
                keys.step(-1)
            }
            (Screen::Keys(keys), Action::Down) => {
                keys.notice = None;
                keys.step(1)
            }
            (Screen::Keys(_), Action::Submit) => self.submit_key(),
            (Screen::Keys(keys), Action::Keep) => {
                keys.notice = None;
                keys.field_mut().keep();
                self.leave_field()
            }
            (Screen::Keys(keys), Action::PasteClipboard) => {
                keys.notice = None;
                let provider = keys.field().provider;
                Outcome::Answer(SetupAnswer::PasteClipboard { provider })
            }
            (Screen::Installing(Installing { failed, .. }), Action::Submit) if failed.is_some() => {
                Outcome::Answer(SetupAnswer::Retry)
            }
            (Screen::Installing(Installing { failed, .. }), Action::Close) if failed.is_some() => {
                self.screen = one_moment();
                Outcome::Answer(SetupAnswer::GiveUpInstall)
            }
            (Screen::Waiting { .. }, Action::Close) => {
                self.screen = one_moment();
                Outcome::Answer(SetupAnswer::Later)
            }
            (Screen::Done(_) | Screen::Failed { .. }, Action::Submit) => Outcome::Close,
            (Screen::Choose(_) | Screen::Keys(_), Action::Close) => {
                self.screen = Screen::Leaving;
                Outcome::Answer(SetupAnswer::Left)
            }
            _ => Outcome::Stay,
        }
    }

    fn submit_key(&mut self) -> Outcome {
        let Screen::Keys(keys) = &mut self.screen else {
            return Outcome::Stay;
        };
        keys.notice = None;
        let field = keys.field_mut();
        match field.enter() {
            Enter::Nothing => Outcome::Stay,
            Enter::Replace => {
                field.begin_typing();
                Outcome::Stay
            }
            Enter::Check => Outcome::Answer(field.start_check()),
            Enter::SaveAnyway => {
                field.state = KeyState::Saving;
                field.note = None;
                Outcome::Answer(SetupAnswer::SaveAnyway {
                    provider: field.provider,
                })
            }
            Enter::Keep => {
                field.keep();
                self.leave_field()
            }
            Enter::Continue | Enter::Skip => self.leave_field(),
        }
    }

    /// On to the next key, or out of the keys screen after the last.
    fn leave_field(&mut self) -> Outcome {
        let Screen::Keys(keys) = &mut self.screen else {
            return Outcome::Stay;
        };
        match keys.advance() {
            Ok(outcome) => outcome,
            Err(()) => {
                self.screen = one_moment();
                Outcome::Answer(SetupAnswer::KeysDone)
            }
        }
    }

    /// A key no binding took, for typing into a key field.
    pub fn type_key(&mut self, key: KeyEvent) {
        let Screen::Keys(keys) = &mut self.screen else {
            return;
        };
        let field = keys.field_mut();
        if !field.takes_typing() || !matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace) {
            return;
        }
        // Typing over a result starts a new key.
        if !matches!(field.state, KeyState::Blank | KeyState::Replacing) {
            field.begin_typing();
        }
        field.note = None;
        field.text.edit(key);
        keys.notice = None;
    }

    /// A paste into a key field is a whole key: it is checked at once.
    pub fn paste(&mut self, pasted: &str) -> Outcome {
        let Screen::Keys(keys) = &mut self.screen else {
            return Outcome::Stay;
        };
        keys.notice = None;
        let field = keys.field_mut();
        if !field.takes_typing() {
            return Outcome::Stay;
        }
        field.begin_typing();
        field.text.paste(pasted);
        if field.text.chars() == 0 {
            return Outcome::Stay;
        }
        Outcome::Answer(field.start_check())
    }

    pub fn on_event(&mut self, event: SetupEvent, now_ms: u64) -> Landed {
        match event {
            SetupEvent::Ask(choose) => {
                self.step = choose.step;
                self.screen = Screen::Choose(choose);
            }
            SetupEvent::Keys {
                step,
                fields,
                required,
            } => {
                self.step = step;
                self.screen = Screen::Keys(KeysScreen {
                    fields: fields.into_iter().map(KeyField::new).collect(),
                    selected: 0,
                    required,
                    notice: None,
                });
            }
            SetupEvent::KeyUpdate {
                provider,
                state,
                dots,
                note,
            } => self.on_key_update(provider, state, dots, note),
            SetupEvent::SavedWord { provider, word } => {
                if let Screen::Keys(keys) = &mut self.screen
                    && let Some((_, field)) = keys.by_provider(provider)
                {
                    field.word = Some(word);
                }
            }
            SetupEvent::KeyNote { provider, note } => {
                if let Screen::Keys(keys) = &mut self.screen
                    && let Some((_, field)) = keys.by_provider(provider)
                {
                    field.note = Some(note);
                }
            }
            SetupEvent::InstallStarted { step, total } => {
                self.step = step;
                self.screen = Screen::Installing(Installing {
                    total,
                    so_far: 0,
                    stage: Stage::Downloading,
                    started_ms: now_ms,
                    failed: None,
                });
            }
            SetupEvent::Downloaded { so_far } => {
                if let Screen::Installing(installing) = &mut self.screen {
                    installing.so_far = so_far.min(installing.total);
                }
            }
            SetupEvent::Stage(stage) => {
                if let Screen::Installing(installing) = &mut self.screen {
                    installing.stage = stage;
                    if stage != Stage::Downloading {
                        installing.so_far = installing.total;
                    }
                }
            }
            SetupEvent::InstallFailed { message, esc } => {
                if let Screen::Installing(installing) = &mut self.screen {
                    installing.failed = Some((message, esc));
                }
            }
            SetupEvent::Busy { step, title, line } => {
                self.step = step;
                self.screen = Screen::Busy { title, line };
            }
            SetupEvent::Waiting { step, title } => {
                self.step = step;
                self.screen = Screen::Waiting { title };
            }
            SetupEvent::Finished { code, finish } => {
                self.code = Some(code);
                match finish {
                    Finish::Ready(summary) => {
                        self.step = STEPS;
                        self.screen = Screen::Done(summary);
                    }
                    Finish::Failed { message, detail } => {
                        self.screen = Screen::Failed { message, detail };
                    }
                    Finish::Left => return Landed::Gone { code },
                }
                return Landed::Finished { code };
            }
        }
        Landed::Stay
    }

    fn on_key_update(
        &mut self,
        provider: Provider,
        state: KeyState,
        dots: Option<usize>,
        note: Option<String>,
    ) {
        let Screen::Keys(keys) = &mut self.screen else {
            return;
        };
        let selected = keys.selected;
        let Some((index, field)) = keys.by_provider(provider) else {
            return;
        };
        // Whatever was typed meanwhile gave way to the key being checked.
        if state == KeyState::Checking {
            field.text = PromptText::default();
        }
        if let Some(dots) = dots {
            field.dots = dots;
        }
        field.note = note;
        field.state = state;
        match field.state {
            KeyState::Works => {
                field.saved = true;
                field.word = Some(KEY_WORKS.to_owned());
            }
            KeyState::Unverified => {
                field.saved = true;
                field.word = Some(KEY_UNVERIFIED.to_owned());
            }
            _ => {}
        }
        // A key that works needs nothing more: on to the next.
        if field.state == KeyState::Works && index == selected && selected + 1 < keys.fields.len() {
            keys.selected += 1;
        }
    }
}

pub const KEY_WORKS: &str = "Works";
pub const KEY_UNVERIFIED: &str = "Saved, not checked yet";

/// "3" or "0.8": what the consent line says the download costs.
pub fn about_gb(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 1.0 {
        format!("{}", gb.round() as u64)
    } else {
        format!("{gb:.1}")
    }
}

/// "1.2": the progress bar's amounts.
pub fn gb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / 1e9)
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyModifiers;

    use super::*;

    const FAR_LATER: u64 = 60_000;

    fn keys_state(fields: Vec<KeyStart>, required: bool) -> SetupState {
        let mut state = SetupState::starting();
        state.on_event(
            SetupEvent::Keys {
                step: 2,
                fields,
                required,
            },
            0,
        );
        state
    }

    fn start(provider: Provider, state: KeyState) -> KeyStart {
        KeyStart {
            provider,
            state,
            note: None,
        }
    }

    fn blank_keys(required: bool) -> SetupState {
        keys_state(
            vec![
                start(Provider::Groq, KeyState::Blank),
                start(Provider::Deepgram, KeyState::Blank),
            ],
            required,
        )
    }

    fn type_text(state: &mut SetupState, text: &str) {
        for ch in text.chars() {
            state.type_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
    }

    fn keys(state: &SetupState) -> &KeysScreen {
        match &state.screen {
            Screen::Keys(keys) => keys,
            other => panic!("not the keys screen: {other:?}"),
        }
    }

    fn update(state: &mut SetupState, provider: Provider, new: KeyState) {
        state.on_event(
            SetupEvent::KeyUpdate {
                provider,
                state: new,
                dots: None,
                note: None,
            },
            0,
        );
    }

    fn enter_label(state: &SetupState) -> Option<&'static str> {
        match state.context() {
            Context::SetupKey { enter, .. } => enter,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_question_starts_on_its_default_and_answers_with_the_row_picked() {
        let mut state = SetupState::starting();
        state.on_event(
            SetupEvent::Ask(Choose {
                step: 2,
                title: "Install Local?".to_owned(),
                body: Vec::new(),
                rows: vec![ChoiceRow::new("Not now", ""), ChoiceRow::new("Install", "")],
                picked: 0,
                note: None,
            }),
            0,
        );
        assert_eq!(state.step, 2);
        assert_eq!(state.context(), Context::SetupChoice);
        state.act(Action::Up);
        state.act(Action::Down);
        state.act(Action::Down);
        match state.act(Action::Submit) {
            Outcome::Answer(SetupAnswer::Picked(1)) => {}
            other => panic!("{other:?}"),
        }
        assert!(matches!(state.screen, Screen::Busy { .. }));
    }

    #[test]
    fn esc_on_a_question_leaves_setup_without_picking_the_row() {
        let mut state = SetupState::starting();
        state.on_event(
            SetupEvent::Ask(Choose {
                step: 2,
                title: String::new(),
                body: Vec::new(),
                rows: vec![ChoiceRow::new("Not now", ""), ChoiceRow::new("Install", "")],
                picked: 1,
                note: None,
            }),
            0,
        );
        assert!(matches!(
            state.act(Action::Close),
            Outcome::Answer(SetupAnswer::Left)
        ));
        assert!(matches!(state.screen, Screen::Leaving));
        assert_eq!(
            state.on_event(
                SetupEvent::Finished {
                    code: 4,
                    finish: Finish::Left
                },
                0
            ),
            Landed::Gone { code: 4 }
        );
    }

    #[test]
    fn a_typed_key_leaves_only_inside_the_answer_and_only_its_length_stays() {
        let mut state = blank_keys(true);
        type_text(&mut state, "gsk_secret");
        assert_eq!(keys(&state).field().shown_dots(), 10);
        assert_eq!(enter_label(&state), Some("check"));

        let Outcome::Answer(SetupAnswer::CheckKey { provider, text }) = state.act(Action::Submit)
        else {
            panic!("expected a check");
        };
        assert_eq!(provider, Provider::Groq);
        assert_eq!(text.as_str(), "gsk_secret");
        let debug = format!("{:?} {:?}", SetupAnswer::CheckKey { provider, text }, state);
        assert!(!debug.contains("gsk_secret"), "{debug}");
        let field = keys(&state).field();
        assert_eq!(field.state, KeyState::Checking);
        assert_eq!(field.shown_dots(), 10);
        assert_eq!(enter_label(&state), None, "one check at a time");
    }

    #[test]
    fn typing_is_ignored_while_a_key_is_being_checked() {
        let mut state = blank_keys(true);
        type_text(&mut state, "abc");
        state.act(Action::Submit);
        type_text(&mut state, "def");
        assert_eq!(keys(&state).field().text.chars(), 0);
    }

    #[test]
    fn a_key_that_works_moves_on_to_the_next_one() {
        let mut state = blank_keys(true);
        type_text(&mut state, "abc");
        state.act(Action::Submit);
        update(&mut state, Provider::Groq, KeyState::Works);
        assert_eq!(keys(&state).selected, 1);
        assert!(keys(&state).fields[0].saved);
    }

    #[test]
    fn a_rejected_key_stays_and_typing_starts_over() {
        let mut state = blank_keys(true);
        type_text(&mut state, "abc");
        state.act(Action::Submit);
        update(&mut state, Provider::Groq, KeyState::Rejected);
        assert_eq!(keys(&state).selected, 0);
        assert_eq!(enter_label(&state), Some("skip"));
        assert_eq!(keys(&state).field().shown_dots(), 3);

        type_text(&mut state, "x");
        let field = keys(&state).field();
        assert_eq!(field.state, KeyState::Blank);
        assert_eq!(field.shown_dots(), 1);
        assert_eq!(enter_label(&state), Some("check"));
    }

    #[test]
    fn a_check_that_cannot_tell_offers_to_save_anyway() {
        let mut state = blank_keys(true);
        type_text(&mut state, "abc");
        state.act(Action::Submit);
        update(
            &mut state,
            Provider::Groq,
            KeyState::Transient("Can't reach it".to_owned()),
        );
        assert_eq!(enter_label(&state), Some("save it"));
        assert!(matches!(
            state.act(Action::Submit),
            Outcome::Answer(SetupAnswer::SaveAnyway {
                provider: Provider::Groq
            })
        ));
        assert_eq!(keys(&state).fields[0].state, KeyState::Saving);
        assert_eq!(enter_label(&state), None);
        update(&mut state, Provider::Groq, KeyState::Unverified);
        assert_eq!(keys(&state).fields[0].word.as_deref(), Some(KEY_UNVERIFIED));
        assert!(keys(&state).fields[0].usable());
        assert!(
            matches!(state.context(), Context::SetupKey { keep: false, .. }),
            "`↵ continue` already keeps a key just saved"
        );
    }

    #[test]
    fn typing_over_a_check_that_could_not_tell_drops_the_offer() {
        let mut state = blank_keys(true);
        type_text(&mut state, "abc");
        state.act(Action::Submit);
        update(
            &mut state,
            Provider::Groq,
            KeyState::Transient("Busy, try again soon".to_owned()),
        );
        type_text(&mut state, "d");
        assert_eq!(keys(&state).field().state, KeyState::Blank);
        assert_eq!(enter_label(&state), Some("check"));
    }

    #[test]
    fn cloud_cannot_finish_without_a_usable_key() {
        let mut state = blank_keys(true);
        assert_eq!(enter_label(&state), Some("skip"));
        state.act(Action::Submit);
        assert_eq!(keys(&state).selected, 1);
        state.act(Action::Submit);
        assert_eq!(keys(&state).notice, Some("Cloud needs at least one key."));
        assert!(matches!(state.screen, Screen::Keys(_)));
    }

    #[test]
    fn after_local_no_key_is_needed() {
        let mut state = blank_keys(false);
        state.act(Action::Submit);
        assert!(matches!(
            state.act(Action::Submit),
            Outcome::Answer(SetupAnswer::KeysDone)
        ));
    }

    #[test]
    fn finishing_waits_for_a_check_still_running() {
        let mut state = blank_keys(false);
        type_text(&mut state, "abc");
        state.act(Action::Down);
        assert_eq!(keys(&state).fields[0].state, KeyState::Checking);
        assert!(matches!(state.act(Action::Submit), Outcome::Stay));
        assert_eq!(keys(&state).notice, Some("Still checking a key…"));
    }

    #[test]
    fn moving_off_a_typed_key_checks_it_rather_than_dropping_it() {
        let mut state = blank_keys(true);
        type_text(&mut state, "abc");
        assert!(matches!(
            state.act(Action::Down),
            Outcome::Answer(SetupAnswer::CheckKey {
                provider: Provider::Groq,
                ..
            })
        ));
        assert_eq!(keys(&state).selected, 1);
    }

    #[test]
    fn a_saved_key_can_be_kept_or_replaced_and_stays_until_a_new_one_works() {
        let mut state = keys_state(
            vec![
                start(Provider::Groq, KeyState::Saved),
                start(Provider::Deepgram, KeyState::Blank),
            ],
            true,
        );
        assert_eq!(
            state.context(),
            Context::SetupKey {
                enter: Some("replace"),
                keep: true,
                editable: true,
            }
        );
        state.act(Action::Submit);
        assert_eq!(keys(&state).field().state, KeyState::Replacing);
        assert_eq!(enter_label(&state), Some("keep"));

        type_text(&mut state, "new");
        state.act(Action::Submit);
        update(&mut state, Provider::Groq, KeyState::Rejected);
        // The stored key is still there, so Cloud can still finish.
        assert_eq!(enter_label(&state), Some("keep"));
        state.act(Action::Keep);
        assert_eq!(keys(&state).fields[0].state, KeyState::Saved);
        assert_eq!(keys(&state).selected, 1);
        assert!(matches!(
            state.act(Action::Submit),
            Outcome::Answer(SetupAnswer::KeysDone)
        ));
    }

    #[test]
    fn the_saved_keys_own_check_never_disturbs_a_replacement() {
        let mut state = keys_state(vec![start(Provider::Groq, KeyState::Saved)], true);
        state.act(Action::Submit);
        type_text(&mut state, "new");
        state.on_event(
            SetupEvent::SavedWord {
                provider: Provider::Groq,
                word: "Rejected".to_owned(),
            },
            0,
        );
        let field = keys(&state).field();
        assert_eq!(field.state, KeyState::Replacing);
        assert_eq!(field.text.chars(), 3);
        assert_eq!(field.word.as_deref(), Some("Rejected"));
    }

    #[test]
    fn a_key_from_the_environment_cannot_be_edited() {
        let mut state = keys_state(
            vec![
                start(Provider::Groq, KeyState::Outside),
                start(Provider::Deepgram, KeyState::Blank),
            ],
            true,
        );
        type_text(&mut state, "abc");
        assert!(matches!(state.paste("abc"), Outcome::Stay));
        let field = keys(&state).field();
        assert_eq!(field.state, KeyState::Outside);
        assert_eq!(field.text.chars(), 0);
        assert_eq!(enter_label(&state), Some("continue"));
    }

    #[test]
    fn a_paste_is_a_whole_key_and_is_checked_at_once() {
        let mut state = blank_keys(true);
        type_text(&mut state, "stale");
        let Outcome::Answer(SetupAnswer::CheckKey { text, .. }) = state.paste("  gsk_new\nrest")
        else {
            panic!("expected a check");
        };
        assert_eq!(text.as_str(), "gsk_new", "stops at the line break");
        assert_eq!(keys(&state).field().dots, 7);
        assert!(matches!(state.paste("   "), Outcome::Stay));
    }

    #[test]
    fn ctrl_v_asks_the_flow_to_read_the_clipboard() {
        let mut state = blank_keys(true);
        assert!(matches!(
            state.act(Action::PasteClipboard),
            Outcome::Answer(SetupAnswer::PasteClipboard {
                provider: Provider::Groq
            })
        ));
        state.on_event(
            SetupEvent::KeyNote {
                provider: Provider::Groq,
                note: "Nothing to paste.".to_owned(),
            },
            0,
        );
        assert_eq!(
            keys(&state).field().note.as_deref(),
            Some("Nothing to paste.")
        );
    }

    fn installing(total: u64, so_far: u64) -> Installing {
        Installing {
            total,
            so_far,
            stage: Stage::Downloading,
            started_ms: 0,
            failed: None,
        }
    }

    #[test]
    fn time_left_waits_for_enough_to_estimate_from() {
        assert_eq!(installing(100, 50).time_left(1_000), None);
        assert_eq!(installing(100, 0).time_left(FAR_LATER), None);
        assert_eq!(installing(100, 100).time_left(FAR_LATER), None);
        assert_eq!(
            installing(100, 20).time_left(FAR_LATER).as_deref(),
            Some("about 4 min left")
        );
        assert_eq!(
            installing(100, 90).time_left(FAR_LATER).as_deref(),
            Some("under a minute left")
        );
    }

    #[test]
    fn an_install_runs_through_its_stages_and_a_failure_offers_a_way_out() {
        let mut state = SetupState::starting();
        state.on_event(
            SetupEvent::InstallStarted {
                step: 2,
                total: 100,
            },
            0,
        );
        state.on_event(SetupEvent::Downloaded { so_far: 40 }, 0);
        assert_eq!(state.context(), Context::SetupBusy, "no cancel, no Esc");
        assert!(matches!(state.act(Action::Close), Outcome::Stay));

        state.on_event(
            SetupEvent::InstallFailed {
                message: "stopped".to_owned(),
                esc: "use Cloud",
            },
            0,
        );
        assert_eq!(
            state.context(),
            Context::SetupInstallFailed { esc: "use Cloud" }
        );
        assert!(matches!(
            state.act(Action::Submit),
            Outcome::Answer(SetupAnswer::Retry)
        ));
        state.on_event(
            SetupEvent::InstallStarted {
                step: 2,
                total: 100,
            },
            0,
        );
        assert_eq!(state.context(), Context::SetupBusy, "the failure is gone");
        state.on_event(SetupEvent::Stage(Stage::GettingReady), 0);
        let Screen::Installing(install) = &state.screen else {
            panic!("still installing");
        };
        assert_eq!(install.so_far, 100, "the download is done");
    }

    #[test]
    fn the_end_of_setup_waits_for_a_key_and_then_closes() {
        let mut state = SetupState::starting();
        let landed = state.on_event(
            SetupEvent::Finished {
                code: 0,
                finish: Finish::Ready(Summary {
                    rows: Vec::new(),
                    try_it: None,
                }),
            },
            0,
        );
        assert_eq!(landed, Landed::Finished { code: 0 });
        assert_eq!(state.step, STEPS);
        assert_eq!(state.context(), Context::SetupDone);
        assert!(matches!(state.act(Action::Submit), Outcome::Close));
    }

    #[test]
    fn download_sizes_read_in_plain_decimal_gigabytes() {
        assert_eq!(about_gb(2_980_914_661), "3");
        assert_eq!(about_gb(769_772_225), "0.8");
        assert_eq!(gb(1_200_000_000), "1.2");
    }
}
