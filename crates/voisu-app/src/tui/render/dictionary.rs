//! Dictionary: the user's own terms, one per row.

use ratatui::text::Line;

use super::super::look::{Look, Tone};
use super::super::model::App;
use super::super::text;
use super::{list_row, single};

const EMPTY: &str = "Add names and jargon here so Voisu spells them your way.";

pub(super) fn lines(app: &App, look: &Look, width: usize, height: usize) -> Vec<Line<'static>> {
    if let Some(error) = &app.terms_error {
        let message = format!(
            "   {}",
            text::fit(&text::clean_line(error), width.saturating_sub(4))
        );
        return vec![single(look, &message, Tone::Failure)];
    }
    let Some(terms) = &app.terms else {
        return vec![single(look, "   Checking…", Tone::Dim)];
    };
    if terms.is_empty() {
        return vec![single(look, &format!("   {EMPTY}"), Tone::Plain)];
    }
    let start = text::window_start(app.term_selected, height);
    terms
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(index, term)| {
            list_row(
                look,
                index == app.term_selected,
                &text::clean_line(term),
                width,
            )
        })
        .collect()
}
