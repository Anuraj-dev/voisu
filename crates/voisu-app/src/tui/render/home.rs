//! Home: the one most important thing, then the rows below it.

use ratatui::text::Line;

use super::super::keys::{self, Action};
use super::super::look::{Look, Tone};
use super::super::model::{
    App, CHECK_ACTION_COLUMN, CHECK_VALUE_COLUMN, CheckLine, Fix, HERO_HEIGHT, Hero, ServiceVerb,
    delivery_value, engine_label, layout_label, tone_label, writing_label,
};
use super::super::text;
use super::{Pieces, single};
use voisu_core::ReadinessStatus;

const INDENT: &str = "   ";

pub(super) fn lines(app: &App, look: &Look, width: usize, height: usize) -> Vec<Line<'static>> {
    let mut lines = hero_lines(app, look, width);
    lines.push(Line::default());
    debug_assert_eq!(lines.len(), HERO_HEIGHT);
    if app.checks_open {
        lines.extend(check_rows(
            app,
            look,
            width,
            height.saturating_sub(HERO_HEIGHT),
        ));
    } else {
        lines.extend(summary_rows(app, look, width));
        if let Some(version) = &app.update_available {
            let notice = crate::update_notice::notice_line(version);
            lines.push(Line::default());
            lines.push(single(
                look,
                &format!(
                    "{INDENT}{}",
                    text::fit(&notice, width.saturating_sub(INDENT.len()))
                ),
                Tone::Accent,
            ));
        }
        if app.star_line() {
            lines.push(Line::default());
            lines.push(single(look, &star_line(width), Tone::Accent));
        }
    }
    lines
}

/// `Enjoying Voisu? Star it on GitHub`, with its two keys when they fit.
fn star_line(width: usize) -> String {
    let room = width.saturating_sub(INDENT.len());
    let line = crate::star_prompt::HOME_LINE;
    let with_keys = format!("{line}  s star · x dismiss");
    if text::width(&with_keys) <= room {
        format!("{INDENT}{with_keys}")
    } else {
        format!("{INDENT}{}", text::fit(line, room))
    }
}

fn hero_lines(app: &App, look: &Look, width: usize) -> Vec<Line<'static>> {
    let room = width.saturating_sub(INDENT.len());
    let fix = |label: &str| format!("↵ {label}");
    let busy = app.service_busy.map(|verb| match verb {
        ServiceVerb::Start => "Starting Voisu…".to_owned(),
        ServiceVerb::Restart => "Restarting Voisu…".to_owned(),
    });
    let (title, title_tone, detail): (String, Tone, String) = match app.hero() {
        Hero::Checking => ("Checking Voisu…".to_owned(), Tone::Bold, String::new()),
        Hero::NeedsRestart => (
            "Voisu needs a restart".to_owned(),
            Tone::Failure,
            busy.unwrap_or_else(|| key_hint(app, Action::Restart)),
        ),
        Hero::NotSetUp => (
            "Voisu isn't set up yet".to_owned(),
            Tone::Caution,
            fix("Run setup"),
        ),
        Hero::NotRunning => (
            "Voisu isn't running".to_owned(),
            Tone::Failure,
            busy.unwrap_or_else(|| fix("Start Voisu")),
        ),
        Hero::Failing {
            label,
            value,
            action,
            ..
        } => (
            match value {
                Some(value) => format!("{label}: {value}"),
                None => label,
            },
            Tone::Failure,
            busy.unwrap_or_else(|| action.unwrap_or_default()),
        ),
        Hero::Recording { seconds } => (
            format!("Recording  {}:{:02}", seconds / 60, seconds % 60),
            Tone::Recording,
            "Press your Trigger Key again to stop.".to_owned(),
        ),
        Hero::Processing => (
            "Working on your Transcript…".to_owned(),
            Tone::Processing,
            String::new(),
        ),
        Hero::Ready => (
            "Ready to dictate".to_owned(),
            Tone::Bold,
            match (busy, app.fix()) {
                (Some(busy), _) => busy,
                (None, Some(Fix::RestartVoisu)) => {
                    format!("{} to apply your changes", fix(Fix::RestartVoisu.label()))
                }
                _ => "Press your Trigger Key, speak, and press it again.".to_owned(),
            },
        ),
    };
    let title_style = title_tone;
    vec![
        Pieces::new(look)
            .add(INDENT, Tone::Plain)
            .add(&text::fit(&title, room), title_style)
            .line(),
        single(
            look,
            &format!("{INDENT}{}", text::fit(&detail, room)),
            Tone::Plain,
        ),
    ]
}

/// How the key table names the key for `action` right now, such as `Ctrl-R Restart Voisu`.
fn key_hint(app: &App, action: Action) -> String {
    keys::bindings(&app.context())
        .into_iter()
        .find(|binding| binding.chords.iter().any(|(_, bound)| *bound == action))
        .map(|binding| format!("{} {}", binding.keys, binding.label))
        .unwrap_or_default()
}

fn summary_row(look: &Look, label: &str, value: &str, tone: Tone, width: usize) -> Line<'static> {
    let lead = format!("{INDENT}{label:<11}");
    let room = width.saturating_sub(text::width(&lead) + 1);
    Pieces::new(look)
        .add(&lead, Tone::Dim)
        .add(&text::fit(value, room), tone)
        .line()
}

fn summary_rows(app: &App, look: &Look, width: usize) -> Vec<Line<'static>> {
    let shown = |row: Option<String>| row.unwrap_or_else(|| "Checking…".to_owned());
    let settings = app.settings;
    let mut rows = vec![summary_row(
        look,
        "Last",
        &last_value(app, width),
        Tone::Plain,
        width,
    )];
    rows.push(summary_row(
        look,
        "Engine",
        &shown(app.engine().map(|mode| engine_label(mode).to_owned())),
        Tone::Plain,
        width,
    ));
    rows.push(summary_row(
        look,
        "Delivery",
        &shown(settings.map(|s| delivery_value(&s).to_owned())),
        Tone::Plain,
        width,
    ));
    rows.push(summary_row(
        look,
        "Writing",
        &shown(settings.map(|s| {
            format!(
                "{} · {} · {}",
                writing_label(s.writing),
                layout_label(s.rendering),
                tone_label(s.tone)
            )
        })),
        Tone::Plain,
        width,
    ));
    let (health, tone) = app
        .health()
        .unwrap_or_else(|| ("Checking…".to_owned(), Tone::Plain));
    rows.push(summary_row(look, "Health", &health, tone, width));
    rows
}

fn last_value(app: &App, width: usize) -> String {
    let Some(newest) = app.history.as_ref().and_then(|items| items.first()) else {
        return if super::daemon_needed(app) {
            "Unknown until Voisu runs".to_owned()
        } else {
            "Nothing yet".to_owned()
        };
    };
    let when = crate::history_view::relative_time(app.now.wall_ms, newest.recorded_at_ms);
    // Indent, label column, the time, its gap, and the two quotes.
    let room = width.saturating_sub(INDENT.len() + 11 + text::width(&when) + 2 + 2 + 1);
    match &newest.transcript {
        Some(transcript) => format!(
            "{when}  \"{}\"",
            text::fit(&text::clean_line(transcript), room)
        ),
        None => format!("{when}  No Transcript"),
    }
}

fn check_rows(app: &App, look: &Look, width: usize, height: usize) -> Vec<Line<'static>> {
    let lines = app.check_lines();
    if lines.is_empty() {
        return vec![single(look, &format!("{INDENT}Checking…"), Tone::Dim)];
    }
    lines
        .iter()
        .skip(app.checks_scroll)
        .take(height)
        .map(|line| match line {
            CheckLine::Row {
                status,
                label,
                value,
            } => {
                let (word, tone) = match status {
                    ReadinessStatus::Fail => ("✕ FAIL", Tone::Failure),
                    ReadinessStatus::Warn => ("! WARN", Tone::Caution),
                    _ => ("✓ PASS", Tone::Success),
                };
                let lead = format!("{label:<16}");
                let room = width.saturating_sub(INDENT.len() + 8 + text::width(&lead) + 1);
                Pieces::new(look)
                    .add(INDENT, Tone::Plain)
                    .add(word, tone)
                    .add("  ", Tone::Plain)
                    .add(&lead, Tone::Plain)
                    .add(&text::fit(value, room), Tone::Dim)
                    .line()
            }
            CheckLine::More(more) => {
                let lead = " ".repeat(CHECK_VALUE_COLUMN);
                let room = width.saturating_sub(lead.len());
                single(look, &format!("{lead}{}", text::fit(more, room)), Tone::Dim)
            }
            CheckLine::Action(action) => {
                let lead = " ".repeat(CHECK_ACTION_COLUMN);
                let room = width.saturating_sub(lead.len());
                single(
                    look,
                    &format!("{lead}{}", text::fit(action, room)),
                    Tone::Plain,
                )
            }
        })
        .collect()
}
