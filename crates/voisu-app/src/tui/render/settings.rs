//! Settings: one row per setting, grouped under the headings of
//! [`SECTIONS`], and a guide to the selected row: its description, a note on
//! what its value means here (Local model only), and, for a row with values,
//! every value with one plain line and `●` on the one in effect. `↵` turns
//! that same guide into the picker, so what you read is what you pick from.
//!
//! From [`PANEL_FROM`] columns the guide sits in a panel to the right of the
//! rows; narrower, it sits under them. A blank line parts the sections when
//! everything fits; when the list scrolls, the top line names the section the
//! rows below it belong to.

use ratatui::text::{Line, Span};

use super::super::look::{Look, Tone};
use super::super::model::{App, Prompt, SECTIONS, SettingsRow};
use super::super::text;
use super::{Pieces, SELECTED, UNSELECTED, single};

const LABEL_WIDTH: usize = 14;
/// The guide's highlight while picking, and its blank stand-in.
const PICKING: &str = "▌ ";
const NOT_PICKING: &str = "  ";
/// The narrowest terminal that gets the guide as a right-hand panel.
pub(super) const PANEL_FROM: usize = 90;
/// The rows' column in panel mode: marker, label and the longest value
/// ("Unknown until Voisu runs") with a little air. Fixed, so the panel never
/// shifts when a value changes.
const ROWS_WIDTH: usize = 44;
const PANEL_RULE: &str = " │ ";

pub(super) fn lines(app: &App, look: &Look, width: usize, height: usize) -> Vec<Line<'static>> {
    if width >= PANEL_FROM {
        panel_layout(app, look, width, height)
    } else {
        stacked_layout(app, look, width, height)
    }
}

/// Rows, a blank, then the description and guide under them.
fn stacked_layout(app: &App, look: &Look, width: usize, height: usize) -> Vec<Line<'static>> {
    let row = app.selected_setting();
    let indent = UNSELECTED.len();
    let mut below: Vec<Line<'static>> =
        text::wrap(row.description(), width.saturating_sub(indent + 1))
            .into_iter()
            .map(|line| single(look, &format!("{UNSELECTED}{line}"), Tone::Dim))
            .collect();
    if let Some((note, tone)) = app.setting_note(row) {
        below.extend(
            text::wrap(&note, width.saturating_sub(indent + 1))
                .into_iter()
                .map(|line| single(look, &format!("{UNSELECTED}{line}"), tone)),
        );
    }
    below.extend(guide_lines(app, look, row, UNSELECTED, width, false));

    let mut lines = list_lines(app, look, width, height.saturating_sub(below.len()));
    if lines.len() + 1 + below.len() <= height {
        lines.push(Line::default());
    }
    lines.extend(below);
    lines
}

/// Rows on the left; the selected row's name, description and guide on the
/// right, each guide line wrapped rather than cut.
fn panel_layout(app: &App, look: &Look, width: usize, height: usize) -> Vec<Line<'static>> {
    let row = app.selected_setting();
    let panel_width = width.saturating_sub(ROWS_WIDTH + text::width(PANEL_RULE));
    let mut panel = vec![single(look, row.label(), Tone::Bold)];
    panel.extend(
        text::wrap(row.description(), panel_width)
            .into_iter()
            .map(|line| single(look, &line, Tone::Dim)),
    );
    if let Some((note, tone)) = app.setting_note(row) {
        panel.extend(
            text::wrap(&note, panel_width)
                .into_iter()
                .map(|line| single(look, &line, tone)),
        );
    }
    let guide = guide_lines(app, look, row, "", panel_width, true);
    if !guide.is_empty() {
        panel.push(Line::default());
        panel.extend(guide);
    }

    let rows = list_lines(app, look, ROWS_WIDTH, height);
    let tall = rows.len().max(panel.len()).min(height);
    let mut rows = rows.into_iter();
    let mut panel = panel.into_iter();
    (0..tall)
        .map(|_| {
            let mut spans = rows.next().map_or_else(Vec::new, |line| line.spans);
            let used: usize = spans.iter().map(Span::width).sum();
            spans.push(Span::raw(" ".repeat(ROWS_WIDTH.saturating_sub(used))));
            spans.push(Span::styled(look.text(PANEL_RULE), look.style(Tone::Dim)));
            if let Some(line) = panel.next() {
                spans.extend(line.spans);
            }
            Line::from(spans)
        })
        .collect()
}

/// One line of the list: a section's heading, a blank between sections, or
/// the row at that index of [`SettingsRow::ALL`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Item {
    Heading(&'static str),
    Gap,
    Row(usize),
}

/// The list in [`SECTIONS`] order, with a blank line between sections when
/// `gaps`.
fn items(gaps: bool) -> Vec<Item> {
    let mut items = Vec::new();
    let mut index = 0;
    for (number, section) in SECTIONS.iter().enumerate() {
        if gaps && number > 0 {
            items.push(Item::Gap);
        }
        items.push(Item::Heading(section.title));
        for _ in section.rows {
            items.push(Item::Row(index));
            index += 1;
        }
    }
    items
}

/// The headings and rows that fit in `height`, scrolled to keep the selection
/// seen. Rows always line up under one label column.
fn list_lines(app: &App, look: &Look, width: usize, height: usize) -> Vec<Line<'static>> {
    let mut items = items(true);
    if items.len() > height {
        items = self::items(false);
    }
    let selected = app.selected_setting();
    let at = items
        .iter()
        .position(|item| matches!(item, Item::Row(index) if SettingsRow::ALL[*index] == selected))
        .unwrap_or(0);
    let start = text::window_start(at, height);
    let mut shown: Vec<Item> = items.iter().copied().skip(start).take(height).collect();
    // Scrolled into the middle of a section: its heading takes the top line,
    // which is never the selected row's.
    if start > 0
        && start < at
        && let Some(heading) = items[..=start]
            .iter()
            .rev()
            .find(|item| matches!(item, Item::Heading(_)))
    {
        shown[0] = *heading;
    }
    // A heading cut off from all of its rows names nothing; a blank keeps the
    // guide below in place.
    if let Some(last) = shown.last_mut()
        && matches!(last, Item::Heading(_))
    {
        *last = Item::Gap;
    }
    shown
        .into_iter()
        .map(|item| match item {
            Item::Heading(title) => Pieces::new(look)
                .add(" ", Tone::Plain)
                .add(&text::fit(title, width.saturating_sub(1)), Tone::Bold)
                .line(),
            Item::Gap => Line::default(),
            Item::Row(index) => row_line(app, look, width, SettingsRow::ALL[index]),
        })
        .collect()
}

fn row_line(app: &App, look: &Look, width: usize, row: SettingsRow) -> Line<'static> {
    let selected = row == app.selected_setting();
    let marker = if selected { SELECTED } else { UNSELECTED };
    let label_tone = if selected { Tone::Bold } else { Tone::Plain };
    let (value, tone) = app.setting_value(row);
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
    Pieces::new(look)
        .add(marker, label_tone)
        .add(&label, label_tone)
        .add(&text::fit(&value, room), tone)
        .line()
}

/// The row's values, one line each, once the current one is known. `indent`
/// leads every line; `wrap` continues a long explanation under itself instead
/// of cutting it.
fn guide_lines(
    app: &App,
    look: &Look,
    row: SettingsRow,
    indent: &str,
    width: usize,
    wrap: bool,
) -> Vec<Line<'static>> {
    let choices = row.choices();
    if app.current_choice(row).is_none() {
        return Vec::new();
    }
    let current = app.choice_in_effect(row);
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
    let mut lines = Vec::new();
    for (index, choice) in choices.iter().enumerate() {
        let picking = picked == Some(index);
        let lead = format!(
            "{indent}{}{} ",
            if picking { PICKING } else { NOT_PICKING },
            if current == Some(index) { "●" } else { " " },
        );
        let label = format!("{:<label_width$}", choice.label);
        let hang = text::width(&lead) + label_width;
        let room = width.saturating_sub(hang + 1).max(1);
        let label_tone = match (picking, current == Some(index)) {
            (true, _) => Tone::Accent,
            (false, true) => Tone::Bold,
            (false, false) => Tone::Plain,
        };
        let explanation = if wrap {
            text::wrap(choice.line, room)
        } else {
            vec![text::fit(choice.line, room)]
        };
        for (part, words) in explanation.iter().enumerate() {
            let line = if part == 0 {
                Pieces::new(look)
                    .add(&lead, if picking { Tone::Accent } else { Tone::Plain })
                    .add(&label, label_tone)
            } else {
                Pieces::new(look).add(&" ".repeat(hang), Tone::Plain)
            };
            lines.push(line.add(words, Tone::Dim).line());
        }
    }
    lines
}
