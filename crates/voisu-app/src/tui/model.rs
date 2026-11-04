//! The interface's state and the pure function that changes it.
//!
//! [`update`] takes a message and the time and returns what should happen next.
//! It never touches the terminal, a socket or a file, so every behaviour here is
//! testable without any of them.

use std::fmt;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use voisu_core::{AsrMode, AsrModeStatus, Credential, DaemonState, Provider, ReadinessStatus};

use crate::config::{DeliveryMode, RenderingPolicy, WritingMode};
pub use crate::doctor::DoctorFix as Fix;
use crate::doctor::DoctorRow;

use super::keys::{self, Action, Context};
pub use super::look::Tone;
use super::text;

/// Smallest size the interface can lay out.
pub const MIN_WIDTH: u16 = 60;
pub const MIN_HEIGHT: u16 = 16;
/// Below this height the blank chrome lines go, so Settings fits without scrolling.
const ROOMY_HEIGHT: u16 = 20;
/// Dictated keystrokes can keep arriving after the daemon reports Idle.
const GUARD_AFTER_MS: u64 = 1_000;
const TOAST_MS: u64 = 5_000;
/// Failed status replies in a row before the daemon is shown as unreadable.
const FAILED_POLLS_UNREADABLE: u8 = 3;
/// Longest line a prompt accepts. Real keys and dictionary terms are far shorter.
const PROMPT_LIMIT: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tab {
    Home,
    History,
    Dictionary,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Home, Tab::History, Tab::Dictionary, Tab::Settings];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Home => "Home",
            Tab::History => "History",
            Tab::Dictionary => "Dictionary",
            Tab::Settings => "Settings",
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    fn shifted(self, by: isize) -> Tab {
        let count = Self::ALL.len() as isize;
        Self::ALL[(self.index() as isize + by).rem_euclid(count) as usize]
    }
}

/// Both clocks: monotonic for durations, wall for "48s ago".
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Now {
    pub mono_ms: u64,
    pub wall_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonSnapshot {
    pub state: DaemonState,
    pub asr: Option<AsrModeStatus>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatusPoll {
    Up(DaemonSnapshot),
    /// Nothing accepted the connection.
    Unavailable,
    /// A connection was made and the round trip failed, as a busy daemon can.
    Failed,
    /// The daemon speaks another protocol version: an old one after an update.
    Mismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DaemonView {
    Unknown,
    Unavailable,
    /// Something answers but cannot be read. It may still be dictating.
    Unreadable,
    Up(DaemonSnapshot),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SettingsSnapshot {
    pub engine: AsrMode,
    pub delivery: DeliveryMode,
    pub writing: WritingMode,
    pub rendering: RenderingPolicy,
    pub deepgram: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryItem {
    pub recorded_at_ms: u64,
    pub transcript: Option<String>,
    pub failure: Option<String>,
    pub providers: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Toast {
    pub kind: Tone,
    pub text: String,
    until_mono_ms: u64,
}

/// What must be reloaded once a change has landed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refresh {
    Settings,
    Doctor,
    Dictionary,
    Shortcut,
    /// The daemon reads this setting only when it starts.
    PendingRestart(SettingsRow),
}

/// The answer to a [`Job`]. Never contains a secret.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reply {
    Status(StatusPoll),
    /// `true` when Setup still has to run.
    SetupNeeded(bool),
    Doctor(Vec<DoctorRow>),
    Settings(SettingsSnapshot),
    /// The Trigger Key's display value, or `None` when the daemon cannot say.
    Shortcut(Option<String>),
    History(Result<Vec<HistoryItem>, String>),
    Dictionary(Result<Vec<String>, String>),
    Applied {
        kind: Tone,
        text: String,
        refresh: &'static [Refresh],
    },
    KeyChecked {
        kind: Tone,
        text: String,
    },
    ServiceDone {
        kind: Tone,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceVerb {
    Start,
    Restart,
}

/// Slow or blocking work for the worker threads.
pub enum Job {
    LoadSettings,
    Doctor,
    LoadHistory,
    LoadDictionary,
    LoadShortcut,
    SetDelivery(DeliveryMode),
    SetWriting(WritingMode),
    SetRendering(RenderingPolicy),
    SetDeepgram(bool),
    SetEngine {
        mode: AsrMode,
        /// The daemon's own word that Local is Ready.
        local_ready: bool,
    },
    AddTerm(String),
    RemoveTerm(String),
    SaveKey {
        provider: Provider,
        credential: Credential,
    },
    Copy(String),
    Service(ServiceVerb),
}

impl Job {
    /// Whether this job can take seconds, and so must not delay quick ones.
    pub fn is_slow(&self) -> bool {
        matches!(self, Job::Doctor | Job::SaveKey { .. } | Job::Service(_))
    }
}

/// Written by hand so a credential can never reach a log through `{:?}`.
impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Job::LoadSettings => f.write_str("LoadSettings"),
            Job::Doctor => f.write_str("Doctor"),
            Job::LoadHistory => f.write_str("LoadHistory"),
            Job::LoadDictionary => f.write_str("LoadDictionary"),
            Job::LoadShortcut => f.write_str("LoadShortcut"),
            Job::SetDelivery(mode) => write!(f, "SetDelivery({mode:?})"),
            Job::SetWriting(mode) => write!(f, "SetWriting({mode:?})"),
            Job::SetRendering(policy) => write!(f, "SetRendering({policy:?})"),
            Job::SetDeepgram(enabled) => write!(f, "SetDeepgram({enabled})"),
            Job::SetEngine { mode, local_ready } => write!(f, "SetEngine({mode:?}, {local_ready})"),
            Job::AddTerm(term) => write!(f, "AddTerm({term:?})"),
            Job::RemoveTerm(term) => write!(f, "RemoveTerm({term:?})"),
            Job::SaveKey { provider, .. } => write!(f, "SaveKey({provider:?}, <redacted>)"),
            Job::Copy(_) => f.write_str("Copy(<transcript>)"),
            Job::Service(verb) => write!(f, "Service({verb:?})"),
        }
    }
}

#[derive(Debug)]
pub enum Effect {
    Job(Job),
    /// Leave the screen, run the wizard on this terminal, come back.
    RunSetup,
}

pub enum Msg {
    Key(KeyEvent),
    Paste(String),
    Resize(u16, u16),
    Tick,
    Reply(Reply),
    SetupReturned { completed: bool },
}

/// One-line input. Debug is redacted: this may hold an API key.
#[derive(Default)]
pub struct PromptText(String);

impl PromptText {
    pub fn chars(&self) -> usize {
        self.0.chars().count()
    }

    /// The plain text, for prompts that are not masked.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PromptText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PromptText(<{} chars>)", self.chars())
    }
}

#[derive(Debug)]
pub enum Prompt {
    AddTerm(PromptText),
    Key(Provider, PromptText),
    ConfirmRemove(String),
    /// A setting's values, open under its row; `picked` is the highlighted one.
    Choice {
        row: SettingsRow,
        picked: usize,
    },
}

/// One value a setting can take, with one plain line about it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Choice {
    pub label: &'static str,
    pub line: &'static str,
}

const fn choice(label: &'static str, line: &'static str) -> Choice {
    Choice { label, line }
}

// Every word the TUI shows for a setting's values lives in this block, so a
// rename is one edit here. The config keeps its own TOML values.

/// The order each setting's values are offered in; `SettingsRow::choices`
/// lists them in this same order.
const ENGINES: [AsrMode; 2] = [AsrMode::Cloud, AsrMode::Local];
const DELIVERIES: [DeliveryMode; 2] = [DeliveryMode::Paste, DeliveryMode::Copy];
const WRITINGS: [WritingMode; 2] = [WritingMode::Smart, WritingMode::Literal];
const LAYOUTS: [RenderingPolicy; 3] = [
    RenderingPolicy::Natural,
    RenderingPolicy::Adaptive,
    RenderingPolicy::Structured,
];

const ENGINE_CHOICES: [Choice; 2] = [
    choice("Cloud", "Fast and accurate. Needs internet."),
    choice("Local", "Private. Stays on this computer."),
];
const DELIVERY_CHOICES: [Choice; 2] = [
    choice("Paste", "Puts your words into your window."),
    choice("Copy", "Copies only; you press Ctrl+V."),
];
const WRITING_CHOICES: [Choice; 2] = [
    choice("Clean", "Fixes punctuation, grammar and misheard words."),
    choice("Exact", "Your words, exactly as spoken."),
];
const LAYOUT_CHOICES: [Choice; 3] = [
    choice("Plain", "Punctuation only."),
    choice("Auto", "Organizes when you speak in parts."),
    choice("Sections", "Adds headings."),
];

pub fn engine_label(mode: AsrMode) -> &'static str {
    ENGINE_CHOICES[position(&ENGINES, mode)].label
}

pub fn delivery_label(mode: DeliveryMode) -> &'static str {
    DELIVERY_CHOICES[position(&DELIVERIES, mode)].label
}

pub fn writing_label(mode: WritingMode) -> &'static str {
    WRITING_CHOICES[position(&WRITINGS, mode)].label
}

pub fn layout_label(policy: RenderingPolicy) -> &'static str {
    LAYOUT_CHOICES[position(&LAYOUTS, policy)].label
}

/// A provider key's check result in plain words.
fn key_word(value: &str) -> String {
    match value {
        "valid" => "Works".to_owned(),
        "invalid" => "Rejected".to_owned(),
        "rate-limited" => "Busy, try again soon".to_owned(),
        "quota exhausted" => "Out of free use".to_owned(),
        "unreachable" => "Can't reach it".to_owned(),
        other => text::capitalized(other),
    }
}

fn position<T: PartialEq>(values: &[T], value: T) -> usize {
    values
        .iter()
        .position(|candidate| *candidate == value)
        .unwrap_or(0)
}

/// One Home problem's fix: the one key rule 3 promises.
impl Fix {
    pub fn label(self) -> &'static str {
        match self {
            Fix::RunSetup => "Run setup",
            Fix::StartVoisu => "Start Voisu",
            Fix::RestartVoisu => "Restart Voisu",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Hero {
    Checking,
    /// Running but unreadable, as an old daemon is after an update.
    NeedsRestart,
    NotSetUp,
    NotRunning,
    Failing {
        label: String,
        value: Option<String>,
        action: Option<String>,
        fix: Option<Fix>,
    },
    Recording {
        seconds: u64,
    },
    Processing,
    Ready,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingsRow {
    Engine,
    Delivery,
    Writing,
    Rendering,
    Deepgram,
    GroqKey,
    DeepgramKey,
    TriggerKey,
    RunSetup,
}

impl SettingsRow {
    pub const ALL: [SettingsRow; 9] = [
        SettingsRow::Engine,
        SettingsRow::Delivery,
        SettingsRow::Writing,
        SettingsRow::Rendering,
        SettingsRow::Deepgram,
        SettingsRow::GroqKey,
        SettingsRow::DeepgramKey,
        SettingsRow::TriggerKey,
        SettingsRow::RunSetup,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsRow::Engine => "Engine",
            SettingsRow::Delivery => "Delivery",
            SettingsRow::Writing => "Writing",
            SettingsRow::Rendering => "Layout",
            SettingsRow::Deepgram => "Deepgram",
            SettingsRow::GroqKey => "Groq key",
            SettingsRow::DeepgramKey => "Deepgram key",
            SettingsRow::TriggerKey => "Trigger Key",
            SettingsRow::RunSetup => "Run setup",
        }
    }

    /// The values `↵` offers, or nothing for a row that is not a choice.
    pub fn choices(self) -> &'static [Choice] {
        match self {
            SettingsRow::Engine => &ENGINE_CHOICES,
            SettingsRow::Delivery => &DELIVERY_CHOICES,
            SettingsRow::Writing => &WRITING_CHOICES,
            SettingsRow::Rendering => &LAYOUT_CHOICES,
            _ => &[],
        }
    }

    fn enter_label(self) -> Option<&'static str> {
        match self {
            SettingsRow::TriggerKey => None,
            SettingsRow::GroqKey | SettingsRow::DeepgramKey => Some("enter key"),
            SettingsRow::RunSetup => Some("run setup"),
            _ => Some("change"),
        }
    }

    /// One plain line about what the setting does, kept short enough to fit
    /// two rows at the minimum width.
    pub fn description(self) -> &'static str {
        match self {
            SettingsRow::Engine => "Where your speech becomes text.",
            SettingsRow::Delivery => "How your words reach your window.",
            SettingsRow::Writing => "How much Voisu tidies your words.",
            SettingsRow::Rendering => "How your words are laid out.",
            SettingsRow::Deepgram => {
                "Uses Deepgram alongside Groq for better accuracy on technical \
                 words. Off uses Groq alone."
            }
            SettingsRow::GroqKey => {
                "Your Groq key. Press ↵ to enter a new one; it is checked before it is saved."
            }
            SettingsRow::DeepgramKey => {
                "Your Deepgram key. Press ↵ to enter a new one; it is checked before it is saved."
            }
            SettingsRow::TriggerKey => {
                "The shortcut that starts and stops a Recording. You can't change it here."
            }
            SettingsRow::RunSetup => {
                "Walks through your keys, Engine and Trigger Key again. Safe to re-run."
            }
        }
    }
}

pub struct App {
    pub tab: Tab,
    pub size: (u16, u16),
    pub now: Now,
    pub daemon: DaemonView,
    pub setup_needed: Option<bool>,
    pub doctor: Option<Vec<DoctorRow>>,
    doctor_in_flight: bool,
    doctor_stale: bool,
    pub settings: Option<SettingsSnapshot>,
    pub shortcut: Option<String>,
    pub history: Option<Vec<HistoryItem>>,
    pub history_error: Option<String>,
    pub history_selected: usize,
    pub detail_scroll: usize,
    pub terms: Option<Vec<String>>,
    pub terms_error: Option<String>,
    pub term_selected: usize,
    pub settings_selected: usize,
    pub checks_open: bool,
    pub checks_scroll: usize,
    pub prompt: Option<Prompt>,
    pub overlay: bool,
    pub toast: Option<Toast>,
    pub key_in_check: Option<Provider>,
    pub service_busy: Option<ServiceVerb>,
    /// Whether the service call in flight, if it succeeds, starts a daemon that
    /// reads every setting afresh: a Restart, or a Start of a stopped daemon.
    fresh_daemon_on_success: bool,
    /// Settings changed since the daemon started, which it reads only then.
    pub restart_pending: Vec<SettingsRow>,
    pub should_quit: bool,
    failed_polls: u8,
    guard_until_mono_ms: u64,
    recording_since_mono_ms: Option<u64>,
    dirty: bool,
}

impl App {
    pub fn new(size: (u16, u16), now: Now) -> Self {
        Self {
            tab: Tab::Home,
            size,
            now,
            daemon: DaemonView::Unknown,
            setup_needed: None,
            doctor: None,
            doctor_in_flight: false,
            doctor_stale: false,
            settings: None,
            shortcut: None,
            history: None,
            history_error: None,
            history_selected: 0,
            detail_scroll: 0,
            terms: None,
            terms_error: None,
            term_selected: 0,
            settings_selected: 0,
            checks_open: false,
            checks_scroll: 0,
            prompt: None,
            overlay: false,
            toast: None,
            key_in_check: None,
            service_busy: None,
            fresh_daemon_on_success: false,
            restart_pending: Vec::new(),
            should_quit: false,
            failed_polls: 0,
            guard_until_mono_ms: 0,
            recording_since_mono_ms: None,
            dirty: true,
        }
    }

    /// The first round of work. History and the Trigger Key wait for the first
    /// status poll, since both need a daemon.
    pub fn start(&mut self) -> Vec<Effect> {
        let mut effects = vec![
            Effect::Job(Job::LoadSettings),
            Effect::Job(Job::LoadDictionary),
        ];
        self.request_doctor(&mut effects);
        effects
    }

    /// Whether anything visible changed since the last call.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub fn too_small(&self) -> bool {
        self.size.0 < MIN_WIDTH || self.size.1 < MIN_HEIGHT
    }

    pub fn roomy(&self) -> bool {
        self.size.1 >= ROOMY_HEIGHT
    }

    /// Rows available to the active tab's body.
    pub fn body_height(&self) -> usize {
        let chrome = if self.roomy() { 7 } else { 5 };
        (self.size.1 as usize).saturating_sub(chrome)
    }

    /// An unreadable daemon counts: it may be dictating into this terminal.
    fn daemon_active(&self) -> bool {
        match &self.daemon {
            DaemonView::Up(snapshot) => snapshot.state != DaemonState::Idle,
            DaemonView::Unreadable => true,
            DaemonView::Unknown | DaemonView::Unavailable => false,
        }
    }

    pub fn daemon_down(&self) -> bool {
        matches!(
            self.daemon,
            DaemonView::Unavailable | DaemonView::Unreadable
        )
    }

    fn can_restart(&self) -> bool {
        self.service_busy.is_none()
    }

    /// True while keystrokes may be the user's own dictation arriving here.
    pub fn guarded(&self) -> bool {
        self.daemon_active() || self.now.mono_ms < self.guard_until_mono_ms
    }

    pub fn base_context(&self) -> Context {
        match self.tab {
            Tab::Home => Context::Home {
                fix: self.fix().map(Fix::label),
                checks: self.checks_open,
                restart: self.can_restart(),
            },
            Tab::History => Context::History {
                has_records: self.history.as_ref().is_some_and(|items| !items.is_empty()),
            },
            Tab::Dictionary => Context::Dictionary {
                has_terms: self.terms.as_ref().is_some_and(|terms| !terms.is_empty()),
            },
            Tab::Settings => Context::Settings {
                enter: self.selected_setting().enter_label(),
                restart: self.can_restart(),
            },
        }
    }

    pub fn context(&self) -> Context {
        if self.guarded() {
            Context::Guarded {
                restart: self.daemon == DaemonView::Unreadable && self.can_restart(),
            }
        } else if self.too_small() {
            Context::TooSmall
        } else if let Some(prompt) = &self.prompt {
            match prompt {
                Prompt::AddTerm(_) => Context::AddTermPrompt,
                Prompt::Key(..) => Context::KeyPrompt,
                Prompt::ConfirmRemove(_) => Context::ConfirmPrompt,
                Prompt::Choice { .. } => Context::ChoicePrompt,
            }
        } else if self.overlay {
            Context::Overlay
        } else {
            self.base_context()
        }
    }

    pub fn selected_setting(&self) -> SettingsRow {
        SettingsRow::ALL[self.settings_selected.min(SettingsRow::ALL.len() - 1)]
    }

    /// The most important thing right now. First match wins.
    pub fn hero(&self) -> Hero {
        if self.daemon == DaemonView::Unreadable {
            return Hero::NeedsRestart;
        }
        if self.setup_needed == Some(true) {
            return Hero::NotSetUp;
        }
        // Until the Setup check answers, a stopped daemon may only mean Setup
        // never ran, so "isn't running" would flash the wrong problem.
        if self.setup_needed.is_none() && !self.daemon_active() {
            return Hero::Checking;
        }
        if self.daemon == DaemonView::Unavailable {
            return Hero::NotRunning;
        }
        if let Some(row) = self
            .doctor
            .iter()
            .flatten()
            .find(|row| row.status == ReadinessStatus::Fail)
        {
            return Hero::Failing {
                label: row.label.clone(),
                value: row.value.clone(),
                action: row.action.clone(),
                fix: row.fix,
            };
        }
        match &self.daemon {
            DaemonView::Up(snapshot) if snapshot.state == DaemonState::Recording => {
                Hero::Recording {
                    seconds: self.recording_seconds().unwrap_or(0),
                }
            }
            DaemonView::Up(snapshot) if snapshot.state == DaemonState::Processing => {
                Hero::Processing
            }
            DaemonView::Up(_) if self.setup_needed == Some(false) => Hero::Ready,
            _ => Hero::Checking,
        }
    }

    pub fn fix(&self) -> Option<Fix> {
        if self.service_busy.is_some() {
            return None;
        }
        match self.hero() {
            Hero::NotSetUp => Some(Fix::RunSetup),
            Hero::NotRunning => Some(Fix::StartVoisu),
            Hero::Failing { fix, .. } => fix,
            // Below any real problem or live Recording, a change that waits for
            // a restart offers one.
            Hero::Ready if !self.restart_pending.is_empty() => Some(Fix::RestartVoisu),
            _ => None,
        }
    }

    fn recording_seconds(&self) -> Option<u64> {
        self.recording_since_mono_ms
            .map(|since| self.now.mono_ms.saturating_sub(since) / 1_000)
    }

    pub fn engine(&self) -> Option<AsrMode> {
        if let DaemonView::Up(snapshot) = &self.daemon
            && let Some(pending) = snapshot.asr.as_ref().and_then(|asr| asr.pending)
        {
            return Some(pending);
        }
        self.settings.map(|settings| settings.engine)
    }

    /// `All 16 checks passed`, or `15 passed · 1 warning`. SKIP rows are checks
    /// that do not apply here, so they count for nothing.
    pub fn health(&self) -> Option<(String, Tone)> {
        let rows = self.doctor.as_ref()?;
        let count = |status| rows.iter().filter(|row| row.status == status).count();
        let (passed, warned, failed) = (
            count(ReadinessStatus::Pass),
            count(ReadinessStatus::Warn),
            count(ReadinessStatus::Fail),
        );
        if warned == 0 && failed == 0 {
            return Some((format!("All {passed} checks passed"), Tone::Success));
        }
        let mut parts = vec![format!("{passed} passed")];
        if failed > 0 {
            parts.push(format!("{failed} failed"));
        }
        if warned > 0 {
            parts.push(format!(
                "{warned} warning{}",
                if warned == 1 { "" } else { "s" }
            ));
        }
        let tone = if failed > 0 {
            Tone::Failure
        } else {
            Tone::Caution
        };
        Some((parts.join(" · "), tone))
    }

    /// The doctor list as lines: FAIL, then WARN, then PASS, with the action
    /// under each failure.
    pub fn check_lines(&self) -> Vec<CheckLine> {
        let mut lines = Vec::new();
        for status in [
            ReadinessStatus::Fail,
            ReadinessStatus::Warn,
            ReadinessStatus::Pass,
        ] {
            for row in self
                .doctor
                .iter()
                .flatten()
                .filter(|row| row.status == status)
            {
                lines.push(CheckLine::Row {
                    status,
                    label: row.label.clone(),
                    value: row.value.clone().unwrap_or_default(),
                });
                if status == ReadinessStatus::Fail
                    && let Some(action) = &row.action
                {
                    lines.push(CheckLine::Action(action.clone()));
                }
            }
        }
        lines
    }

    /// Rows the doctor list can use below the hero.
    pub fn checks_height(&self) -> usize {
        self.body_height().saturating_sub(HERO_HEIGHT)
    }

    /// The display value and tone of a settings row.
    pub fn setting_value(&self, row: SettingsRow) -> (String, Tone) {
        let plain = |text: String| (text, Tone::Plain);
        let loading = || ("Checking…".to_owned(), Tone::Plain);
        let (value, tone) = match row {
            SettingsRow::Engine => self
                .engine()
                .map_or_else(loading, |mode| plain(engine_label(mode).to_owned())),
            SettingsRow::Delivery => self.settings.map_or_else(loading, |settings| {
                plain(delivery_label(settings.delivery).to_owned())
            }),
            SettingsRow::Writing => self.settings.map_or_else(loading, |settings| {
                plain(writing_label(settings.writing).to_owned())
            }),
            SettingsRow::Rendering => self.settings.map_or_else(loading, |settings| {
                plain(layout_label(settings.rendering).to_owned())
            }),
            SettingsRow::Deepgram => self.settings.map_or_else(loading, |settings| {
                plain(if settings.deepgram { "On" } else { "Off" }.to_owned())
            }),
            SettingsRow::GroqKey => self.key_value(Provider::Groq),
            SettingsRow::DeepgramKey => self.key_value(Provider::Deepgram),
            SettingsRow::TriggerKey => match (&self.shortcut, &self.daemon) {
                (Some(value), _) => plain(value.clone()),
                (None, _) if self.daemon_down() => plain("Unknown until Voisu runs".to_owned()),
                (None, _) => loading(),
            },
            SettingsRow::RunSetup => plain(String::new()),
        };
        if self.restart_pending.contains(&row) && self.settings.is_some() {
            return (format!("{value} · after restart"), Tone::Caution);
        }
        (value, tone)
    }

    fn key_value(&self, provider: Provider) -> (String, Tone) {
        if self.key_in_check == Some(provider) {
            return ("Checking…".to_owned(), Tone::Plain);
        }
        let Some(rows) = &self.doctor else {
            return ("Checking…".to_owned(), Tone::Plain);
        };
        let label = format!("{} key", provider.cli_label());
        let Some(row) = rows
            .iter()
            .find(|row| row.label.eq_ignore_ascii_case(&label))
        else {
            return ("Not checked".to_owned(), Tone::Plain);
        };
        let tone = match row.status {
            ReadinessStatus::Pass => Tone::Success,
            ReadinessStatus::Warn => Tone::Caution,
            ReadinessStatus::Fail => Tone::Failure,
            ReadinessStatus::Skip => Tone::Plain,
        };
        (key_word(row.value.as_deref().unwrap_or("unknown")), tone)
    }

    /// The transcript pane's text for the selected record, wrapped to `width`.
    pub fn detail_lines(&self, width: usize) -> Vec<String> {
        let Some(item) = self.selected_record() else {
            return Vec::new();
        };
        match (&item.transcript, &item.failure) {
            (Some(transcript), _) => text::wrap(transcript, width),
            (None, Some(failure)) => text::wrap(failure, width),
            (None, None) => Vec::new(),
        }
    }

    pub fn selected_record(&self) -> Option<&HistoryItem> {
        self.history.as_ref()?.get(self.history_selected)
    }

    /// Rows for the record list and for the transcript pane. The pane also has
    /// a blank line above it and the provider line below it.
    pub fn history_split(&self) -> (usize, usize) {
        let body = self.body_height();
        let list = (body / 3).max(3);
        (list, body.saturating_sub(list + 2))
    }

    /// Width of the transcript pane, inside the left margin.
    pub fn detail_width(&self) -> usize {
        (self.size.0 as usize).saturating_sub(4)
    }

    fn max_detail_scroll(&self) -> usize {
        let (_, pane) = self.history_split();
        self.detail_lines(self.detail_width())
            .len()
            .saturating_sub(pane)
    }

    fn show(&mut self, kind: Tone, text: impl Into<String>) {
        self.toast = Some(Toast {
            kind,
            text: text.into(),
            until_mono_ms: self.now.mono_ms + TOAST_MS,
        });
        self.dirty = true;
    }

    fn request_doctor(&mut self, effects: &mut Vec<Effect>) {
        if self.doctor_in_flight {
            self.doctor_stale = true;
        } else {
            self.doctor_in_flight = true;
            effects.push(Effect::Job(Job::Doctor));
        }
    }

    fn apply_refresh(&mut self, refresh: &[Refresh], effects: &mut Vec<Effect>) {
        for item in refresh {
            match item {
                Refresh::Settings => effects.push(Effect::Job(Job::LoadSettings)),
                Refresh::Doctor => self.request_doctor(effects),
                Refresh::Dictionary => effects.push(Effect::Job(Job::LoadDictionary)),
                Refresh::Shortcut => effects.push(Effect::Job(Job::LoadShortcut)),
                Refresh::PendingRestart(row) => {
                    if !self.restart_pending.contains(row) {
                        self.restart_pending.push(*row);
                    }
                }
            }
        }
    }

    fn on_status(&mut self, poll: StatusPoll, effects: &mut Vec<Effect>) {
        self.failed_polls = if poll == StatusPoll::Failed {
            self.failed_polls.saturating_add(1)
        } else {
            0
        };
        let next = match poll {
            StatusPoll::Up(snapshot) => DaemonView::Up(snapshot),
            StatusPoll::Unavailable => DaemonView::Unavailable,
            StatusPoll::Mismatch => DaemonView::Unreadable,
            // One slow reply, as a busy daemon can give mid-Recording, changes
            // nothing; only a daemon that keeps failing to answer is shown.
            StatusPoll::Failed if self.failed_polls < FAILED_POLLS_UNREADABLE => return,
            StatusPoll::Failed => DaemonView::Unreadable,
        };
        if next == self.daemon {
            return;
        }
        let was_active = self.daemon_active();
        let was_known = self.daemon != DaemonView::Unknown;
        let was_up = matches!(self.daemon, DaemonView::Up(_));
        let was_stopped = self.daemon == DaemonView::Unavailable;
        self.daemon = next;
        self.dirty = true;
        let is_up = matches!(self.daemon, DaemonView::Up(_));
        let active = self.daemon_active();

        if was_active && !active {
            self.guard_until_mono_ms = self.now.mono_ms + GUARD_AFTER_MS;
            effects.push(Effect::Job(Job::LoadHistory));
        }
        let recording =
            matches!(&self.daemon, DaemonView::Up(s) if s.state == DaemonState::Recording);
        if !recording {
            self.recording_since_mono_ms = None;
        } else if self.recording_since_mono_ms.is_none() {
            self.recording_since_mono_ms = Some(self.now.mono_ms);
        }
        if was_up != is_up {
            // A daemon seen stopped and back has read every setting afresh. One
            // that was only unreadable may simply have been slow.
            if is_up && was_stopped {
                self.restart_pending.clear();
            }
            if is_up {
                effects.push(Effect::Job(Job::LoadHistory));
                effects.push(Effect::Job(Job::LoadShortcut));
            } else {
                self.shortcut = None;
            }
            // The first poll answers what the startup doctor run is already
            // asking, so only a real change of availability runs it again.
            if was_known {
                self.request_doctor(effects);
            }
        }
    }

    fn on_reply(&mut self, reply: Reply, effects: &mut Vec<Effect>) {
        match reply {
            Reply::Status(poll) => self.on_status(poll, effects),
            Reply::SetupNeeded(needed) => {
                self.dirty |= self.setup_needed != Some(needed);
                self.setup_needed = Some(needed);
            }
            Reply::Doctor(rows) => {
                self.dirty |= self.doctor.as_ref() != Some(&rows);
                self.doctor = Some(rows);
                self.doctor_in_flight = false;
                if std::mem::take(&mut self.doctor_stale) {
                    self.request_doctor(effects);
                }
            }
            Reply::Settings(settings) => {
                self.dirty |= self.settings != Some(settings);
                self.settings = Some(settings);
            }
            Reply::Shortcut(value) => {
                self.dirty |= self.shortcut != value;
                self.shortcut = value;
            }
            Reply::History(Ok(items)) => {
                if self.history.as_ref() != Some(&items) || self.history_error.is_some() {
                    self.dirty = true;
                    self.history_selected =
                        self.history_selected.min(items.len().saturating_sub(1));
                    self.detail_scroll = 0;
                    self.history = Some(items);
                    self.history_error = None;
                }
            }
            Reply::History(Err(message)) => {
                self.dirty |= self.history_error.as_deref() != Some(&message);
                self.history_error = Some(message);
            }
            Reply::Dictionary(Ok(terms)) => {
                if self.terms.as_ref() != Some(&terms) || self.terms_error.is_some() {
                    self.dirty = true;
                    self.term_selected = self.term_selected.min(terms.len().saturating_sub(1));
                    self.terms = Some(terms);
                    self.terms_error = None;
                }
            }
            Reply::Dictionary(Err(message)) => {
                self.dirty |= self.terms_error.as_deref() != Some(&message);
                self.terms_error = Some(message);
            }
            Reply::Applied {
                kind,
                text,
                refresh,
            } => {
                self.show(kind, text);
                self.apply_refresh(refresh, effects);
            }
            Reply::KeyChecked { kind, text } => {
                self.key_in_check = None;
                self.show(kind, text);
                self.request_doctor(effects);
            }
            Reply::ServiceDone { kind, text } => {
                self.service_busy = None;
                // A Start that found the daemon already running reloads nothing.
                if kind == Tone::Success && std::mem::take(&mut self.fresh_daemon_on_success) {
                    self.restart_pending.clear();
                }
                self.show(kind, text);
                self.request_doctor(effects);
            }
        }
    }

    fn set_tab(&mut self, tab: Tab, effects: &mut Vec<Effect>) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.dirty = true;
        match tab {
            Tab::Home => effects.push(Effect::Job(Job::LoadSettings)),
            Tab::History => effects.push(Effect::Job(Job::LoadHistory)),
            Tab::Dictionary => effects.push(Effect::Job(Job::LoadDictionary)),
            Tab::Settings => {
                effects.push(Effect::Job(Job::LoadSettings));
                effects.push(Effect::Job(Job::LoadShortcut));
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent, effects: &mut Vec<Effect>) {
        let context = self.context();
        if let Some(action) = keys::action_for(&context, key) {
            self.apply(action, effects);
        } else if matches!(context, Context::AddTermPrompt | Context::KeyPrompt) {
            self.edit_prompt(key);
        }
    }

    fn apply(&mut self, action: Action, effects: &mut Vec<Effect>) {
        match action {
            Action::Quit => self.should_quit = true,
            Action::PrevTab => self.set_tab(self.tab.shifted(-1), effects),
            Action::NextTab => self.set_tab(self.tab.shifted(1), effects),
            Action::GoTab(tab) => self.set_tab(tab, effects),
            Action::Up => self.move_selection(-1),
            Action::Down => self.move_selection(1),
            Action::PageUp => self.scroll_detail(-1),
            Action::PageDown => self.scroll_detail(1),
            Action::Activate => self.activate(effects),
            Action::Restart => {
                if self.can_restart() {
                    self.start_service(ServiceVerb::Restart, effects);
                }
            }
            Action::ToggleChecks => {
                self.checks_open = !self.checks_open;
                self.checks_scroll = 0;
                self.dirty = true;
                if self.checks_open {
                    self.request_doctor(effects);
                }
            }
            Action::Copy => self.copy_selected(effects),
            Action::AddTerm => {
                self.prompt = Some(Prompt::AddTerm(PromptText::default()));
                self.dirty = true;
            }
            Action::AskRemove => {
                if let Some(term) = self
                    .terms
                    .as_ref()
                    .and_then(|terms| terms.get(self.term_selected))
                {
                    self.prompt = Some(Prompt::ConfirmRemove(term.clone()));
                    self.dirty = true;
                }
            }
            Action::OpenHelp => {
                self.overlay = true;
                self.dirty = true;
            }
            Action::Close => {
                self.prompt = None;
                self.overlay = false;
                self.dirty = true;
            }
            Action::Submit => self.submit_prompt(effects),
            Action::ConfirmRemove => {
                if let Some(Prompt::ConfirmRemove(term)) = self.prompt.take() {
                    self.dirty = true;
                    effects.push(Effect::Job(Job::RemoveTerm(term)));
                }
            }
        }
    }

    fn move_selection(&mut self, by: isize) {
        fn moved(value: usize, by: isize, limit: usize) -> usize {
            value.saturating_add_signed(by).min(limit)
        }
        if let Some(Prompt::Choice { row, picked }) = &mut self.prompt {
            *picked = moved(*picked, by, row.choices().len() - 1);
            self.dirty = true;
            return;
        }
        match self.tab {
            Tab::Home => {
                let limit = self
                    .check_lines()
                    .len()
                    .saturating_sub(self.checks_height());
                self.checks_scroll = moved(self.checks_scroll, by, limit);
            }
            Tab::History => {
                let limit = self
                    .history
                    .as_ref()
                    .map_or(0, |items| items.len().saturating_sub(1));
                let next = moved(self.history_selected, by, limit);
                if next != self.history_selected {
                    self.detail_scroll = 0;
                }
                self.history_selected = next;
            }
            Tab::Dictionary => {
                let limit = self
                    .terms
                    .as_ref()
                    .map_or(0, |terms| terms.len().saturating_sub(1));
                self.term_selected = moved(self.term_selected, by, limit);
            }
            Tab::Settings => {
                self.settings_selected =
                    moved(self.settings_selected, by, SettingsRow::ALL.len() - 1);
            }
        }
        self.dirty = true;
    }

    fn scroll_detail(&mut self, pages: isize) {
        let (_, pane) = self.history_split();
        let step = pane.max(1) as isize * pages;
        self.detail_scroll = self
            .detail_scroll
            .saturating_add_signed(step)
            .min(self.max_detail_scroll());
        self.dirty = true;
    }

    fn activate(&mut self, effects: &mut Vec<Effect>) {
        match self.tab {
            Tab::Home => match self.fix() {
                Some(Fix::RunSetup) => effects.push(Effect::RunSetup),
                Some(Fix::StartVoisu) => self.start_service(ServiceVerb::Start, effects),
                Some(Fix::RestartVoisu) => self.start_service(ServiceVerb::Restart, effects),
                None => {}
            },
            Tab::Settings => self.activate_setting(effects),
            Tab::History | Tab::Dictionary => {}
        }
    }

    fn start_service(&mut self, verb: ServiceVerb, effects: &mut Vec<Effect>) {
        self.service_busy = Some(verb);
        self.fresh_daemon_on_success =
            verb == ServiceVerb::Restart || !matches!(self.daemon, DaemonView::Up(_));
        self.dirty = true;
        effects.push(Effect::Job(Job::Service(verb)));
    }

    /// `↵` on a settings row. A row with values opens them inline, so one
    /// stray Enter never changes anything; On/Off rows flip at once.
    fn activate_setting(&mut self, effects: &mut Vec<Effect>) {
        let row = self.selected_setting();
        match row {
            SettingsRow::Engine
            | SettingsRow::Delivery
            | SettingsRow::Writing
            | SettingsRow::Rendering => {
                let Some(picked) = self.current_choice(row) else {
                    return;
                };
                self.prompt = Some(Prompt::Choice { row, picked });
            }
            SettingsRow::Deepgram => {
                if let Some(settings) = &mut self.settings {
                    settings.deepgram = !settings.deepgram;
                    effects.push(Effect::Job(Job::SetDeepgram(settings.deepgram)));
                }
            }
            SettingsRow::GroqKey => {
                self.prompt = Some(Prompt::Key(Provider::Groq, PromptText::default()));
            }
            SettingsRow::DeepgramKey => {
                self.prompt = Some(Prompt::Key(Provider::Deepgram, PromptText::default()));
            }
            SettingsRow::TriggerKey => return,
            SettingsRow::RunSetup => effects.push(Effect::RunSetup),
        }
        self.dirty = true;
    }

    /// Which of `row.choices()` is in effect now, once it is known.
    pub fn current_choice(&self, row: SettingsRow) -> Option<usize> {
        let settings = self.settings?;
        Some(match row {
            SettingsRow::Engine => position(&ENGINES, self.engine()?),
            SettingsRow::Delivery => position(&DELIVERIES, settings.delivery),
            SettingsRow::Writing => position(&WRITINGS, settings.writing),
            SettingsRow::Rendering => position(&LAYOUTS, settings.rendering),
            _ => return None,
        })
    }

    /// `↵` in an open choice. Picking the value already in effect changes
    /// nothing. The screen moves at once and the reload that follows confirms
    /// it, except Engine, which waits for the worker: Local needs consent.
    fn choose(&mut self, row: SettingsRow, picked: usize, effects: &mut Vec<Effect>) {
        if self.current_choice(row) == Some(picked) {
            return;
        }
        let daemon_says_local_ready = self.daemon_says_local_ready();
        let Some(settings) = &mut self.settings else {
            return;
        };
        let job = match row {
            SettingsRow::Engine => {
                let mode = ENGINES[picked];
                let local_ready = mode == AsrMode::Local && daemon_says_local_ready;
                Job::SetEngine { mode, local_ready }
            }
            SettingsRow::Delivery => {
                settings.delivery = DELIVERIES[picked];
                Job::SetDelivery(settings.delivery)
            }
            SettingsRow::Writing => {
                settings.writing = WRITINGS[picked];
                Job::SetWriting(settings.writing)
            }
            SettingsRow::Rendering => {
                settings.rendering = LAYOUTS[picked];
                Job::SetRendering(settings.rendering)
            }
            _ => return,
        };
        effects.push(Effect::Job(job));
    }

    fn daemon_says_local_ready(&self) -> bool {
        matches!(
            &self.daemon,
            DaemonView::Up(snapshot)
                if snapshot.asr.as_ref().is_some_and(|asr| asr.local_readiness.is_ready())
        )
    }

    fn copy_selected(&mut self, effects: &mut Vec<Effect>) {
        match self.selected_record().map(|item| item.transcript.clone()) {
            Some(Some(transcript)) => effects.push(Effect::Job(Job::Copy(transcript))),
            Some(None) => self.show(Tone::Caution, "This Recording has no Transcript to copy."),
            None => {}
        }
    }

    fn edit_prompt(&mut self, key: KeyEvent) {
        let (Some(Prompt::AddTerm(text)) | Some(Prompt::Key(_, text))) = &mut self.prompt else {
            return;
        };
        match key.code {
            KeyCode::Backspace => {
                text.0.pop();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !ch.is_control()
                    && text.0.len() < PROMPT_LIMIT =>
            {
                text.0.push(ch);
            }
            _ => return,
        }
        self.dirty = true;
    }

    fn submit_prompt(&mut self, effects: &mut Vec<Effect>) {
        match self.prompt.take() {
            Some(Prompt::AddTerm(text)) => {
                let term = text.0.trim();
                if !term.is_empty() {
                    effects.push(Effect::Job(Job::AddTerm(term.to_owned())));
                }
            }
            Some(Prompt::Key(provider, text)) => {
                // The value goes straight into a Credential and out of the
                // model; `text` is dropped when this arm ends.
                match Credential::new(text.0.trim().to_owned()) {
                    Ok(credential) => {
                        self.key_in_check = Some(provider);
                        effects.push(Effect::Job(Job::SaveKey {
                            provider,
                            credential,
                        }));
                    }
                    Err(_) => {
                        self.prompt = Some(Prompt::Key(provider, text));
                        self.show(Tone::Caution, "Enter a key, or press Esc to cancel.");
                    }
                }
            }
            Some(Prompt::Choice { row, picked }) => self.choose(row, picked, effects),
            other => self.prompt = other,
        }
        self.dirty = true;
    }

    /// Bracketed paste is discarded: pasted text must never act as keystrokes.
    /// A masked key prompt is the one exception, because a key that cannot be
    /// pasted cannot realistically be entered.
    fn on_paste(&mut self, pasted: String) {
        if self.context() != Context::KeyPrompt {
            return;
        }
        let Some(Prompt::Key(_, text)) = &mut self.prompt else {
            return;
        };
        for ch in pasted.trim().chars() {
            if ch.is_control() || text.0.len() >= PROMPT_LIMIT {
                break;
            }
            text.0.push(ch);
        }
        self.dirty = true;
    }
}

/// Rows the hero uses at the top of Home, including its trailing blank line.
pub const HERO_HEIGHT: usize = 3;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckLine {
    Row {
        status: ReadinessStatus,
        label: String,
        value: String,
    },
    Action(String),
}

/// The Trigger Key's display value from the daemon's `shortcut` answer.
pub fn trigger_key_value(message: &str) -> String {
    match message.strip_prefix("Trigger Key: ") {
        Some(description) => text::clean_line(description),
        None => "Not bound".to_owned(),
    }
}

/// Consent is never bypassed: Local is reachable only when the daemon says it
/// is Ready or the user has already made a Consented Install.
pub fn local_switch_allowed(daemon_says_ready: bool, consented_install: bool) -> bool {
    daemon_says_ready || consented_install
}

/// Changes `app` for one message and says what should happen next.
pub fn update(app: &mut App, msg: Msg, now: Now) -> Vec<Effect> {
    let mut effects = Vec::new();
    let before = std::mem::replace(&mut app.now, now);
    match msg {
        Msg::Key(key) => app.on_key(key, &mut effects),
        Msg::Paste(pasted) => app.on_paste(pasted),
        Msg::Resize(width, height) => {
            app.size = (width, height);
            app.dirty = true;
        }
        Msg::Tick => {}
        Msg::Reply(reply) => app.on_reply(reply, &mut effects),
        Msg::SetupReturned { completed } => {
            if completed {
                app.show(Tone::Success, "Setup finished.");
            } else {
                app.show(Tone::Caution, "Setup didn't finish.");
            }
            app.setup_needed = None;
            effects.push(Effect::Job(Job::LoadSettings));
            effects.push(Effect::Job(Job::LoadShortcut));
            app.request_doctor(&mut effects);
        }
    }
    if app
        .toast
        .as_ref()
        .is_some_and(|toast| now.mono_ms >= toast.until_mono_ms)
    {
        app.toast = None;
        app.dirty = true;
    }
    // What time alone changes on screen: the Recording timer, the guard
    // lapsing, "48s ago", and the minute.
    let face = |time: Now| {
        (
            app.recording_since_mono_ms
                .map(|since| time.mono_ms.saturating_sub(since) / 1_000),
            time.mono_ms < app.guard_until_mono_ms,
            app.history
                .as_ref()
                .and_then(|items| items.first())
                .map(|item| crate::history_view::relative_time(time.wall_ms, item.recorded_at_ms)),
            time.wall_ms / 60_000,
        )
    };
    app.dirty |= face(before) != face(now);
    effects
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;
    use voisu_core::{AsrModeStatus, LocalReadiness};

    pub fn at(mono_ms: u64) -> Now {
        Now {
            mono_ms,
            wall_ms: 1_000_000_000_000 + mono_ms,
        }
    }

    pub fn snapshot(state: DaemonState) -> DaemonSnapshot {
        DaemonSnapshot { state, asr: None }
    }

    pub fn snapshot_with_local(local_ready: bool) -> DaemonSnapshot {
        DaemonSnapshot {
            state: DaemonState::Idle,
            asr: Some(AsrModeStatus {
                pending: Some(AsrMode::Cloud),
                active: Some(AsrMode::Cloud),
                revision: Some(1),
                local_readiness: if local_ready {
                    LocalReadiness::Ready {
                        model_identity: None,
                    }
                } else {
                    LocalReadiness::Absent
                },
                admission_error: None,
                audio_retention: None,
                local_path_clean: None,
            }),
        }
    }

    pub fn row(label: &str, value: &str, status: ReadinessStatus) -> DoctorRow {
        DoctorRow {
            label: label.to_owned(),
            value: Some(value.to_owned()),
            status,
            action: None,
            detail: String::new(),
            fix: None,
        }
    }

    pub fn healthy_rows() -> Vec<DoctorRow> {
        (0..14)
            .map(|index| row(&format!("Check {index}"), "ok", ReadinessStatus::Pass))
            .chain([
                row("Groq key", "valid", ReadinessStatus::Pass),
                row("Deepgram key", "unreachable", ReadinessStatus::Warn),
            ])
            .collect()
    }

    pub fn settings() -> SettingsSnapshot {
        SettingsSnapshot {
            engine: AsrMode::Cloud,
            delivery: DeliveryMode::Copy,
            writing: WritingMode::Smart,
            rendering: RenderingPolicy::Adaptive,
            deepgram: true,
        }
    }

    /// A settled app: daemon up and idle, Setup done, settings and doctor loaded.
    pub fn ready_app() -> App {
        let mut app = App::new((80, 24), at(0));
        let _ = app.start();
        for reply in [
            Reply::Status(StatusPoll::Up(snapshot(DaemonState::Idle))),
            Reply::SetupNeeded(false),
            Reply::Doctor(healthy_rows()),
            Reply::Settings(settings()),
            Reply::Shortcut(Some("Not bound".to_owned())),
            Reply::History(Ok(Vec::new())),
            Reply::Dictionary(Ok(Vec::new())),
        ] {
            let _ = update(&mut app, Msg::Reply(reply), at(0));
        }
        app
    }

    pub fn press(app: &mut App, code: KeyCode, mono_ms: u64) -> Vec<Effect> {
        update(
            app,
            Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            at(mono_ms),
        )
    }

    pub fn type_text(app: &mut App, text: &str, mono_ms: u64) {
        for ch in text.chars() {
            let _ = press(app, KeyCode::Char(ch), mono_ms);
        }
    }

    pub fn item(transcript: Option<&str>, failure: Option<&str>) -> HistoryItem {
        HistoryItem {
            recorded_at_ms: 1_000_000_000_000,
            transcript: transcript.map(str::to_owned),
            failure: failure.map(str::to_owned),
            providers: "Groq 300ms".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn set_status(app: &mut App, state: DaemonState, mono_ms: u64) -> Vec<Effect> {
        update(
            app,
            Msg::Reply(Reply::Status(StatusPoll::Up(snapshot(state)))),
            at(mono_ms),
        )
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    // --- Trap 1: dictating into the interface ---------------------------------

    #[test]
    fn while_dictating_every_key_but_ctrl_c_is_ignored() {
        for state in [DaemonState::Recording, DaemonState::Processing] {
            let mut app = ready_app();
            let _ = press(&mut app, KeyCode::Char('4'), 0); // Settings
            let _ = set_status(&mut app, state, 10);
            for code in [
                KeyCode::Char('q'),
                KeyCode::Char('d'),
                KeyCode::Char('2'),
                KeyCode::Char('?'),
                KeyCode::Tab,
                KeyCode::Enter,
                KeyCode::Esc,
                KeyCode::Down,
            ] {
                let effects = press(&mut app, code, 20);
                assert!(effects.is_empty(), "{code:?} produced {effects:?}");
            }
            assert!(!app.should_quit);
            assert_eq!(app.tab, Tab::Settings);
            assert_eq!(app.settings_selected, 0);
            assert!(!app.overlay);

            let _ = update(&mut app, Msg::Key(ctrl_c()), at(30));
            assert!(app.should_quit);
        }
    }

    #[test]
    fn the_guard_outlives_the_recording_by_one_second() {
        let mut app = ready_app();
        let _ = set_status(&mut app, DaemonState::Processing, 100);
        let _ = set_status(&mut app, DaemonState::Idle, 5_000);

        let _ = press(&mut app, KeyCode::Char('q'), 5_999);
        assert!(!app.should_quit, "a dictated q landed inside the guard");

        let _ = press(&mut app, KeyCode::Char('q'), 6_000);
        assert!(app.should_quit, "keys work again one second after");
    }

    #[test]
    fn the_guard_survives_a_status_poll_that_fails_mid_recording() {
        let mut app = ready_app();
        let _ = set_status(&mut app, DaemonState::Recording, 0);
        let _ = update(
            &mut app,
            Msg::Reply(Reply::Status(StatusPoll::Failed)),
            at(500),
        );
        assert!(app.guarded());
        let _ = press(&mut app, KeyCode::Char('q'), 600);
        assert!(!app.should_quit);
    }

    #[test]
    fn a_daemon_that_dies_mid_recording_still_gets_the_trailing_guard() {
        let mut app = ready_app();
        let _ = set_status(&mut app, DaemonState::Recording, 0);
        let _ = update(
            &mut app,
            Msg::Reply(Reply::Status(StatusPoll::Unavailable)),
            at(500),
        );
        assert!(app.guarded());
        let _ = press(&mut app, KeyCode::Char('q'), 1_000);
        assert!(!app.should_quit);
        let _ = press(&mut app, KeyCode::Char('q'), 1_500);
        assert!(app.should_quit);
    }

    fn poll(app: &mut App, poll: StatusPoll, mono_ms: u64) -> Vec<Effect> {
        update(app, Msg::Reply(Reply::Status(poll)), at(mono_ms))
    }

    #[test]
    fn one_failed_poll_keeps_what_was_last_seen() {
        let mut app = ready_app();
        let _ = poll(&mut app, StatusPoll::Failed, 500);
        assert_eq!(app.daemon, DaemonView::Up(snapshot(DaemonState::Idle)));
        assert_eq!(app.hero(), Hero::Ready);
    }

    #[test]
    fn a_daemon_that_keeps_failing_to_answer_keeps_the_guard_on() {
        let mut app = ready_app();
        for mono_ms in [500, 1_000, 1_500] {
            let _ = poll(&mut app, StatusPoll::Failed, mono_ms);
        }
        assert!(app.guarded(), "it may still be dictating");
        assert!(press(&mut app, KeyCode::Enter, 1_600).is_empty());
        let _ = press(&mut app, KeyCode::Char('q'), 1_600);
        assert!(!app.should_quit, "a dictated q quit");
    }

    #[test]
    fn a_daemon_that_keeps_failing_to_answer_asks_for_a_restart() {
        let mut app = ready_app();
        for mono_ms in [500, 1_000] {
            let _ = poll(&mut app, StatusPoll::Failed, mono_ms);
        }
        assert_eq!(app.hero(), Hero::Ready, "two slow replies are not a fault");
        let _ = poll(&mut app, StatusPoll::Failed, 1_500);
        assert_eq!(app.hero(), Hero::NeedsRestart);
        // A good reply in between starts the count again.
        let _ = poll(&mut app, StatusPoll::Up(snapshot(DaemonState::Idle)), 2_000);
        for mono_ms in [2_500, 3_000] {
            let _ = poll(&mut app, StatusPoll::Failed, mono_ms);
        }
        assert_eq!(app.hero(), Hero::Ready);
    }

    #[test]
    fn a_protocol_mismatch_asks_for_a_restart_at_once_and_stays_guarded() {
        let mut app = ready_app();
        let _ = poll(&mut app, StatusPoll::Mismatch, 500);
        assert_eq!(app.hero(), Hero::NeedsRestart);
        assert!(app.guarded(), "the old daemon may still be dictating");
        assert_eq!(app.fix(), None, "a dictated newline must not be the fix");
        for code in [KeyCode::Char('q'), KeyCode::Enter, KeyCode::Char('4')] {
            assert!(press(&mut app, code, 600).is_empty(), "{code:?}");
        }
        assert!(!app.should_quit);
        assert_eq!(app.tab, Tab::Home);

        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        let effects = update(&mut app, Msg::Key(ctrl_r), at(700));
        assert!(matches!(
            &effects[..],
            [Effect::Job(Job::Service(ServiceVerb::Restart))]
        ));
        assert!(
            update(&mut app, Msg::Key(ctrl_r), at(800)).is_empty(),
            "one restart at a time"
        );
    }

    #[test]
    fn dictation_cannot_type_into_an_open_prompt() {
        let mut app = ready_app();
        let _ = press(&mut app, KeyCode::Char('3'), 0);
        let _ = press(&mut app, KeyCode::Char('a'), 0);
        let _ = set_status(&mut app, DaemonState::Processing, 10);
        type_text(&mut app, "dictated", 20);
        let _ = update(&mut app, Msg::Paste("pasted".to_owned()), at(20));
        let Some(Prompt::AddTerm(text)) = &app.prompt else {
            panic!("prompt closed");
        };
        assert_eq!(text.as_str(), "");
    }

    #[test]
    fn paste_is_discarded_everywhere_but_an_open_key_prompt() {
        let mut app = ready_app();
        let _ = press(&mut app, KeyCode::Char('3'), 0);
        let effects = update(&mut app, Msg::Paste("qqq\n4".to_owned()), at(0));
        assert!(effects.is_empty());
        assert_eq!(app.tab, Tab::Dictionary);
        assert!(!app.should_quit);

        let _ = press(&mut app, KeyCode::Char('a'), 0);
        let _ = update(&mut app, Msg::Paste("term".to_owned()), at(0));
        let Some(Prompt::AddTerm(text)) = &app.prompt else {
            panic!("prompt closed");
        };
        assert_eq!(text.as_str(), "", "a dictionary prompt discards paste");
    }

    #[test]
    fn a_key_prompt_accepts_paste_and_stops_at_a_line_break() {
        let mut app = ready_app();
        app.tab = Tab::Settings;
        app.settings_selected = 5; // Groq key
        let _ = press(&mut app, KeyCode::Enter, 0);
        let _ = update(&mut app, Msg::Paste("gsk_abc\nextra".to_owned()), at(0));
        let Some(Prompt::Key(Provider::Groq, text)) = &app.prompt else {
            panic!("no key prompt");
        };
        assert_eq!(text.as_str(), "gsk_abc");
    }

    // --- Home hero -------------------------------------------------------------

    #[test]
    fn hero_priority_runs_setup_daemon_failure_activity_ready() {
        let mut app = ready_app();
        assert_eq!(app.hero(), Hero::Ready);

        let _ = set_status(&mut app, DaemonState::Recording, 0);
        assert_eq!(app.hero(), Hero::Recording { seconds: 0 });
        let _ = set_status(&mut app, DaemonState::Processing, 0);
        assert_eq!(app.hero(), Hero::Processing);

        let mut failing = row("Microphone", "absent", ReadinessStatus::Fail);
        failing.action = Some("voisu service restart".to_owned());
        failing.fix = Some(Fix::RestartVoisu);
        let mut rows = healthy_rows();
        rows.push(failing);
        let _ = update(&mut app, Msg::Reply(Reply::Doctor(rows)), at(0));
        assert!(
            matches!(app.hero(), Hero::Failing { label, fix: Some(Fix::RestartVoisu), .. } if label == "Microphone")
        );

        let _ = update(
            &mut app,
            Msg::Reply(Reply::Status(StatusPoll::Unavailable)),
            at(0),
        );
        assert_eq!(app.hero(), Hero::NotRunning);
        assert_eq!(app.fix(), Some(Fix::StartVoisu));

        let _ = update(&mut app, Msg::Reply(Reply::SetupNeeded(true)), at(0));
        assert_eq!(app.hero(), Hero::NotSetUp);
        assert_eq!(app.fix(), Some(Fix::RunSetup));

        let _ = poll(&mut app, StatusPoll::Mismatch, 0);
        assert_eq!(
            app.hero(),
            Hero::NeedsRestart,
            "it outranks every other problem"
        );
    }

    fn deepgram_turned_off(app: &mut App) {
        app.tab = Tab::Settings;
        app.settings_selected = 4;
        let _ = press(app, KeyCode::Enter, 0);
        let _ = update(
            app,
            Msg::Reply(Reply::Applied {
                kind: Tone::Success,
                text: "Deepgram turned off. Restart Voisu to apply.".to_owned(),
                refresh: &[Refresh::PendingRestart(SettingsRow::Deepgram)],
            }),
            at(0),
        );
    }

    #[test]
    fn a_setting_read_at_start_stays_marked_until_a_restart() {
        let mut app = ready_app();
        deepgram_turned_off(&mut app);
        // Long after the toast has gone.
        let _ = update(&mut app, Msg::Tick, at(60_000));
        assert_eq!(
            app.setting_value(SettingsRow::Deepgram),
            ("Off · after restart".to_owned(), Tone::Caution)
        );
        assert!(matches!(
            app.context(),
            Context::Settings { restart: true, .. }
        ));
        let effects = press(&mut app, KeyCode::Char('r'), 60_000);
        assert!(matches!(
            &effects[..],
            [Effect::Job(Job::Service(ServiceVerb::Restart))]
        ));
        let _ = update(
            &mut app,
            Msg::Reply(Reply::ServiceDone {
                kind: Tone::Success,
                text: "Voisu restarted.".to_owned(),
            }),
            at(61_000),
        );
        assert!(app.restart_pending.is_empty());
        assert_eq!(app.setting_value(SettingsRow::Deepgram).0, "Off");
    }

    #[test]
    fn a_failed_restart_keeps_the_mark() {
        let mut app = ready_app();
        deepgram_turned_off(&mut app);
        let _ = update(
            &mut app,
            Msg::Reply(Reply::ServiceDone {
                kind: Tone::Failure,
                text: "Voisu didn't restart.".to_owned(),
            }),
            at(0),
        );
        assert_eq!(app.restart_pending, [SettingsRow::Deepgram]);
    }

    fn service_done(app: &mut App, text: &str) {
        let _ = update(
            app,
            Msg::Reply(Reply::ServiceDone {
                kind: Tone::Success,
                text: text.to_owned(),
            }),
            at(0),
        );
    }

    #[test]
    fn a_start_that_found_voisu_running_keeps_the_mark() {
        let mut app = ready_app();
        deepgram_turned_off(&mut app);
        app.tab = Tab::Home;
        let mut failing = row("Overlay", "stopped", ReadinessStatus::Fail);
        failing.fix = Some(Fix::StartVoisu);
        let _ = update(&mut app, Msg::Reply(Reply::Doctor(vec![failing])), at(0));
        let effects = press(&mut app, KeyCode::Enter, 0);
        assert!(matches!(
            &effects[..],
            [Effect::Job(Job::Service(ServiceVerb::Start))]
        ));
        service_done(&mut app, "Voisu started.");
        assert_eq!(
            app.restart_pending,
            [SettingsRow::Deepgram],
            "the running daemon still holds the old value"
        );
    }

    #[test]
    fn a_start_of_a_stopped_voisu_clears_the_mark() {
        let mut app = ready_app();
        deepgram_turned_off(&mut app);
        app.tab = Tab::Home;
        let _ = poll(&mut app, StatusPoll::Unavailable, 0);
        let effects = press(&mut app, KeyCode::Enter, 0);
        assert!(matches!(
            &effects[..],
            [Effect::Job(Job::Service(ServiceVerb::Start))]
        ));
        service_done(&mut app, "Voisu started.");
        assert!(app.restart_pending.is_empty());
    }

    #[test]
    fn a_daemon_seen_going_down_and_back_clears_the_mark() {
        let mut app = ready_app();
        deepgram_turned_off(&mut app);
        for mono_ms in [100, 200, 300] {
            let _ = poll(&mut app, StatusPoll::Failed, mono_ms);
        }
        let _ = poll(&mut app, StatusPoll::Up(snapshot(DaemonState::Idle)), 400);
        assert!(!app.restart_pending.is_empty(), "slow is not restarted");
        let _ = poll(&mut app, StatusPoll::Unavailable, 500);
        assert!(!app.restart_pending.is_empty());
        let _ = poll(&mut app, StatusPoll::Up(snapshot(DaemonState::Idle)), 1_000);
        assert!(app.restart_pending.is_empty());
    }

    #[test]
    fn home_offers_the_pending_restart_below_real_problems_and_recordings() {
        let mut app = ready_app();
        deepgram_turned_off(&mut app);
        app.tab = Tab::Home;
        assert_eq!(app.hero(), Hero::Ready);
        assert_eq!(app.fix(), Some(Fix::RestartVoisu));

        let _ = set_status(&mut app, DaemonState::Recording, 0);
        assert_eq!(app.fix(), None);
        assert!(
            press(&mut app, KeyCode::Char('r'), 0).is_empty(),
            "normal guard rules apply"
        );
        let _ = set_status(&mut app, DaemonState::Idle, 0);

        let mut failing = row("Microphone", "absent", ReadinessStatus::Fail);
        failing.fix = Some(Fix::RunSetup);
        let _ = update(&mut app, Msg::Reply(Reply::Doctor(vec![failing])), at(0));
        assert_eq!(app.fix(), Some(Fix::RunSetup), "a real failure outranks it");

        let _ = update(&mut app, Msg::Reply(Reply::Doctor(healthy_rows())), at(0));
        let effects = press(&mut app, KeyCode::Enter, 5_000);
        assert!(matches!(
            &effects[..],
            [Effect::Job(Job::Service(ServiceVerb::Restart))]
        ));
    }

    #[test]
    fn a_warning_does_not_take_over_home() {
        let app = ready_app();
        assert_eq!(app.hero(), Hero::Ready);
        let (health, tone) = app.health().expect("doctor loaded");
        assert_eq!(health, "15 passed · 1 warning");
        assert_eq!(tone, Tone::Caution);
    }

    #[test]
    fn health_reads_all_passed_and_ignores_checks_that_do_not_apply() {
        let mut app = ready_app();
        let mut rows: Vec<_> = (0..16)
            .map(|index| row(&format!("C{index}"), "ok", ReadinessStatus::Pass))
            .collect();
        rows.push(row("Skipped", "n/a", ReadinessStatus::Skip));
        let _ = update(&mut app, Msg::Reply(Reply::Doctor(rows)), at(0));
        assert_eq!(app.health().expect("loaded").0, "All 16 checks passed");
    }

    #[test]
    fn hero_waits_while_nothing_is_known_yet() {
        let app = App::new((80, 24), at(0));
        assert_eq!(app.hero(), Hero::Checking);
    }

    #[test]
    fn enter_on_home_performs_the_one_fix() {
        let mut app = ready_app();
        let _ = update(
            &mut app,
            Msg::Reply(Reply::Status(StatusPoll::Unavailable)),
            at(0),
        );
        let effects = press(&mut app, KeyCode::Enter, 0);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Job(Job::Service(ServiceVerb::Start))]
        ));
        assert_eq!(app.service_busy, Some(ServiceVerb::Start));
        assert!(
            press(&mut app, KeyCode::Enter, 0).is_empty(),
            "one at a time"
        );

        let _ = update(&mut app, Msg::Reply(Reply::SetupNeeded(true)), at(0));
        let _ = update(
            &mut app,
            Msg::Reply(Reply::ServiceDone {
                kind: Tone::Success,
                text: "Voisu started.".to_owned(),
            }),
            at(0),
        );
        let effects = press(&mut app, KeyCode::Enter, 0);
        assert!(matches!(effects.as_slice(), [Effect::RunSetup]));
    }

    #[test]
    fn enter_on_a_healthy_home_does_nothing() {
        let mut app = ready_app();
        assert!(press(&mut app, KeyCode::Enter, 0).is_empty());
    }

    #[test]
    fn a_failure_with_no_typed_fix_offers_no_key_however_it_reads() {
        let mut app = ready_app();
        let mut failing = row("Groq key", "invalid", ReadinessStatus::Fail);
        failing.action = Some("voisu service restart".to_owned());
        let _ = update(&mut app, Msg::Reply(Reply::Doctor(vec![failing])), at(0));
        assert!(matches!(app.hero(), Hero::Failing { fix: None, .. }));
        assert_eq!(app.fix(), None);
    }

    #[test]
    fn check_lines_order_failures_first_with_their_action_beneath() {
        let mut app = ready_app();
        let mut failing = row("Socket", "missing", ReadinessStatus::Fail);
        failing.action = Some("voisu service start".to_owned());
        let rows = vec![
            row("Pass me", "ok", ReadinessStatus::Pass),
            row("Warn me", "hm", ReadinessStatus::Warn),
            failing,
            row("Skip me", "n/a", ReadinessStatus::Skip),
        ];
        let _ = update(&mut app, Msg::Reply(Reply::Doctor(rows)), at(0));
        let labels: Vec<String> = app
            .check_lines()
            .iter()
            .map(|line| match line {
                CheckLine::Row { label, .. } => label.clone(),
                CheckLine::Action(action) => format!("-> {action}"),
            })
            .collect();
        assert_eq!(
            labels,
            ["Socket", "-> voisu service start", "Warn me", "Pass me"]
        );
    }

    // --- Doctor coalescing ------------------------------------------------------

    #[test]
    fn doctor_requests_coalesce_while_one_runs() {
        let mut app = App::new((80, 24), at(0));
        let first = app.start();
        assert_eq!(
            first
                .iter()
                .filter(|effect| matches!(effect, Effect::Job(Job::Doctor)))
                .count(),
            1
        );
        // Two more reasons to re-run arrive while the first is out.
        let _ = press(&mut app, KeyCode::Char('d'), 0);
        let _ = press(&mut app, KeyCode::Char('d'), 0);
        let _ = press(&mut app, KeyCode::Char('d'), 0);
        let effects = update(&mut app, Msg::Reply(Reply::Doctor(Vec::new())), at(0));
        assert!(matches!(effects.as_slice(), [Effect::Job(Job::Doctor)]));
        let effects = update(&mut app, Msg::Reply(Reply::Doctor(Vec::new())), at(0));
        assert!(effects.is_empty());
    }

    #[test]
    fn the_first_poll_does_not_repeat_the_startup_doctor_run() {
        let mut app = App::new((80, 24), at(0));
        let _ = app.start();
        let _ = update(&mut app, Msg::Reply(Reply::Doctor(Vec::new())), at(0));
        let effects = update(
            &mut app,
            Msg::Reply(Reply::Status(StatusPoll::Up(snapshot(DaemonState::Idle)))),
            at(0),
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::Job(Job::Doctor)))
        );
    }

    #[test]
    fn losing_and_regaining_the_daemon_reruns_the_doctor() {
        let mut app = ready_app();
        let effects = update(
            &mut app,
            Msg::Reply(Reply::Status(StatusPoll::Unavailable)),
            at(0),
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Job(Job::Doctor)))
        );
    }

    // --- Redraw only on change --------------------------------------------------

    #[test]
    fn an_unchanged_status_poll_does_not_redraw() {
        let mut app = ready_app();
        let _ = update(&mut app, Msg::Tick, at(0));
        assert!(app.take_dirty());
        for mono_ms in [500, 1_000, 1_500, 2_000] {
            let _ = update(
                &mut app,
                Msg::Reply(Reply::Status(StatusPoll::Up(snapshot(DaemonState::Idle)))),
                at(mono_ms),
            );
            assert!(!app.take_dirty(), "redraw at {mono_ms}");
        }
    }

    #[test]
    fn idle_ticks_do_not_redraw_until_the_minute_turns() {
        let mut app = ready_app();
        let _ = update(&mut app, Msg::Tick, at(0));
        assert!(app.take_dirty());
        // The wall clock in `at` turns a minute once within this span, and that
        // is the only redraw an idle screen with no history owes.
        let mut redraws = 0;
        for mono_ms in (100..60_000).step_by(100) {
            let _ = update(&mut app, Msg::Tick, at(mono_ms));
            redraws += u32::from(app.take_dirty());
        }
        assert!(redraws <= 1, "{redraws} redraws in an idle minute");
    }

    #[test]
    fn a_recording_redraws_once_a_second_for_its_timer() {
        let mut app = ready_app();
        let _ = set_status(&mut app, DaemonState::Recording, 0);
        let _ = app.take_dirty();
        let mut redraws = 0;
        for mono_ms in (100..=2_000).step_by(100) {
            let _ = update(&mut app, Msg::Tick, at(mono_ms));
            redraws += u32::from(app.take_dirty());
        }
        assert_eq!(redraws, 2);
        assert_eq!(app.hero(), Hero::Recording { seconds: 2 });
    }

    #[test]
    fn toasts_expire_and_their_expiry_redraws() {
        let mut app = ready_app();
        let _ = update(
            &mut app,
            Msg::Reply(Reply::Applied {
                kind: Tone::Success,
                text: "Done.".to_owned(),
                refresh: &[],
            }),
            at(0),
        );
        assert!(app.toast.is_some());
        let _ = app.take_dirty();
        let _ = update(&mut app, Msg::Tick, at(TOAST_MS - 1));
        assert!(app.toast.is_some());
        assert!(!app.take_dirty());
        let _ = update(&mut app, Msg::Tick, at(TOAST_MS));
        assert!(app.toast.is_none());
        assert!(app.take_dirty());
    }

    // --- Navigation -------------------------------------------------------------

    #[test]
    fn every_tab_key_reaches_its_tab() {
        let mut app = ready_app();
        let _ = press(&mut app, KeyCode::Right, 0);
        assert_eq!(app.tab, Tab::History);
        let _ = press(&mut app, KeyCode::Tab, 0);
        assert_eq!(app.tab, Tab::Dictionary);
        let _ = press(&mut app, KeyCode::Char('l'), 0);
        assert_eq!(app.tab, Tab::Settings);
        let _ = press(&mut app, KeyCode::Right, 0);
        assert_eq!(app.tab, Tab::Home, "wraps");
        let _ = press(&mut app, KeyCode::Left, 0);
        assert_eq!(app.tab, Tab::Settings, "wraps back");
        let _ = press(&mut app, KeyCode::Char('h'), 0);
        assert_eq!(app.tab, Tab::Dictionary);
        let _ = press(&mut app, KeyCode::BackTab, 0);
        assert_eq!(app.tab, Tab::History);
        let _ = press(&mut app, KeyCode::Char('1'), 0);
        assert_eq!(app.tab, Tab::Home);
        let _ = press(&mut app, KeyCode::Char('4'), 0);
        assert_eq!(app.tab, Tab::Settings);
    }

    #[test]
    fn escape_closes_things_but_never_quits() {
        let mut app = ready_app();
        let _ = press(&mut app, KeyCode::Esc, 0);
        assert!(!app.should_quit);
        let _ = press(&mut app, KeyCode::Char('?'), 0);
        assert!(app.overlay);
        let _ = press(&mut app, KeyCode::Esc, 0);
        assert!(!app.overlay);
        assert!(!app.should_quit);
    }

    #[test]
    fn q_quits_from_a_tab_but_types_inside_a_prompt() {
        let mut app = ready_app();
        let _ = press(&mut app, KeyCode::Char('3'), 0);
        let _ = press(&mut app, KeyCode::Char('a'), 0);
        let _ = press(&mut app, KeyCode::Char('q'), 0);
        assert!(!app.should_quit);
        let _ = press(&mut app, KeyCode::Esc, 0);
        let _ = press(&mut app, KeyCode::Char('q'), 0);
        assert!(app.should_quit);
    }

    #[test]
    fn a_too_small_screen_only_answers_to_quit() {
        let mut app = ready_app();
        let _ = update(&mut app, Msg::Resize(52, 14), at(0));
        let _ = press(&mut app, KeyCode::Char('2'), 0);
        let _ = press(&mut app, KeyCode::Char('?'), 0);
        assert_eq!(app.tab, Tab::Home);
        assert!(!app.overlay);
        let _ = press(&mut app, KeyCode::Char('q'), 0);
        assert!(app.should_quit);
    }

    // --- Settings ----------------------------------------------------------------

    #[test]
    fn enter_opens_the_choices_and_only_a_second_enter_changes_anything() {
        let mut app = ready_app();
        app.tab = Tab::Settings;
        app.settings_selected = 1;
        app.settings.as_mut().expect("loaded").delivery = DeliveryMode::Paste;
        let current = app.current_choice(SettingsRow::Delivery).expect("loaded");

        assert!(press(&mut app, KeyCode::Enter, 0).is_empty());
        assert_eq!(app.context(), Context::ChoicePrompt);
        assert!(press(&mut app, KeyCode::Esc, 0).is_empty());
        assert!(app.prompt.is_none(), "Esc closes the choice unchanged");

        let _ = press(&mut app, KeyCode::Enter, 0);
        assert!(
            press(&mut app, KeyCode::Enter, 0).is_empty(),
            "the value in effect is no change"
        );
        assert!(app.prompt.is_none());

        let _ = press(&mut app, KeyCode::Enter, 0);
        let _ = press(&mut app, KeyCode::Down, 0);
        let effects = press(&mut app, KeyCode::Enter, 0);
        let expected = DELIVERIES[current + 1];
        assert!(
            matches!(effects.as_slice(), [Effect::Job(Job::SetDelivery(mode))] if *mode == expected)
        );
        assert_eq!(app.current_choice(SettingsRow::Delivery), Some(current + 1));
        assert!(app.prompt.is_none());
    }

    #[test]
    fn the_picker_stops_at_both_ends() {
        let mut app = ready_app();
        app.tab = Tab::Settings;
        app.settings_selected = 3;
        let _ = press(&mut app, KeyCode::Enter, 0);
        let picked = |app: &App| match app.prompt {
            Some(Prompt::Choice { picked, .. }) => picked,
            _ => panic!("the choice closed"),
        };
        for _ in 0..5 {
            let _ = press(&mut app, KeyCode::Up, 0);
        }
        assert_eq!(picked(&app), 0);
        for _ in 0..5 {
            let _ = press(&mut app, KeyCode::Down, 0);
        }
        assert_eq!(picked(&app), LAYOUTS.len() - 1);
    }

    #[test]
    fn writing_and_layout_send_their_own_jobs_and_deepgram_still_flips() {
        let mut app = ready_app();
        app.tab = Tab::Settings;
        let pick_next = |app: &mut App, row: usize| {
            app.settings_selected = row;
            let _ = press(app, KeyCode::Enter, 0);
            let _ = press(app, KeyCode::Down, 0);
            press(app, KeyCode::Enter, 0)
        };

        let next = app.current_choice(SettingsRow::Writing).expect("loaded") + 1;
        let effects = pick_next(&mut app, 2);
        assert!(
            matches!(effects.as_slice(), [Effect::Job(Job::SetWriting(mode))] if *mode == WRITINGS[next])
        );

        let next = app.current_choice(SettingsRow::Rendering).expect("loaded") + 1;
        let effects = pick_next(&mut app, 3);
        assert!(
            matches!(effects.as_slice(), [Effect::Job(Job::SetRendering(policy))] if *policy == LAYOUTS[next])
        );

        app.settings_selected = 4;
        let effects = press(&mut app, KeyCode::Enter, 0);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Job(Job::SetDeepgram(false))]
        ));
        assert_eq!(app.setting_value(SettingsRow::Deepgram).0, "Off");
    }

    #[test]
    fn the_trigger_key_row_is_read_only() {
        let mut app = ready_app();
        app.tab = Tab::Settings;
        app.settings_selected = 7;
        assert!(press(&mut app, KeyCode::Enter, 0).is_empty());
        assert!(app.prompt.is_none());
        assert_eq!(
            app.context(),
            Context::Settings {
                enter: None,
                restart: true
            }
        );
    }

    #[test]
    fn the_engine_row_passes_the_daemon_readiness_to_the_worker() {
        for ready in [false, true] {
            let mut app = ready_app();
            let _ = update(
                &mut app,
                Msg::Reply(Reply::Status(StatusPoll::Up(snapshot_with_local(ready)))),
                at(0),
            );
            app.tab = Tab::Settings;
            let _ = press(&mut app, KeyCode::Enter, 0);
            let _ = press(&mut app, KeyCode::Down, 0);
            let effects = press(&mut app, KeyCode::Enter, 0);
            assert!(matches!(
                effects.as_slice(),
                [Effect::Job(Job::SetEngine { mode: AsrMode::Local, local_ready })] if *local_ready == ready
            ));
            // The Engine row must not move before the worker has decided.
            assert_eq!(app.setting_value(SettingsRow::Engine).0, "Cloud");
        }
    }

    #[test]
    fn local_needs_readiness_or_a_consented_install() {
        assert!(!local_switch_allowed(false, false));
        assert!(local_switch_allowed(true, false));
        assert!(local_switch_allowed(false, true));
    }

    #[test]
    fn run_setup_row_hands_off_to_the_wizard() {
        let mut app = ready_app();
        app.tab = Tab::Settings;
        app.settings_selected = 8;
        let effects = press(&mut app, KeyCode::Enter, 0);
        assert!(matches!(effects.as_slice(), [Effect::RunSetup]));
    }

    #[test]
    fn trigger_key_value_reads_the_daemon_answer() {
        assert_eq!(trigger_key_value("Trigger Key: Ctrl+Space"), "Ctrl+Space");
        assert_eq!(
            trigger_key_value("No Trigger Key is bound; start, stop, and toggle remain available"),
            "Not bound"
        );
    }

    // --- Trap 4: secrets ---------------------------------------------------------

    #[test]
    fn a_key_goes_straight_into_a_credential_and_no_debug_shows_it() {
        const SECRET: &str = "gsk_very_secret_value";
        let mut app = ready_app();
        app.tab = Tab::Settings;
        app.settings_selected = 5;
        let _ = press(&mut app, KeyCode::Enter, 0);
        type_text(&mut app, SECRET, 0);
        assert!(!format!("{:?}", app.prompt).contains(SECRET));

        let effects = press(&mut app, KeyCode::Enter, 0);
        assert!(app.prompt.is_none(), "the buffer is gone");
        assert_eq!(app.key_in_check, Some(Provider::Groq));
        assert!(!format!("{effects:?}").contains(SECRET));
        let [Effect::Job(Job::SaveKey { credential, .. })] = effects.as_slice() else {
            panic!("expected a save");
        };
        assert_eq!(credential.expose_to_boundary(), SECRET);
    }

    #[test]
    fn an_empty_key_stays_in_the_prompt() {
        let mut app = ready_app();
        app.tab = Tab::Settings;
        app.settings_selected = 6;
        let _ = press(&mut app, KeyCode::Enter, 0);
        assert!(press(&mut app, KeyCode::Enter, 0).is_empty());
        assert!(matches!(
            app.prompt,
            Some(Prompt::Key(Provider::Deepgram, _))
        ));
    }

    #[test]
    fn a_finished_key_check_reruns_the_doctor_for_the_new_verdict() {
        let mut app = ready_app();
        app.key_in_check = Some(Provider::Groq);
        assert_eq!(app.setting_value(SettingsRow::GroqKey).0, "Checking…");
        let effects = update(
            &mut app,
            Msg::Reply(Reply::KeyChecked {
                kind: Tone::Success,
                text: "Groq key saved.".to_owned(),
            }),
            at(0),
        );
        assert!(app.key_in_check.is_none());
        assert!(matches!(effects.as_slice(), [Effect::Job(Job::Doctor)]));
    }

    // --- Dictionary --------------------------------------------------------------

    #[test]
    fn adding_and_removing_a_term_go_through_the_prompt() {
        let mut app = ready_app();
        let _ = update(
            &mut app,
            Msg::Reply(Reply::Dictionary(Ok(vec![
                "Voisu".to_owned(),
                "Hyprland".to_owned(),
            ]))),
            at(0),
        );
        let _ = press(&mut app, KeyCode::Char('3'), 0);

        let _ = press(&mut app, KeyCode::Char('a'), 0);
        type_text(&mut app, "  ratatuii", 0);
        let _ = press(&mut app, KeyCode::Backspace, 0);
        let effects = press(&mut app, KeyCode::Enter, 0);
        assert!(
            matches!(effects.as_slice(), [Effect::Job(Job::AddTerm(term))] if term == "ratatui")
        );

        let _ = press(&mut app, KeyCode::Down, 0);
        let _ = press(&mut app, KeyCode::Char('d'), 0);
        assert!(matches!(&app.prompt, Some(Prompt::ConfirmRemove(term)) if term == "Hyprland"));
        assert!(press(&mut app, KeyCode::Char('n'), 0).is_empty());
        assert!(app.prompt.is_none(), "n keeps the term");

        let _ = press(&mut app, KeyCode::Char('d'), 0);
        let effects = press(&mut app, KeyCode::Char('y'), 0);
        assert!(
            matches!(effects.as_slice(), [Effect::Job(Job::RemoveTerm(term))] if term == "Hyprland")
        );
    }

    #[test]
    fn a_blank_term_is_not_added() {
        let mut app = ready_app();
        let _ = press(&mut app, KeyCode::Char('3'), 0);
        let _ = press(&mut app, KeyCode::Char('a'), 0);
        type_text(&mut app, "   ", 0);
        assert!(press(&mut app, KeyCode::Enter, 0).is_empty());
        assert!(app.prompt.is_none());
    }

    // --- History -----------------------------------------------------------------

    #[test]
    fn copy_sends_the_selected_transcript_or_says_why_it_cannot() {
        let mut app = ready_app();
        let _ = update(
            &mut app,
            Msg::Reply(Reply::History(Ok(vec![
                item(Some("first"), None),
                item(None, Some("No speech")),
            ]))),
            at(0),
        );
        let _ = press(&mut app, KeyCode::Char('2'), 0);
        let effects = press(&mut app, KeyCode::Char('c'), 0);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Job(Job::Copy(text))] if text == "first"
        ));

        let _ = press(&mut app, KeyCode::Down, 0);
        let effects = press(&mut app, KeyCode::Char('c'), 0);
        assert!(effects.is_empty());
        assert_eq!(app.toast.as_ref().map(|t| t.kind), Some(Tone::Caution));
    }

    #[test]
    fn a_long_transcript_can_be_read_to_its_end_and_scroll_resets_on_move() {
        let mut app = ready_app();
        let long = (0..200)
            .map(|n| format!("word{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let _ = update(
            &mut app,
            Msg::Reply(Reply::History(Ok(vec![
                item(Some(&long), None),
                item(Some("short"), None),
            ]))),
            at(0),
        );
        let _ = press(&mut app, KeyCode::Char('2'), 0);
        for _ in 0..50 {
            let _ = press(&mut app, KeyCode::PageDown, 0);
        }
        let (_, pane) = app.history_split();
        let lines = app.detail_lines(app.detail_width());
        assert_eq!(app.detail_scroll + pane, lines.len(), "stops at the end");
        let _ = press(&mut app, KeyCode::Down, 0);
        assert_eq!(app.detail_scroll, 0);
        let _ = press(&mut app, KeyCode::Up, 0);
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn a_shorter_history_cannot_leave_the_selection_out_of_range() {
        let mut app = ready_app();
        let _ = update(
            &mut app,
            Msg::Reply(Reply::History(Ok(vec![
                item(Some("a"), None),
                item(Some("b"), None),
            ]))),
            at(0),
        );
        app.history_selected = 1;
        let _ = update(
            &mut app,
            Msg::Reply(Reply::History(Ok(vec![item(Some("a"), None)]))),
            at(0),
        );
        assert_eq!(app.history_selected, 0);
    }

    #[test]
    fn a_recording_ending_reloads_history() {
        let mut app = ready_app();
        let _ = set_status(&mut app, DaemonState::Processing, 0);
        let effects = set_status(&mut app, DaemonState::Idle, 500);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Job(Job::LoadHistory)]
        ));
    }
}
