//! Settings: one row per setting; the selected row's description is always shown.

use ratatui::text::Line;

use super::super::look::{Look, Tone};
use super::super::model::{App, SettingsRow};
use super::super::text;
use super::{Pieces, SELECTED, UNSELECTED, single, value_tone};

const LABEL_WIDTH: usize = 14;

pub(super) fn lines(app: &App, look: &Look, width: usize, height: usize) -> Vec<Line<'static>> {
    let delivery = app.settings.map(|settings| settings.delivery);
    let description = text::wrap(
        app.selected_setting().description(delivery),
        width.saturating_sub(UNSELECTED.len() + 1),
    );
    let rows_height = SettingsRow::ALL
        .len()
        .min(height.saturating_sub(description.len()));
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
        let tone = if selected && tone == super::ValueTone::Plain {
            Tone::Bold
        } else {
            value_tone(tone)
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
    if lines.len() + 1 + description.len() <= height {
        lines.push(Line::default());
    }
    for line in description {
        lines.push(single(look, &format!("{UNSELECTED}{line}"), Tone::Dim));
    }
    lines
}
