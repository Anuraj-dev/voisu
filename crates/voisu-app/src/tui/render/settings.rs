//! Settings: one row per setting. Under the rows, the selected row's
//! description and, for a row with values, its guide: every value with one
//! plain line and `●` on the one in effect. `↵` turns that same guide into the
//! picker, so what you read is what you pick from.

use ratatui::text::Line;

use super::super::look::{Look, Tone};
use super::super::model::{App, Prompt, SettingsRow};
use super::super::text;
use super::{Pieces, SELECTED, UNSELECTED, single};

const LABEL_WIDTH: usize = 14;
/// The guide's highlight while picking, and its blank stand-in.
const PICKING: &str = "▌ ";
const NOT_PICKING: &str = "  ";

pub(super) fn lines(app: &App, look: &Look, width: usize, height: usize) -> Vec<Line<'static>> {
    let row = app.selected_setting();
    let mut below: Vec<Line<'static>> = text::wrap(
        row.description(),
        width.saturating_sub(UNSELECTED.len() + 1),
    )
    .into_iter()
    .map(|line| single(look, &format!("{UNSELECTED}{line}"), Tone::Dim))
    .collect();
    below.extend(guide_lines(app, look, row, width));

    let rows_height = SettingsRow::ALL
        .len()
        .min(height.saturating_sub(below.len()));
    let start = text::window_start(app.settings_selected, rows_height);

    let mut lines = Vec::new();
    for (index, row) in SettingsRow::ALL
        .iter()
        .enumerate()
        .skip(start)
        .take(rows_height)
    {
        let selected = index == app.settings_selected;
        let marker = if selected { SELECTED } else { UNSELECTED };
        let label_tone = if selected { Tone::Bold } else { Tone::Plain };
        let (value, tone) = app.setting_value(*row);
        let room = width.saturating_sub(UNSELECTED.len() + LABEL_WIDTH + 1);
        let tone = if selected && tone == Tone::Plain {
            Tone::Accent
        } else {
            tone
        };
        let label = if value.is_empty() {
            row.label().to_owned()
        } else {
            format!("{:<LABEL_WIDTH$}", row.label())
        };
        lines.push(
            Pieces::new(look)
                .add(marker, label_tone)
                .add(&label, label_tone)
                .add(&text::fit(&value, room), tone)
                .line(),
        );
    }
    if lines.len() + 1 + below.len() <= height {
        lines.push(Line::default());
    }
    lines.extend(below);
    lines
}

/// The row's values, one line each, once the current one is known.
fn guide_lines(app: &App, look: &Look, row: SettingsRow, width: usize) -> Vec<Line<'static>> {
    let choices = row.choices();
    let Some(current) = app.current_choice(row) else {
        return Vec::new();
    };
    let picked = match app.prompt {
        Some(Prompt::Choice { row: open, picked }) if open == row => Some(picked),
        _ => None,
    };
    let label_width = choices
        .iter()
        .map(|c| text::width(c.label))
        .max()
        .unwrap_or(0)
        + 2;
    choices
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            let picking = picked == Some(index);
            let lead = format!(
                "{UNSELECTED}{}{} ",
                if picking { PICKING } else { NOT_PICKING },
                if index == current { "●" } else { " " },
            );
            let label = format!("{:<label_width$}", choice.label);
            let room = width.saturating_sub(text::width(&lead) + label_width + 1);
            let label_tone = match (picking, index == current) {
                (true, _) => Tone::Accent,
                (false, true) => Tone::Bold,
                (false, false) => Tone::Plain,
            };
            Pieces::new(look)
                .add(&lead, if picking { Tone::Accent } else { Tone::Plain })
                .add(&label, label_tone)
                .add(&text::fit(choice.line, room), Tone::Dim)
                .line()
        })
        .collect()
}
