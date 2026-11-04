//! History: the list on top, the selected record's full Transcript below it.

use ratatui::text::Line;

use super::super::look::{Look, Tone};
use super::super::model::{App, HistoryItem};
use super::super::text;
use super::{list_row, single};

const INDENT: &str = "   ";

pub(super) fn lines(app: &App, look: &Look, width: usize) -> Vec<Line<'static>> {
    let Some(items) = app.history.as_ref().filter(|items| !items.is_empty()) else {
        let message = match &app.history_error {
            Some(error) => text::clean_line(error),
            None if app.history.is_some() => {
                "Nothing yet. Your dictations will appear here.".to_owned()
            }
            None if super::daemon_needed(app) => {
                "Voisu isn't running, so your History can't be read yet.".to_owned()
            }
            None => "Checking…".to_owned(),
        };
        return vec![single(look, &format!("{INDENT}{message}"), Tone::Plain)];
    };

    let (list_height, pane_height) = app.history_split();
    let selected = app.history_selected;
    let start = text::window_start(selected, list_height);
    let mut lines = Vec::new();
    for (index, item) in items.iter().enumerate().skip(start).take(list_height) {
        lines.push(list_row(
            look,
            index == selected,
            &summary(app, item),
            width,
        ));
    }
    lines.resize(list_height, Line::default());
    lines.push(Line::default());

    let pane_width = app.detail_width();
    let detail = app.detail_lines(pane_width);
    let tone = match app.selected_record().map(|item| item.transcript.is_some()) {
        Some(true) => Tone::Plain,
        _ => Tone::Failure,
    };
    let mut pane: Vec<Line<'static>> = detail
        .iter()
        .skip(app.detail_scroll)
        .take(pane_height)
        .map(|line| single(look, &format!("{INDENT}{line}"), tone))
        .collect();
    pane.resize(pane_height, Line::default());
    lines.extend(pane);

    if let Some(item) = app.selected_record() {
        let providers = text::fit(&item.providers, pane_width);
        lines.push(single(look, &format!("{INDENT}{providers}"), Tone::Dim));
    }
    lines
}

fn summary(app: &App, item: &HistoryItem) -> String {
    let when = crate::history_view::relative_time(app.now.wall_ms, item.recorded_at_ms);
    match (&item.transcript, &item.failure) {
        (Some(transcript), _) => format!("{when:<9} {}", text::clean_line(transcript)),
        (None, Some(failure)) => format!("{when:<9} ✕ {}", text::clean_line(failure)),
        (None, None) => format!("{when:<9} ✕ No Transcript"),
    }
}
