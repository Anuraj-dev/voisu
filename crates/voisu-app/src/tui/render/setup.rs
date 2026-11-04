//! The setup screens: one header line with the step count, no tabs. Hierarchy
//! comes from spacing, bold and dim, as everywhere else. The footer is the same
//! key table.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use voisu_core::Provider;

use super::super::look::{Look, Tone};
use super::super::model::{App, SettingsRow};
use super::super::setup::{
    Choose, Installing, KeyField, KeyState, KeysScreen, STEPS, Screen, SetupState, Stage, Summary,
    gb,
};
use super::super::text;
use super::{Pieces, SELECTED, UNSELECTED, footer_line, single, toast_line};

const INDENT: usize = UNSELECTED.len();
/// "Deepgram key" with a little air.
const KEY_LABEL_WIDTH: usize = 15;
/// The install's stage names, up to where the bar starts.
const STAGE_WIDTH: usize = 15;
const SUMMARY_LABEL_WIDTH: usize = 16;
const BAR_MIN: usize = 6;
const BAR_MAX: usize = 30;
const CHECK_MARK: &str = "✓ ";
const CAUTION_MARK: &str = "! ";

pub(super) fn draw(frame: &mut Frame<'_>, app: &App, setup: &SetupState, look: &Look) {
    let area = frame.area();
    let width = area.width as usize;
    let gap = u16::from(app.roomy());
    let header = Rect::new(area.x, area.y, area.width, 1);
    let body = Rect::new(
        area.x,
        area.y + 1 + gap,
        area.width,
        area.height.saturating_sub(3 + gap),
    );
    let notice = Rect::new(area.x, body.y + body.height, area.width, 1);
    let footer = Rect::new(area.x, notice.y + 1, area.width, 1);

    frame.render_widget(Paragraph::new(header_line(setup, look, width)), header);
    let (lines, cursor) = body_lines(setup, look, width, app.now.mono_ms);
    frame.render_widget(Paragraph::new(lines), body);
    frame.render_widget(Paragraph::new(notice_line(app, setup, look, width)), notice);
    frame.render_widget(
        Paragraph::new(footer_line(look, &app.context(), width)),
        footer,
    );
    if let Some((column, row)) = cursor {
        frame.set_cursor_position(Position::new(
            column.min(area.width.saturating_sub(1)),
            body.y + row,
        ));
    }
}

fn header_line(setup: &SetupState, look: &Look, width: usize) -> Line<'static> {
    const NAME: &str = " voisu";
    const TITLE: &str = "   Setup";
    let step = format!("step {} of {STEPS}", setup.step);
    let gap = width
        .saturating_sub(text::width(NAME) + text::width(TITLE) + text::width(&step) + 1)
        .max(1);
    Pieces::new(look)
        .add(NAME, Tone::Accent)
        .add(TITLE, Tone::Bold)
        .add(&" ".repeat(gap), Tone::Plain)
        .add(&step, Tone::Dim)
        .line()
}

/// What `↵` refused to do on the keys screen, or else the latest toast.
fn notice_line(app: &App, setup: &SetupState, look: &Look, width: usize) -> Line<'static> {
    if let Screen::Keys(KeysScreen {
        notice: Some(notice),
        ..
    }) = &setup.screen
    {
        return single(
            look,
            &text::fit(&format!("{UNSELECTED}{notice}"), width),
            Tone::Caution,
        );
    }
    toast_line(app, look, width).0
}

fn body_lines(
    setup: &SetupState,
    look: &Look,
    width: usize,
    now_ms: u64,
) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    match &setup.screen {
        Screen::Busy { title, line } => (titled(look, title, &[line.as_str()], width), None),
        Screen::Leaving => (titled(look, "", &["One moment…"], width), None),
        Screen::Choose(choose) => (choose_lines(choose, look, width), None),
        Screen::Keys(keys) => key_lines(keys, look, width),
        Screen::Installing(installing) => (install_lines(installing, look, width, now_ms), None),
        Screen::Waiting { title } => (
            titled(
                look,
                title,
                &[
                    "Your desktop will open a small window.",
                    "Press the key you want there, then save it.",
                    "",
                    "Waiting for your desktop…",
                ],
                width,
            ),
            None,
        ),
        Screen::Done(summary) => (done_lines(summary, look, width), None),
        Screen::Failed(message) => (
            titled(look, "Setup didn't finish", &[message.as_str()], width),
            None,
        ),
    }
}

/// `content`, indented and wrapped to the screen.
fn wrapped(look: &Look, content: &str, width: usize, tone: Tone) -> Vec<Line<'static>> {
    if content.is_empty() {
        return vec![Line::default()];
    }
    text::wrap(content, width.saturating_sub(INDENT + 1).max(1))
        .into_iter()
        .map(|line| single(look, &format!("{UNSELECTED}{line}"), tone))
        .collect()
}

fn title_lines(look: &Look, title: &str) -> Vec<Line<'static>> {
    vec![
        single(look, &format!("{UNSELECTED}{title}"), Tone::Bold),
        Line::default(),
    ]
}

fn titled(look: &Look, title: &str, body: &[&str], width: usize) -> Vec<Line<'static>> {
    let mut lines = if title.is_empty() {
        Vec::new()
    } else {
        title_lines(look, title)
    };
    for line in body {
        lines.extend(wrapped(look, line, width, Tone::Dim));
    }
    lines
}

fn choose_lines(choose: &Choose, look: &Look, width: usize) -> Vec<Line<'static>> {
    let mut lines = title_lines(look, &choose.title);
    for line in &choose.body {
        lines.extend(wrapped(look, line, width, Tone::Dim));
    }
    if !choose.body.is_empty() {
        lines.push(Line::default());
    }
    let label_width = choose
        .rows
        .iter()
        .map(|row| text::width(&row.label))
        .max()
        .unwrap_or(0)
        + 3;
    let hang = INDENT + label_width;
    let room = width.saturating_sub(hang + 1).max(1);
    for (index, row) in choose.rows.iter().enumerate() {
        let picked = index == choose.picked;
        let label = format!("{:<label_width$}", row.label);
        let explanation = text::wrap(&row.line, room);
        lines.push(
            Pieces::new(look)
                .add(
                    if picked { SELECTED } else { UNSELECTED },
                    if picked { Tone::Accent } else { Tone::Plain },
                )
                .add(&label, if picked { Tone::Bold } else { Tone::Plain })
                .add(explanation.first().map_or("", String::as_str), Tone::Dim)
                .line(),
        );
        for more in explanation.iter().skip(1) {
            lines.push(
                Pieces::new(look)
                    .add(&" ".repeat(hang), Tone::Plain)
                    .add(more, Tone::Dim)
                    .line(),
            );
        }
    }
    if let Some(note) = &choose.note {
        lines.push(Line::default());
        lines.extend(wrapped(look, note, width, Tone::Dim));
    }
    lines
}

fn key_label(provider: Provider) -> &'static str {
    match provider {
        Provider::Groq => SettingsRow::GroqKey.label(),
        Provider::Deepgram => SettingsRow::DeepgramKey.label(),
    }
}

fn free_key_hint(provider: Provider) -> &'static str {
    match provider {
        Provider::Groq => "Free key: console.groq.com/keys",
        Provider::Deepgram => "Free key: console.deepgram.com",
    }
}

fn key_lines(
    keys: &KeysScreen,
    look: &Look,
    width: usize,
) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let mut lines = title_lines(look, "Paste your keys");
    lines.extend(wrapped(
        look,
        "Voisu sends your voice to these services to turn it into text.",
        width,
        Tone::Dim,
    ));
    lines.push(Line::default());
    let mut cursor = None;
    for (index, field) in keys.fields.iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        let selected = index == keys.selected;
        let (line, column) = key_row(field, selected, look, width);
        if selected {
            cursor = column.map(|column| (column, lines.len() as u16));
        }
        lines.push(line);
        lines.extend(key_under_row(field, look, width));
    }
    (lines, cursor)
}

/// One key: its label, then dots or how it stands, then a word at the right edge.
fn key_row(
    field: &KeyField,
    selected: bool,
    look: &Look,
    width: usize,
) -> (Line<'static>, Option<u16>) {
    let word = field.word.as_deref().unwrap_or("Checking…");
    let (right, right_tone): (String, Tone) = match &field.state {
        KeyState::Checking => ("Checking…".to_owned(), Tone::Dim),
        KeyState::Works => ("✓ Works".to_owned(), Tone::Success),
        KeyState::Unverified => ("Saved, not checked yet".to_owned(), Tone::Caution),
        KeyState::Rejected => ("✕ Rejected".to_owned(), Tone::Failure),
        KeyState::Transient(transient) => (format!("! {transient}"), Tone::Caution),
        KeyState::Failed(_) => ("✕ Not saved".to_owned(), Tone::Failure),
        KeyState::Saved if selected => ("↵ replace   → keep".to_owned(), Tone::Dim),
        _ => (String::new(), Tone::Plain),
    };
    let start = INDENT + KEY_LABEL_WIDTH;
    let available = width.saturating_sub(start + 1);
    let measure = |words: &str| text::width(&look.text(words));
    let saved_middle = match &field.state {
        KeyState::Saved => Some(format!("Saved · {word}")),
        KeyState::Outside => Some(format!("Set outside Voisu · {word}")),
        _ => None,
    };
    // The keep-or-replace hint gives way first: the footer says the same.
    let right = match &saved_middle {
        Some(middle) if measure(middle) + 2 + measure(&right) > available => String::new(),
        _ => right,
    };
    let right_width = measure(&right);
    let room = available.saturating_sub(right_width + usize::from(right_width > 0) * 2);

    let (middle, middle_tone) = match (&saved_middle, &field.state) {
        (Some(middle), _) => (middle.clone(), Tone::Plain),
        (None, KeyState::Blank | KeyState::Replacing)
            if field.shown_dots() == 0 && field.provider == Provider::Deepgram =>
        {
            (
                "(optional — better with technical words)".to_owned(),
                Tone::Dim,
            )
        }
        _ => ("•".repeat(field.shown_dots().min(room)), Tone::Plain),
    };
    let shown = fit_drawn(look, &middle, room);
    let pad = available.saturating_sub(measure(&shown) + right_width);
    let line = Pieces::new(look)
        .add(
            if selected { SELECTED } else { UNSELECTED },
            if selected { Tone::Accent } else { Tone::Plain },
        )
        .add(
            &format!("{:<KEY_LABEL_WIDTH$}", key_label(field.provider)),
            if selected { Tone::Bold } else { Tone::Plain },
        )
        .add(&shown, middle_tone)
        .add(&" ".repeat(pad), Tone::Plain)
        .add(&right, right_tone)
        .line();
    let takes_cursor = field.takes_typing() && field.state != KeyState::Saved;
    let dots = field.shown_dots().min(room);
    (line, takes_cursor.then_some((start + dots) as u16))
}

/// Cuts `words` to `room` as drawn: an ASCII ellipsis is three columns.
fn fit_drawn(look: &Look, words: &str, room: usize) -> String {
    if text::width(&look.text(words)) <= room {
        return words.to_owned();
    }
    let extra = text::width(&look.text("…")) - 1;
    text::fit(words, room.saturating_sub(extra))
}

/// What sits under a key: why it stands as it does, or else where to get one.
fn key_under_row(field: &KeyField, look: &Look, width: usize) -> Vec<Line<'static>> {
    if let Some(note) = &field.note {
        return wrapped(look, note, width, Tone::Caution);
    }
    if field.state == KeyState::Replacing {
        return wrapped(
            look,
            "Your saved key stays until a new one works.",
            width,
            Tone::Dim,
        );
    }
    if field.saved {
        return Vec::new();
    }
    wrapped(look, free_key_hint(field.provider), width, Tone::Dim)
}

fn install_lines(
    installing: &Installing,
    look: &Look,
    width: usize,
    now_ms: u64,
) -> Vec<Line<'static>> {
    let mut lines = title_lines(look, "Installing Local");
    for (stage, name) in [
        (Stage::Downloading, "Downloading"),
        (Stage::GettingReady, "Getting ready"),
        (Stage::Checking, "Checking it works"),
    ] {
        lines.push(stage_row(installing, stage, name, look, width, now_ms));
    }
    lines.push(Line::default());
    match &installing.failed {
        Some((message, _)) => lines.extend(wrapped(look, message, width, Tone::Failure)),
        None => lines.extend(wrapped(
            look,
            "Keep this window open. This can take a few minutes.",
            width,
            Tone::Dim,
        )),
    }
    lines
}

fn stage_row(
    installing: &Installing,
    stage: Stage,
    name: &str,
    look: &Look,
    width: usize,
    now_ms: u64,
) -> Line<'static> {
    let current = installing.stage;
    let (marker, marker_tone) = if stage < current {
        (" ✓ ", Tone::Success)
    } else if stage == current && installing.failed.is_some() {
        (" ✕ ", Tone::Failure)
    } else if stage == current {
        (SELECTED, Tone::Accent)
    } else {
        (UNSELECTED, Tone::Plain)
    };
    let tone = match (stage == current, stage < current) {
        (true, _) => Tone::Bold,
        (_, true) => Tone::Plain,
        _ => Tone::Dim,
    };
    let row = Pieces::new(look)
        .add(marker, marker_tone)
        .add(&format!("{name:<STAGE_WIDTH$}"), tone);
    if stage != Stage::Downloading {
        return row.line();
    }
    let mut amounts = format!("{} of {} GB", gb(installing.so_far), gb(installing.total));
    if stage == current
        && installing.failed.is_none()
        && let Some(left) = installing.time_left(now_ms)
    {
        amounts = format!("{amounts} · {left}");
    }
    let start = INDENT + STAGE_WIDTH;
    let room = width.saturating_sub(start + text::width(&amounts) + 3);
    let bar_width = room.clamp(BAR_MIN, BAR_MAX);
    let filled = if installing.total == 0 {
        0
    } else {
        (u128::from(installing.so_far) * bar_width as u128 / u128::from(installing.total)) as usize
    }
    .min(bar_width);
    row.add(&"━".repeat(filled), Tone::Accent)
        .add(&"─".repeat(bar_width - filled), Tone::Dim)
        .add("  ", Tone::Plain)
        .add(&amounts, Tone::Dim)
        .line()
}

fn done_lines(summary: &Summary, look: &Look, width: usize) -> Vec<Line<'static>> {
    let mut lines = title_lines(look, "You're ready");
    let hang = INDENT + text::width(CHECK_MARK) + SUMMARY_LABEL_WIDTH;
    let room = width.saturating_sub(hang + 1).max(1);
    for row in &summary.rows {
        let (mark, tone) = if row.ok {
            (CHECK_MARK, Tone::Success)
        } else {
            (CAUTION_MARK, Tone::Caution)
        };
        let label = if row.value.is_empty() {
            row.label.clone()
        } else {
            format!("{:<SUMMARY_LABEL_WIDTH$}", row.label)
        };
        let values = text::wrap(&row.value, room);
        lines.push(
            Pieces::new(look)
                .add(UNSELECTED, Tone::Plain)
                .add(mark, tone)
                .add(&label, Tone::Plain)
                .add(values.first().map_or("", String::as_str), Tone::Dim)
                .line(),
        );
        for more in values.iter().skip(1) {
            lines.push(
                Pieces::new(look)
                    .add(&" ".repeat(hang), Tone::Plain)
                    .add(more, Tone::Dim)
                    .line(),
            );
        }
    }
    if let Some(try_it) = &summary.try_it {
        lines.push(Line::default());
        lines.extend(wrapped(look, try_it, width, Tone::Plain));
    }
    lines
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::super::super::keys::Action;
    use super::super::super::model::{Msg, Now, update};
    use super::super::super::setup::{ChoiceRow, Finish, KeyStart, SetupEvent, SummaryRow};
    use super::super::draw;
    use super::*;

    const UNICODE: Look = Look {
        color: true,
        ascii: false,
    };
    const NO_COLOR: Look = Look {
        color: false,
        ascii: false,
    };
    const ASCII: Look = Look {
        color: false,
        ascii: true,
    };
    const SIZES: [(u16, u16); 3] = [(80, 24), (60, 16), (120, 40)];
    const NOW: Now = Now {
        mono_ms: 0,
        wall_ms: 0,
    };

    fn app_on(events: Vec<SetupEvent>, size: (u16, u16)) -> App {
        let mut app = App::new(size, NOW);
        let mut setup = SetupState::starting();
        for event in events {
            setup.on_event(event, 0);
        }
        app.setup = Some(setup);
        app
    }

    fn rows_of(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn terminal_with(app: &App, look: &Look, (w, h): (u16, u16)) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test backend");
        terminal.draw(|frame| draw(frame, app, look)).expect("draw");
        terminal
    }

    fn screen(events: Vec<SetupEvent>, look: &Look, size: (u16, u16)) -> Vec<String> {
        rows_of(&terminal_with(&app_on(events, size), look, size))
    }

    fn engine() -> SetupEvent {
        SetupEvent::Ask(Choose {
            step: 1,
            title: "Where should your speech become text?".to_owned(),
            body: Vec::new(),
            rows: vec![
                ChoiceRow::new("Cloud", "Fast and accurate. Needs internet."),
                ChoiceRow::new("Local", "Private. Stays on this computer."),
            ],
            picked: 0,
            note: Some("You can change this later in Settings.".to_owned()),
        })
    }

    fn consent() -> SetupEvent {
        SetupEvent::Ask(Choose {
            step: 2,
            title: "Install Local?".to_owned(),
            body: vec![
                "Voisu downloads about 3 GB once. Then your speech never leaves this computer."
                    .to_owned(),
            ],
            rows: vec![
                ChoiceRow::new("Not now", "Keep using Cloud."),
                ChoiceRow::new("Install", "Download and install now."),
            ],
            picked: 0,
            note: None,
        })
    }

    fn keys_event(groq: KeyState, deepgram: KeyState) -> SetupEvent {
        SetupEvent::Keys {
            step: 2,
            fields: vec![
                KeyStart {
                    provider: Provider::Groq,
                    state: groq,
                    note: None,
                },
                KeyStart {
                    provider: Provider::Deepgram,
                    state: deepgram,
                    note: None,
                },
            ],
            required: true,
        }
    }

    fn installing(so_far: u64) -> Vec<SetupEvent> {
        vec![
            SetupEvent::InstallStarted {
                step: 2,
                total: 3_000_000_000,
            },
            SetupEvent::Downloaded { so_far },
        ]
    }

    fn done() -> SetupEvent {
        let row = |ok, label: &str, value: &str| SummaryRow {
            ok,
            label: label.to_owned(),
            value: value.to_owned(),
        };
        SetupEvent::Finished {
            code: 0,
            finish: Finish::Ready(Summary {
                rows: vec![
                    row(true, "Cloud", "Groq key works"),
                    row(
                        true,
                        "Trigger Key",
                        "Caps Lock — press to start, again to stop",
                    ),
                    row(true, "Voisu is running", ""),
                ],
                try_it: Some(
                    "Try it: click into any text box, press Caps Lock, speak, press it again."
                        .to_owned(),
                ),
            }),
        }
    }

    #[test]
    fn the_engine_question_matches_the_reference_layout() {
        let lines = screen(vec![engine()], &UNICODE, (80, 24));
        assert!(lines[0].starts_with(" voisu   Setup"), "{}", lines[0]);
        assert!(lines[0].ends_with("step 1 of 4"), "{}", lines[0]);
        assert_eq!(lines[2], "   Where should your speech become text?");
        assert_eq!(lines[4], " ▌ Cloud   Fast and accurate. Needs internet.");
        assert_eq!(lines[5], "   Local   Private. Stays on this computer.");
        assert_eq!(lines[7], "   You can change this later in Settings.");
        assert_eq!(lines[23], " ↑↓ pick   ↵ choose   Esc leave setup");
    }

    #[test]
    fn the_consent_question_states_the_size_and_rests_on_not_now() {
        let lines = screen(vec![consent()], &UNICODE, (80, 24));
        assert!(lines[0].ends_with("step 2 of 4"));
        assert_eq!(lines[2], "   Install Local?");
        assert_eq!(
            lines[4],
            "   Voisu downloads about 3 GB once. Then your speech never leaves this"
        );
        assert_eq!(lines[5], "   computer.");
        assert_eq!(lines[7], " ▌ Not now   Keep using Cloud.");
        assert_eq!(lines[8], "   Install   Download and install now.");
    }

    #[test]
    fn the_keys_screen_matches_the_reference_layout() {
        let lines = screen(
            vec![keys_event(KeyState::Blank, KeyState::Blank)],
            &UNICODE,
            (80, 24),
        );
        assert_eq!(lines[2], "   Paste your keys");
        assert_eq!(
            lines[4],
            "   Voisu sends your voice to these services to turn it into text."
        );
        assert_eq!(lines[6], " ▌ Groq key");
        assert_eq!(lines[7], "   Free key: console.groq.com/keys");
        assert_eq!(
            lines[9],
            "   Deepgram key   (optional — better with technical words)"
        );
        assert_eq!(lines[10], "   Free key: console.deepgram.com");
        assert_eq!(
            lines[23],
            " ↑↓ move   Ctrl+V paste   ↵ skip   Esc leave setup"
        );
    }

    #[test]
    fn a_key_shows_as_dots_and_never_as_itself() {
        let mut app = app_on(vec![keys_event(KeyState::Blank, KeyState::Blank)], (80, 24));
        for ch in "gsk_abc".chars() {
            let _ = update(
                &mut app,
                Msg::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)),
                NOW,
            );
        }
        let mut terminal = terminal_with(&app, &UNICODE, (80, 24));
        let lines = rows_of(&terminal);
        assert_eq!(lines[6], " ▌ Groq key       •••••••");
        assert!(!lines.join("\n").contains("gsk"));
        assert_eq!(
            lines[23],
            " ↑↓ move   Ctrl+V paste   ↵ check   Esc leave setup"
        );
        terminal
            .backend_mut()
            .assert_cursor_position(Position::new(25, 6));
    }

    #[test]
    fn a_rejected_key_says_so_and_what_to_do() {
        let mut events = vec![keys_event(KeyState::Blank, KeyState::Blank)];
        events.push(SetupEvent::KeyUpdate {
            provider: Provider::Groq,
            state: KeyState::Rejected,
            dots: Some(4),
            note: Some("That key didn't work. Check you copied all of it.".to_owned()),
        });
        let lines = screen(events, &UNICODE, (80, 24));
        assert!(
            lines[6].starts_with(" ▌ Groq key       ••••"),
            "{}",
            lines[6]
        );
        assert!(lines[6].ends_with("✕ Rejected"), "{}", lines[6]);
        assert_eq!(lines[6].chars().count(), 79);
        assert_eq!(
            lines[7],
            "   That key didn't work. Check you copied all of it."
        );
    }

    #[test]
    fn a_saved_key_offers_keep_or_replace() {
        let events = vec![
            keys_event(KeyState::Saved, KeyState::Blank),
            SetupEvent::SavedWord {
                provider: Provider::Groq,
                word: "Works".to_owned(),
            },
        ];
        let lines = screen(events, &UNICODE, (80, 24));
        assert!(
            lines[6].starts_with(" ▌ Groq key       Saved · Works"),
            "{}",
            lines[6]
        );
        assert!(lines[6].ends_with("↵ replace   → keep"), "{}", lines[6]);
        assert_eq!(lines[7], "", "no key to fetch: one is saved");
        assert_eq!(lines[23], " ↑↓ move   ↵ replace   → keep   Esc leave setup");
    }

    #[test]
    fn the_install_shows_its_stages_with_no_way_to_cancel_but_stopping() {
        let mut app = app_on(installing(1_200_000_000), (80, 24));
        app.now = Now {
            mono_ms: 60_000,
            wall_ms: 0,
        };
        let lines = rows_of(&terminal_with(&app, &UNICODE, (80, 24)));
        assert_eq!(lines[2], "   Installing Local");
        assert!(lines[4].starts_with(" ▌ Downloading    ━"), "{}", lines[4]);
        assert!(
            lines[4].ends_with("1.2 of 3.0 GB · about 2 min left"),
            "{}",
            lines[4]
        );
        assert_eq!(lines[5], "   Getting ready");
        assert_eq!(lines[6], "   Checking it works");
        assert_eq!(
            lines[8],
            "   Keep this window open. This can take a few minutes."
        );
        assert_eq!(lines[23], " Ctrl-C stop");
    }

    #[test]
    fn a_finished_stage_gets_its_check_mark() {
        let mut events = installing(3_000_000_000);
        events.push(SetupEvent::Stage(Stage::GettingReady));
        let lines = screen(events, &UNICODE, (80, 24));
        assert!(lines[4].starts_with(" ✓ Downloading    ━"), "{}", lines[4]);
        assert!(lines[4].ends_with("3.0 of 3.0 GB"), "{}", lines[4]);
        assert!(!lines[4].contains('─'), "{}", lines[4]);
        assert_eq!(lines[5], " ▌ Getting ready");
    }

    #[test]
    fn a_failed_install_says_what_to_do_across_two_lines() {
        let mut events = installing(1_200_000_000);
        events.push(SetupEvent::InstallFailed {
            message: "✕ The download stopped. Check your internet, then ↵ to try again. Esc uses Cloud for now.".to_owned(),
            esc: "use Cloud",
        });
        let lines = screen(events, &UNICODE, (80, 24));
        assert!(lines[4].starts_with(" ✕ Downloading"), "{}", lines[4]);
        assert_eq!(
            lines[8],
            "   ✕ The download stopped. Check your internet, then ↵ to try again. Esc uses"
        );
        assert_eq!(lines[9], "   Cloud for now.");
        assert_eq!(lines[23], " ↵ try again   Esc use Cloud");
    }

    #[test]
    fn the_end_of_setup_matches_the_reference_layout() {
        let lines = screen(vec![done()], &UNICODE, (80, 24));
        assert!(lines[0].ends_with("step 4 of 4"));
        assert_eq!(lines[2], "   You're ready");
        assert_eq!(lines[4], "   ✓ Cloud           Groq key works");
        assert_eq!(
            lines[5],
            "   ✓ Trigger Key     Caps Lock — press to start, again to stop"
        );
        assert_eq!(lines[6], "   ✓ Voisu is running");
        assert_eq!(
            lines[8],
            "   Try it: click into any text box, press Caps Lock, speak, press it again."
        );
        assert_eq!(lines[23], " ↵ open Voisu   q quit");
    }

    #[test]
    fn a_setup_that_failed_says_why_and_lets_the_person_move_on() {
        let lines = screen(
            vec![SetupEvent::Finished {
                code: 4,
                finish: Finish::Failed("Voisu has no working key yet.".to_owned()),
            }],
            &UNICODE,
            (80, 24),
        );
        assert_eq!(lines[2], "   Setup didn't finish");
        assert_eq!(lines[4], "   Voisu has no working key yet.");
        assert_eq!(lines[23], " ↵ open Voisu   q quit");
    }

    #[test]
    fn the_desktop_wait_says_what_is_happening() {
        let lines = screen(
            vec![SetupEvent::Waiting {
                step: 3,
                title: "Choose a key that starts talking".to_owned(),
            }],
            &UNICODE,
            (80, 24),
        );
        assert!(lines[0].ends_with("step 3 of 4"));
        assert_eq!(lines[7], "   Waiting for your desktop…");
        assert_eq!(lines[23], " Esc later   Ctrl-C stop");
    }

    /// Every screen, every size, every look: nothing overflows and nothing that
    /// matters falls off the bottom.
    #[test]
    fn every_screen_fits_every_supported_size_and_look() {
        let saved_note = SetupEvent::KeyUpdate {
            provider: Provider::Groq,
            state: KeyState::Transient("Busy, try again soon".to_owned()),
            dots: Some(300),
            note: Some("Voisu couldn't check it. ↵ saves it anyway.".to_owned()),
        };
        let mut failed = installing(1_200_000_000);
        failed.push(SetupEvent::InstallFailed {
            message: "✕ The download stopped. Check your internet, then ↵ to try again. Esc uses Cloud for now.".to_owned(),
            esc: "use Cloud",
        });
        let screens: Vec<(Vec<SetupEvent>, &str)> = vec![
            (vec![engine()], "Local"),
            (vec![consent()], "Install"),
            (
                vec![keys_event(KeyState::Saved, KeyState::Blank)],
                "Free key: console.deepgram.com",
            ),
            (
                vec![keys_event(KeyState::Blank, KeyState::Blank), saved_note],
                "Free key: console.deepgram.com",
            ),
            (installing(1_200_000_000), "Checking it works"),
            (failed, "Cloud for now"),
            (vec![done()], "again"),
        ];
        for (events, must_show) in screens {
            for look in [&UNICODE, &NO_COLOR, &ASCII] {
                for size in SIZES {
                    let lines = screen(events.clone(), look, size);
                    let joined = lines.join("\n");
                    assert!(
                        joined.contains(must_show),
                        "{size:?} lost {must_show:?}:\n{joined}"
                    );
                    assert!(lines[0].contains("step"), "{joined}");
                    let footer = lines.last().expect("footer");
                    assert!(!footer.is_empty(), "{size:?} no footer:\n{joined}");
                    if look.ascii {
                        assert!(joined.is_ascii(), "{size:?}:\n{joined}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_keys_screen_keeps_a_hint_on_the_right_only_when_it_fits() {
        let lines = screen(
            vec![keys_event(KeyState::Saved, KeyState::Blank)],
            &ASCII,
            (60, 16),
        );
        assert_eq!(lines[6], " > Groq key       Saved - Checking...");
    }

    #[test]
    fn enter_on_the_choice_moves_the_flow_along() {
        let mut app = app_on(vec![engine()], (80, 24));
        let setup = app.setup.as_mut().expect("setup");
        setup.act(Action::Down);
        let lines = rows_of(&terminal_with(&app, &UNICODE, (80, 24)));
        assert_eq!(lines[5], " ▌ Local   Private. Stays on this computer.");
    }
}
