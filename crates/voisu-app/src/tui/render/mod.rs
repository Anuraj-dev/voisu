//! Draws one frame from the model. Nothing here changes state.
//!
//! The chrome is a header, a tab line with its rule, a body, one line for a toast
//! or prompt, and the footer. No boxes: hierarchy comes from spacing, bold and
//! dim.

mod dictionary;
mod history;
mod home;
mod settings;

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::keys::{self, Context};
use super::look::{Look, Tone};
use super::model::{App, Hero, MIN_HEIGHT, MIN_WIDTH, Prompt, Tab};
use super::text;

/// The selected row's marker and the indent of an unselected one.
const SELECTED: &str = " ▌ ";
const UNSELECTED: &str = "   ";
const FOOTER_GAP: &str = "   ";

/// A line built from styled pieces.
struct Pieces<'a> {
    look: &'a Look,
    spans: Vec<Span<'static>>,
}

impl<'a> Pieces<'a> {
    fn new(look: &'a Look) -> Self {
        Self {
            look,
            spans: Vec::new(),
        }
    }

    fn add(mut self, text: &str, tone: Tone) -> Self {
        self.spans
            .push(Span::styled(self.look.text(text), self.look.style(tone)));
        self
    }

    fn line(self) -> Line<'static> {
        Line::from(self.spans)
    }
}

fn single(look: &Look, content: &str, tone: Tone) -> Line<'static> {
    Pieces::new(look).add(content, tone).line()
}

/// A list row: the marker, then the text, bold when selected. The `▌` marker is
/// what carries selection when bold does not survive.
fn list_row(look: &Look, selected: bool, content: &str, width: usize) -> Line<'static> {
    let marker = if selected { SELECTED } else { UNSELECTED };
    let room = width.saturating_sub(text::width(marker));
    Pieces::new(look)
        .add(marker, if selected { Tone::Bold } else { Tone::Plain })
        .add(
            &text::fit(content, room),
            if selected { Tone::Bold } else { Tone::Plain },
        )
        .line()
}

pub fn draw(frame: &mut Frame<'_>, app: &App, look: &Look) {
    let area = frame.area();
    if app.too_small() {
        let message = format!(
            "Terminal too small — {}x{}, need {MIN_WIDTH}x{MIN_HEIGHT}",
            area.width, area.height
        );
        let lines = vec![
            single(look, &text::fit(&message, area.width as usize), Tone::Bold),
            footer_line(look, &app.context(), area.width as usize),
        ];
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }

    let roomy = app.roomy();
    let width = area.width as usize;
    let mut row = 0;
    let mut next_line = |rows: u16| {
        let rect = Rect::new(area.x, area.y + row, area.width, rows);
        row += rows;
        rect
    };

    let header = next_line(1);
    if roomy {
        next_line(1);
    }
    let tabs = next_line(2);
    if roomy {
        next_line(1);
    }
    let body = next_line(app.body_height() as u16);
    let notice = next_line(1);
    let footer = next_line(1);

    frame.render_widget(Paragraph::new(header_line(app, look, width)), header);
    frame.render_widget(Paragraph::new(tab_lines(app, look)), tabs);
    let body_lines = if app.overlay && app.prompt.is_none() {
        overlay_lines(app, look, width)
    } else {
        match app.tab {
            Tab::Home => home::lines(app, look, width, body.height as usize),
            Tab::History => history::lines(app, look, width),
            Tab::Dictionary => dictionary::lines(app, look, width, body.height as usize),
            Tab::Settings => settings::lines(app, look, width, body.height as usize),
        }
    };
    frame.render_widget(Paragraph::new(body_lines), body);
    let (notice_line, cursor) = notice_line(app, look, width);
    frame.render_widget(Paragraph::new(notice_line), notice);
    if let Some(column) = cursor {
        frame.set_cursor_position(Position::new(column.min(area.width - 1), notice.y));
    }
    frame.render_widget(
        Paragraph::new(footer_line(look, &app.context(), width)),
        footer,
    );
}

/// The word and glyph that sum up the current state.
fn state_summary(app: &App) -> (&'static str, Tone) {
    match app.hero() {
        Hero::Checking => ("Checking", Tone::Dim),
        Hero::NeedsRestart => ("✕ Needs restart", Tone::Failure),
        Hero::NotSetUp => ("! Not set up", Tone::Caution),
        Hero::NotRunning => ("✕ Not running", Tone::Failure),
        Hero::Failing { .. } => ("✕ Needs attention", Tone::Failure),
        Hero::Recording { .. } => ("● Recording", Tone::Recording),
        Hero::Processing => ("● Working", Tone::Processing),
        Hero::Ready => ("● Ready", Tone::Success),
    }
}

fn header_line(app: &App, look: &Look, width: usize) -> Line<'static> {
    const NAME: &str = " voisu";
    let (state, tone) = state_summary(app);
    let state = look.text(state);
    let gap = width
        .saturating_sub(text::width(NAME) + text::width(&state) + 1)
        .max(1);
    Pieces::new(look)
        .add(NAME, Tone::Bold)
        .add(&" ".repeat(gap), Tone::Plain)
        .add(&state, tone)
        .line()
}

fn tab_lines(app: &App, look: &Look) -> Vec<Line<'static>> {
    const GAP: &str = "    ";
    let mut names = Pieces::new(look).add(" ", Tone::Plain);
    let mut column = 1;
    let mut rule = (1, 0);
    for (index, tab) in Tab::ALL.iter().enumerate() {
        if index > 0 {
            names = names.add(GAP, Tone::Plain);
            column += GAP.len();
        }
        let title = tab.title();
        let active = *tab == app.tab;
        names = names.add(title, if active { Tone::Bold } else { Tone::Dim });
        if active {
            rule = (column, title.len());
        }
        column += title.len();
    }
    let underline = Pieces::new(look)
        .add(&" ".repeat(rule.0), Tone::Plain)
        .add(&"━".repeat(rule.1), Tone::Bold)
        .line();
    vec![names.line(), underline]
}

/// The `?` overlay: every key of the tab underneath, from the footer's table.
fn overlay_lines(app: &App, look: &Look, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![
        single(look, &format!("   Keys on {}", app.tab.title()), Tone::Bold),
        Line::default(),
    ];
    for binding in keys::bindings(&app.base_context()) {
        lines.push(key_row(look, binding.all_keys, &binding.label, width));
    }
    lines
}

fn key_row(look: &Look, keys: &str, label: &str, width: usize) -> Line<'static> {
    let keys = look.text(keys);
    let keys = format!("   {keys:<27}");
    let room = width.saturating_sub(text::width(&keys));
    Pieces::new(look)
        .add(&keys, Tone::Bold)
        .add(&text::fit(label, room), Tone::Dim)
        .line()
}

/// The line above the footer: the open prompt, or else the latest toast.
/// Returns the column for the terminal cursor when a prompt takes typing.
fn notice_line(app: &App, look: &Look, width: usize) -> (Line<'static>, Option<u16>) {
    if let Some(prompt) = &app.prompt {
        let (label, shown, takes_text) = match prompt {
            Prompt::AddTerm(input) => (
                "Add a word: ".to_owned(),
                text::clean_line(input.as_str()),
                true,
            ),
            // Only the count of characters ever reaches the screen.
            Prompt::Key(provider, input) => (
                format!("{} key: ", provider.cli_label()),
                "•".repeat(input.chars()),
                true,
            ),
            Prompt::ConfirmRemove(term) => (
                format!("Remove \"{}\"? y/n", text::clean_line(term)),
                String::new(),
                false,
            ),
        };
        let label = look.text(&text::capitalized(&label));
        let lead = format!(" {label}");
        let room = width.saturating_sub(text::width(&lead) + 1);
        let shown = look.text(&shown);
        // Keep the end of long input visible, where the typing happens.
        let shown = tail(&shown, room);
        let cursor = takes_text.then(|| (text::width(&lead) + text::width(&shown)) as u16);
        let line = Pieces::new(look)
            .add(&lead, Tone::Bold)
            .add(&shown, Tone::Plain)
            .line();
        return (line, cursor);
    }
    let Some(toast) = &app.toast else {
        return (Line::default(), None);
    };
    let glyph = match toast.kind {
        Tone::Success => "✓",
        Tone::Failure => "✕",
        _ => "!",
    };
    let body = format!(" {glyph} {}", text::clean_line(&toast.text));
    (single(look, &text::fit(&body, width), toast.kind), None)
}

/// The last `max` columns of `text`.
fn tail(text: &str, max: usize) -> String {
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for ch in text.chars().rev() {
        let cell = self::text::width(ch.encode_utf8(&mut [0; 4]));
        if used + cell > max {
            break;
        }
        kept.push(ch);
        used += cell;
    }
    kept.into_iter().rev().collect()
}

fn footer_line(look: &Look, context: &Context, width: usize) -> Line<'static> {
    let hints = keys::footer(context);
    let cost =
        |hint: &keys::Binding| text::width(&look.text(hint.keys)) + 1 + text::width(&hint.label);
    let total = |gap: usize| {
        1 + hints.iter().map(cost).sum::<usize>() + gap * hints.len().saturating_sub(1)
    };
    // The ASCII `Enter` is wider than `↵`: tighten the gaps before losing a hint.
    let gap = if total(FOOTER_GAP.len()) > width {
        &FOOTER_GAP[1..]
    } else {
        FOOTER_GAP
    };
    let mut pieces = Pieces::new(look).add(" ", Tone::Plain);
    let mut used = 1;
    for (index, hint) in hints.iter().enumerate() {
        let shown_keys = look.text(hint.keys);
        let gap = if index == 0 { "" } else { gap };
        if used + gap.len() + cost(hint) > width {
            break;
        }
        pieces = pieces
            .add(gap, Tone::Plain)
            .add(&shown_keys, Tone::Bold)
            .add(" ", Tone::Plain)
            .add(&hint.label, Tone::Dim);
        used += gap.len() + cost(hint);
    }
    if matches!(context, Context::Guarded { .. }) && used + 30 < width {
        pieces = pieces.add("   Keys pause while you dictate", Tone::Dim);
    }
    pieces.line()
}

/// A short phrase for when the daemon has to answer first.
fn daemon_needed(app: &App) -> bool {
    app.daemon_down()
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;
    use voisu_core::{DaemonState, Provider, ReadinessStatus};

    use super::super::keys::every_context;
    use super::super::model::test_support::*;
    use super::super::model::{Msg, Reply, SettingsRow, StatusPoll, update};
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

    fn screen_with(app: &App, look: &Look, (w, h): (u16, u16)) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test backend");
        terminal.draw(|frame| draw(frame, app, look)).expect("draw");
        terminal
    }

    fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
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

    fn screen(app: &mut App, look: &Look, size: (u16, u16)) -> Vec<String> {
        let _ = update(app, Msg::Resize(size.0, size.1), app.now);
        rows(&screen_with(app, look, size))
    }

    fn joined(lines: &[String]) -> String {
        lines.join("\n")
    }

    fn app_on(tab_key: char) -> App {
        let mut app = ready_app();
        let _ = press(&mut app, KeyCode::Char(tab_key), 0);
        app
    }

    fn with_terms(terms: &[&str]) -> App {
        let mut app = app_on('3');
        let terms = terms.iter().map(|term| (*term).to_owned()).collect();
        let _ = update(&mut app, Msg::Reply(Reply::Dictionary(Ok(terms))), at(0));
        app
    }

    fn with_records() -> App {
        let mut app = app_on('2');
        let records = vec![
            item(Some("Hey, so the current CLI is okay."), None),
            item(None, Some("No Transcript. Recording contains no speech")),
        ];
        let _ = update(&mut app, Msg::Reply(Reply::History(Ok(records))), at(0));
        app
    }

    // --- The reference layout -----------------------------------------------------

    #[test]
    fn home_at_80x24_matches_the_reference_layout() {
        let mut app = ready_app();
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert!(lines[0].starts_with(" voisu"));
        assert!(lines[0].ends_with("● Ready"), "{}", lines[0]);
        assert_eq!(lines[0].chars().count(), 79);
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], " Home    History    Dictionary    Settings");
        assert_eq!(lines[3], " ━━━━");
        assert_eq!(lines[4], "");
        assert_eq!(lines[5], "   Ready to dictate");
        assert_eq!(
            lines[6],
            "   Press your Trigger Key, speak, and press it again."
        );
        assert_eq!(lines[7], "");
        assert!(lines[8].starts_with("   Last       "), "{}", lines[8]);
        assert_eq!(lines[9], "   Engine     Cloud");
        assert_eq!(lines[10], "   Delivery   Clipboard");
        assert_eq!(lines[11], "   Writing    Smart · Adaptive");
        assert_eq!(lines[12], "   Health     15 passed · 1 warning");
        assert_eq!(lines[23], " ←→ tabs   d checks   ? help   q quit");
    }

    #[test]
    fn the_last_row_quotes_and_truncates_the_newest_transcript() {
        let mut app = ready_app();
        let long =
            "Hey, so the current CLI is okay, but I'm thinking about a much better interface";
        let _ = update(
            &mut app,
            Msg::Reply(Reply::History(Ok(vec![item(Some(long), None)]))),
            at(48_000),
        );
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert!(lines[8].contains("48s ago  \"Hey, so"), "{}", lines[8]);
        assert!(lines[8].ends_with("…\""), "{}", lines[8]);
        assert!(lines[8].chars().count() <= 80);
    }

    #[test]
    fn the_settings_tab_matches_the_reference_layout() {
        let mut app = app_on('4');
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert_eq!(lines[5], " ▌ Engine        Cloud");
        assert_eq!(lines[6], "   Delivery      Clipboard");
        assert_eq!(lines[7], "   Writing       Smart");
        assert_eq!(lines[8], "   Rendering     Adaptive");
        assert_eq!(lines[9], "   Deepgram      On");
        assert_eq!(lines[10], "   Groq key      Valid");
        assert_eq!(lines[11], "   Deepgram key  Unreachable");
        assert_eq!(lines[12], "   Trigger Key   Not bound");
        assert_eq!(lines[13], "   Run setup");
        assert_eq!(lines[14], "");
        assert!(lines[15].starts_with("   Where your speech becomes text."));
    }

    #[test]
    fn the_delivery_row_uses_the_specified_description() {
        let mut app = app_on('4');
        let _ = press(&mut app, KeyCode::Down, 0);
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert_eq!(lines[6], " ▌ Delivery      Clipboard");
        assert_eq!(
            lines[15],
            "   How your words reach the app. Clipboard copies and pastes; Type types them."
        );
    }

    // --- Every tab at every size --------------------------------------------------

    #[test]
    fn all_four_tabs_render_their_chrome_at_every_size() {
        for size in SIZES {
            for (key, title) in [
                ('1', "Home"),
                ('2', "History"),
                ('3', "Dictionary"),
                ('4', "Settings"),
            ] {
                let mut app = app_on(key);
                let lines = screen(&mut app, &UNICODE, size);
                let joined = joined(&lines);
                assert!(lines[0].contains("voisu"), "{size:?} {title}");
                assert!(lines[0].contains("● Ready"), "{size:?} {title}");
                assert!(joined.contains("Home    History    Dictionary    Settings"));
                assert!(
                    lines[size.1 as usize - 1].contains("q quit"),
                    "{size:?} {title}"
                );
                // The rule sits under the active tab's title.
                let tab_row = lines
                    .iter()
                    .position(|l| l.contains("Home    History"))
                    .expect("tabs");
                let rule = &lines[tab_row + 1];
                let column = lines[tab_row].find(title).expect("title");
                assert_eq!(rule.find('━'), Some(column), "{size:?} {title}: {rule}");
                assert_eq!(rule.matches('━').count(), title.len());
            }
        }
    }

    #[test]
    fn every_settings_row_and_every_description_fit_the_minimum_size() {
        for index in 0..SettingsRow::ALL.len() {
            let mut app = app_on('4');
            app.settings_selected = index;
            let lines = screen(&mut app, &UNICODE, (60, 16));
            let body = joined(&lines);
            for row in SettingsRow::ALL {
                assert!(
                    body.contains(row.label()),
                    "{index}: lost {}\n{body}",
                    row.label()
                );
            }
            let description = app.selected_setting().description(None);
            let last_word = description.split_whitespace().last().expect("word");
            assert!(body.contains(last_word), "{index}: description cut\n{body}");
        }
    }

    #[test]
    fn nothing_overflows_at_the_minimum_size_in_any_look() {
        for look in [UNICODE, ASCII] {
            for key in ['1', '2', '3', '4'] {
                let mut app = app_on(key);
                let lines = screen(&mut app, &look, (60, 16));
                for line in &lines {
                    assert!(text::width(line) <= 60, "{line}");
                }
                assert!(
                    lines[15].contains("quit"),
                    "footer lost quit at 60 wide: {}",
                    lines[15]
                );
            }
        }
    }

    #[test]
    fn too_small_says_both_sizes_and_keeps_q() {
        let mut app = ready_app();
        let lines = screen(&mut app, &UNICODE, (52, 14));
        assert_eq!(lines[0], "Terminal too small — 52x14, need 60x16");
        assert_eq!(lines[1], " q quit");
    }

    #[test]
    fn too_small_applies_on_either_dimension() {
        for size in [(59, 40), (120, 15)] {
            let mut app = ready_app();
            assert!(joined(&screen(&mut app, &UNICODE, size)).contains("Terminal too small"));
        }
    }

    // --- Home ---------------------------------------------------------------------

    #[test]
    fn home_names_the_problem_and_the_one_key_for_it() {
        let mut app = ready_app();
        let _ = update(
            &mut app,
            Msg::Reply(Reply::Status(StatusPoll::Unavailable)),
            at(0),
        );
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert!(lines[0].ends_with("✕ Not running"));
        assert_eq!(lines[5], "   Voisu isn't running");
        assert_eq!(lines[6], "   ↵ Start Voisu");
        assert_eq!(
            lines[23],
            " ↵ Start Voisu   ←→ tabs   d checks   ? help   q quit"
        );

        let _ = update(&mut app, Msg::Reply(Reply::SetupNeeded(true)), at(0));
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert_eq!(lines[5], "   Voisu isn't set up yet");
        assert_eq!(lines[6], "   ↵ Run setup");
        assert!(lines[0].ends_with("! Not set up"));
    }

    #[test]
    fn a_daemon_that_cannot_be_read_asks_for_a_restart_with_a_ctrl_key() {
        let mut app = ready_app();
        let _ = update(
            &mut app,
            Msg::Reply(Reply::Status(StatusPoll::Mismatch)),
            at(0),
        );
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert!(lines[0].ends_with("✕ Needs restart"));
        assert_eq!(lines[5], "   Voisu needs a restart");
        assert_eq!(lines[6], "   Ctrl-R Restart Voisu");
        assert!(lines[23].contains("Ctrl-R Restart Voisu"));
    }

    #[test]
    fn a_failed_check_shows_its_label_and_existing_action_text() {
        let mut app = ready_app();
        let mut failing = row("Clipboard", "missing", ReadinessStatus::Fail);
        failing.action = Some("install wl-clipboard on Wayland or xclip on X11".to_owned());
        let _ = update(&mut app, Msg::Reply(Reply::Doctor(vec![failing])), at(0));
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert_eq!(lines[5], "   Clipboard: missing");
        assert_eq!(
            lines[6],
            "   install wl-clipboard on Wayland or xclip on X11"
        );
        assert!(lines[0].ends_with("✕ Needs attention"));
    }

    #[test]
    fn recording_and_processing_read_as_specified() {
        let mut app = ready_app();
        let _ = update(
            &mut app,
            Msg::Reply(Reply::Status(StatusPoll::Up(snapshot(
                DaemonState::Recording,
            )))),
            at(0),
        );
        let _ = update(&mut app, Msg::Tick, at(12_000));
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert_eq!(lines[5], "   Recording  0:12");
        assert!(lines[0].ends_with("● Recording"));
        assert!(lines[23].starts_with(" Ctrl-C quit"), "{}", lines[23]);

        let _ = update(
            &mut app,
            Msg::Reply(Reply::Status(StatusPoll::Up(snapshot(
                DaemonState::Processing,
            )))),
            at(12_500),
        );
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert_eq!(lines[5], "   Working on your Transcript…");
        assert!(lines[0].ends_with("● Working"));
    }

    #[test]
    fn the_checks_list_replaces_the_rows_below_the_hero() {
        let mut app = ready_app();
        let mut failing = row("Socket", "missing", ReadinessStatus::Fail);
        failing.action = Some("voisu service start".to_owned());
        let rows = vec![
            row("Pass one", "ok", ReadinessStatus::Pass),
            row("Warn one", "hm", ReadinessStatus::Warn),
            failing,
        ];
        let _ = update(&mut app, Msg::Reply(Reply::Doctor(rows)), at(0));
        let _ = press(&mut app, KeyCode::Char('d'), 0);
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert!(lines[5].contains("Socket"), "hero stays");
        assert!(lines[8].starts_with("   ✕ FAIL  Socket"), "{}", lines[8]);
        assert_eq!(lines[9], "           voisu service start");
        assert!(
            lines[10].starts_with("   ! WARN  Warn one"),
            "{}",
            lines[10]
        );
        assert!(
            lines[11].starts_with("   ✓ PASS  Pass one"),
            "{}",
            lines[11]
        );
        assert!(!joined(&lines).contains("Delivery  "));
        assert!(lines[23].contains("d summary"));
    }

    // --- History ------------------------------------------------------------------

    #[test]
    fn history_stacks_the_list_over_the_selected_record() {
        let mut app = with_records();
        let lines = screen(&mut app, &UNICODE, (80, 24));
        let body = joined(&lines);
        assert!(lines[5].starts_with(" ▌ "), "{}", lines[5]);
        assert!(lines[5].contains("Hey, so the current CLI"), "{}", lines[5]);
        assert!(
            lines[6].contains("Recording contains no speech"),
            "{}",
            lines[6]
        );
        assert!(body.contains("Groq 300ms"), "{body}");

        let _ = press(&mut app, KeyCode::Down, 0);
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert!(lines[6].starts_with(" ▌ "), "{}", lines[6]);
        let pane = joined(&lines[11..]);
        assert!(
            pane.contains("No Transcript"),
            "failed record says so: {pane}"
        );
        assert!(pane.contains("Recording contains no speech"), "{pane}");
    }

    #[test]
    fn history_empty_state_reads_as_specified() {
        let mut app = app_on('2');
        let body = joined(&screen(&mut app, &UNICODE, (80, 24)));
        assert!(body.contains("Nothing yet. Your dictations will appear here."));
        assert!(!body.contains("c copy"), "nothing to copy");
    }

    #[test]
    fn a_long_transcript_scrolls_inside_its_pane() {
        let mut app = app_on('2');
        let long = (0..300)
            .map(|n| format!("word{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let _ = update(
            &mut app,
            Msg::Reply(Reply::History(Ok(vec![item(Some(&long), None)]))),
            at(0),
        );
        let first = joined(&screen(&mut app, &UNICODE, (80, 24)));
        assert!(first.contains("word0 "));
        assert!(!first.contains("word299"));
        for _ in 0..20 {
            let _ = press(&mut app, KeyCode::PageDown, 0);
        }
        let last = joined(&screen(&mut app, &UNICODE, (80, 24)));
        assert!(last.contains("word299"));
        assert!(last.contains("Groq 300ms"), "provider line stays put");
    }

    #[test]
    fn a_hostile_transcript_cannot_reach_the_terminal() {
        let mut app = app_on('2');
        let hostile = text::clean_block("safe\x1b[2J text");
        let _ = update(
            &mut app,
            Msg::Reply(Reply::History(Ok(vec![item(Some(&hostile), None)]))),
            at(0),
        );
        let body = joined(&screen(&mut app, &UNICODE, (80, 24)));
        assert!(!body.contains('\x1b'));
    }

    // --- Dictionary ---------------------------------------------------------------

    #[test]
    fn the_dictionary_lists_terms_and_prompts_inline() {
        let mut app = with_terms(&["Voisu", "Hyprland"]);
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert_eq!(lines[5], " ▌ Voisu");
        assert_eq!(lines[6], "   Hyprland");
        assert_eq!(lines[23], " a add   d remove   ←→ tabs   ? help   q quit");

        let _ = press(&mut app, KeyCode::Char('d'), 0);
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert_eq!(lines[22], " Remove \"Voisu\"? y/n");
        assert_eq!(lines[23], " y remove   n keep");
        let _ = press(&mut app, KeyCode::Char('n'), 0);

        let _ = press(&mut app, KeyCode::Char('a'), 0);
        type_text(&mut app, "ratatui", 0);
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert_eq!(lines[22], " Add a word: ratatui");
        assert_eq!(lines[23], " ↵ add   Esc cancel");
    }

    #[test]
    fn the_dictionary_empty_state_explains_itself_in_one_line() {
        let mut app = app_on('3');
        let lines = screen(&mut app, &UNICODE, (60, 16));
        let body = joined(&lines);
        assert!(body.contains("Add names and jargon"), "{body}");
        assert!(!body.contains("d remove"));
    }

    // --- Secrets ------------------------------------------------------------------

    #[test]
    fn a_key_prompt_only_ever_shows_bullets() {
        const SECRET: &str = "gsk_secret_value";
        for look in [UNICODE, ASCII] {
            let mut app = app_on('4');
            app.settings_selected = 5;
            let _ = press(&mut app, KeyCode::Enter, 0);
            type_text(&mut app, SECRET, 0);
            let lines = screen(&mut app, &look, (80, 24));
            let body = joined(&lines);
            assert!(!body.contains(SECRET), "{body}");
            assert!(!body.contains("gsk"), "{body}");
            let bullet = if look.ascii { "*" } else { "•" };
            assert!(
                lines[22].contains(&bullet.repeat(SECRET.chars().count())),
                "{}",
                lines[22]
            );
            assert!(lines[22].contains("Groq key"), "{}", lines[22]);
        }
    }

    #[test]
    fn a_saved_key_shows_checking_until_the_doctor_answers() {
        let mut app = app_on('4');
        app.key_in_check = Some(Provider::Deepgram);
        let lines = screen(&mut app, &UNICODE, (80, 24));
        assert_eq!(lines[11], "   Deepgram key  Checking…");
    }

    // --- Footer and overlay agree -------------------------------------------------

    #[test]
    fn the_overlay_lists_everything_the_footer_shows_for_every_tab() {
        for key in ['1', '2', '3', '4'] {
            let mut app = app_on(key);
            let context = app.base_context();
            let _ = press(&mut app, KeyCode::Char('?'), 0);
            assert!(app.overlay);
            let body = joined(&screen(&mut app, &UNICODE, (80, 24)));
            for hint in keys::bindings(&context) {
                assert!(body.contains(hint.label.as_ref()), "{key}: {}", hint.label);
                assert!(body.contains(hint.all_keys), "{key}: {}", hint.all_keys);
            }
            assert_eq!(
                screen(&mut app, &UNICODE, (80, 24))[23],
                " Esc close   q quit",
                "the overlay has its own context"
            );
        }
    }

    #[test]
    fn every_footer_fits_the_minimum_width_in_both_glyph_sets() {
        for context in every_context() {
            for look in [UNICODE, ASCII] {
                let line = footer_line(&look, &context, 60);
                let drawn: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                assert!(text::width(&drawn) <= 60, "{context:?}: {drawn}");
                let declared: Vec<_> = keys::footer(&context);
                for hint in declared {
                    assert!(
                        drawn.contains(hint.label.as_ref()),
                        "{context:?} dropped {} in: {drawn}",
                        hint.label
                    );
                }
            }
        }
    }

    // --- Look ---------------------------------------------------------------------

    #[test]
    fn without_colour_selection_and_state_are_still_there_in_text() {
        let mut app = app_on('4');
        let lines = screen(&mut app, &NO_COLOR, (80, 24));
        assert!(lines[5].starts_with(" ▌ Engine"));
        assert!(lines[0].ends_with("● Ready"));
        let terminal = screen_with(&app, &NO_COLOR, (80, 24));
        let buffer = terminal.backend().buffer();
        for cell in &buffer.content {
            assert_eq!(
                cell.fg,
                ratatui::style::Color::Reset,
                "no colour may be set"
            );
            assert_eq!(cell.bg, ratatui::style::Color::Reset, "default background");
        }
    }

    #[test]
    fn colour_uses_only_named_ansi_colours_and_never_the_background() {
        use ratatui::style::Color;
        let mut app = ready_app();
        let _ = update(
            &mut app,
            Msg::Reply(Reply::Status(StatusPoll::Up(snapshot(
                DaemonState::Processing,
            )))),
            at(0),
        );
        let terminal = screen_with(&app, &UNICODE, (80, 24));
        for cell in &terminal.backend().buffer().content {
            assert!(
                matches!(
                    cell.fg,
                    Color::Reset | Color::Green | Color::Blue | Color::Yellow | Color::Red
                ),
                "{:?}",
                cell.fg
            );
            assert_eq!(cell.bg, Color::Reset);
        }
    }

    #[test]
    fn the_ascii_set_draws_every_screen_in_pure_ascii() {
        for key in ['1', '2', '3', '4'] {
            let mut app = app_on(key);
            let lines = screen(&mut app, &ASCII, (80, 24));
            let body = joined(&lines);
            assert!(body.is_ascii(), "{body}");
            assert!(body.contains("* Ready"));
        }
        let mut app = app_on('4');
        let lines = screen(&mut app, &ASCII, (80, 24));
        assert!(lines[5].starts_with(" > Engine"), "{}", lines[5]);
        assert_eq!(lines[3].trim().len(), "Settings".len());
    }

    #[test]
    fn user_facing_settings_only() {
        let mut app = app_on('4');
        let body = joined(&screen(&mut app, &UNICODE, (120, 40))).to_lowercase();
        for hidden in ["local-dev", "replay", "export", "retention", "revision"] {
            assert!(!body.contains(hidden), "{hidden} leaked");
        }
    }
}
