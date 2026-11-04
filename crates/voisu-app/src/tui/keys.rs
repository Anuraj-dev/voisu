//! The one key table. Dispatch, the footer and the `?` overlay all read it, so
//! a key that is advertised is a key that works.

use std::borrow::Cow;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::model::Tab;

/// The footer never shows more than this many hints.
pub const FOOTER_LIMIT: usize = 5;

/// What the user is looking at, which decides what keys mean.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Context {
    /// Voisu may be dictating, possibly into this very terminal: only Ctrl
    /// chords act, since a Transcript cannot type one. `restart` offers
    /// `Ctrl-R` for a daemon that answers but cannot be read.
    Guarded {
        restart: bool,
    },
    TooSmall,
    AddTermPrompt,
    KeyPrompt,
    ConfirmPrompt,
    /// Picking one value of a setting, inline under its row.
    ChoicePrompt,
    Overlay,
    Home {
        fix: Option<&'static str>,
        checks: bool,
        /// `r` restarts Voisu: nothing else is starting or restarting it.
        restart: bool,
    },
    History {
        has_records: bool,
    },
    Dictionary {
        has_terms: bool,
    },
    Settings {
        enter: Option<&'static str>,
        /// `r` restarts Voisu: nothing else is starting or restarting it.
        restart: bool,
    },
    /// A setup question with a few answers.
    SetupChoice,
    /// A key field on the setup keys screen. Letters are the key being typed.
    SetupKey {
        enter: Option<&'static str>,
        /// A key is stored, and `→` keeps it.
        keep: bool,
        /// The field takes typing, so `Ctrl+V` can fill it.
        editable: bool,
    },
    /// Setup is working and there is nothing to answer.
    SetupBusy,
    /// Setup is waiting for the desktop's own window.
    SetupWaiting,
    SetupInstallFailed {
        esc: &'static str,
    },
    /// Setup has ended, one way or the other.
    SetupDone,
}

impl Context {
    pub fn is_setup(&self) -> bool {
        matches!(
            self,
            Context::SetupChoice
                | Context::SetupKey { .. }
                | Context::SetupBusy
                | Context::SetupWaiting
                | Context::SetupInstallFailed { .. }
                | Context::SetupDone
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Quit,
    PrevTab,
    NextTab,
    GoTab(Tab),
    Up,
    Down,
    PageUp,
    PageDown,
    Activate,
    Restart,
    ToggleChecks,
    Copy,
    AddTerm,
    AskRemove,
    OpenHelp,
    Close,
    Submit,
    ConfirmRemove,
    /// Keep the stored key.
    Keep,
    /// Read a key from the clipboard.
    PasteClipboard,
}

/// One row of the table: how it reads, and which keys do what.
#[derive(Clone, Debug)]
pub struct Binding {
    /// Short form for the footer, such as `←→`.
    pub keys: &'static str,
    /// Every key that works, for the `?` overlay.
    pub all_keys: &'static str,
    pub label: Cow<'static, str>,
    pub in_footer: bool,
    pub chords: Vec<(KeyEvent, Action)>,
}

fn plain(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn letter(ch: char) -> KeyEvent {
    plain(KeyCode::Char(ch))
}

fn ctrl(ch: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL)
}

fn ctrl_c() -> KeyEvent {
    ctrl('c')
}

fn tabs() -> Binding {
    Binding {
        keys: "←→",
        all_keys: "← → Tab Shift-Tab h l 1-4",
        label: "tabs".into(),
        in_footer: true,
        chords: vec![
            (plain(KeyCode::Left), Action::PrevTab),
            (plain(KeyCode::Right), Action::NextTab),
            (plain(KeyCode::Tab), Action::NextTab),
            (plain(KeyCode::BackTab), Action::PrevTab),
            (letter('h'), Action::PrevTab),
            (letter('l'), Action::NextTab),
            (letter('1'), Action::GoTab(Tab::Home)),
            (letter('2'), Action::GoTab(Tab::History)),
            (letter('3'), Action::GoTab(Tab::Dictionary)),
            (letter('4'), Action::GoTab(Tab::Settings)),
        ],
    }
}

fn moves(label: &'static str, in_footer: bool) -> Binding {
    Binding {
        keys: "↑↓",
        all_keys: "↑ ↓ j k",
        label: label.into(),
        in_footer,
        chords: vec![
            (plain(KeyCode::Up), Action::Up),
            (plain(KeyCode::Down), Action::Down),
            (letter('k'), Action::Up),
            (letter('j'), Action::Down),
        ],
    }
}

fn help() -> Binding {
    Binding {
        keys: "?",
        all_keys: "?",
        label: "help".into(),
        in_footer: true,
        chords: vec![(letter('?'), Action::OpenHelp)],
    }
}

fn quit() -> Binding {
    Binding {
        keys: "q",
        all_keys: "q Ctrl-C",
        label: "quit".into(),
        in_footer: true,
        chords: vec![(letter('q'), Action::Quit), (ctrl_c(), Action::Quit)],
    }
}

/// `Ctrl-C` alone, for every context where `q` is ordinary text or must not act.
fn interrupt(in_footer: bool) -> Binding {
    Binding {
        keys: "Ctrl-C",
        all_keys: "Ctrl-C",
        label: "quit".into(),
        in_footer,
        chords: vec![(ctrl_c(), Action::Quit)],
    }
}

/// `Ctrl-C` while setup is at work: it stops the work as well as the interface.
fn stop() -> Binding {
    Binding {
        label: "stop".into(),
        ..interrupt(true)
    }
}

fn enter(label: impl Into<Cow<'static, str>>, action: Action) -> Binding {
    Binding {
        keys: "↵",
        all_keys: "↵",
        label: label.into(),
        in_footer: true,
        chords: vec![(plain(KeyCode::Enter), action)],
    }
}

fn simple(key: char, keys: &'static str, label: &'static str, action: Action) -> Binding {
    Binding {
        keys,
        all_keys: keys,
        label: label.into(),
        in_footer: true,
        chords: vec![(letter(key), action)],
    }
}

/// `r` restarts Voisu and its Overlay, whether or not a change is waiting.
fn restart(in_footer: bool) -> Binding {
    Binding {
        in_footer,
        ..simple('r', "r", "restart", Action::Restart)
    }
}

fn close(label: &'static str) -> Binding {
    Binding {
        keys: "Esc",
        all_keys: "Esc",
        label: label.into(),
        in_footer: true,
        chords: vec![(plain(KeyCode::Esc), Action::Close)],
    }
}

/// The table for one context, in footer order.
pub fn bindings(context: &Context) -> Vec<Binding> {
    match context {
        Context::Guarded { restart } => {
            let mut rows = Vec::new();
            if *restart {
                rows.push(Binding {
                    keys: "Ctrl-R",
                    all_keys: "Ctrl-R",
                    label: "Restart Voisu".into(),
                    in_footer: true,
                    chords: vec![(ctrl('r'), Action::Restart)],
                });
            }
            rows.push(interrupt(true));
            rows
        }
        Context::TooSmall => vec![quit()],
        Context::AddTermPrompt => vec![
            enter("add", Action::Submit),
            close("cancel"),
            interrupt(false),
        ],
        Context::KeyPrompt => vec![
            enter("check and save", Action::Submit),
            close("cancel"),
            interrupt(false),
        ],
        Context::ChoicePrompt => vec![
            moves("pick", true),
            enter("choose", Action::Submit),
            close("cancel"),
            interrupt(false),
        ],
        Context::ConfirmPrompt => vec![
            simple('y', "y", "remove", Action::ConfirmRemove),
            Binding {
                keys: "n",
                all_keys: "n Esc",
                label: "keep".into(),
                in_footer: true,
                chords: vec![
                    (letter('n'), Action::Close),
                    (plain(KeyCode::Esc), Action::Close),
                ],
            },
            interrupt(false),
        ],
        Context::Overlay => {
            let mut closing = close("close");
            closing.all_keys = "Esc ?";
            closing.chords.push((letter('?'), Action::Close));
            vec![closing, quit()]
        }
        Context::Home {
            fix,
            checks,
            restart: can_restart,
        } => {
            let mut rows = Vec::new();
            if let Some(fix) = fix {
                rows.push(enter(*fix, Action::Activate));
            }
            if *checks {
                // Every row fits while nothing needs fixing; the fix outranks
                // scrolling for the footer, and `?` still lists both.
                rows.push(moves("scroll", fix.is_none()));
                rows.push(simple('d', "d", "summary", Action::ToggleChecks));
                rows.push(tabs());
            } else {
                rows.push(tabs());
                rows.push(simple('d', "d", "checks", Action::ToggleChecks));
            }
            if *can_restart {
                // Five fit: a fix or the scroll hint outranks it; `?` lists it.
                rows.push(restart(fix.is_none() && !*checks));
            }
            rows.push(help());
            rows.push(quit());
            rows
        }
        Context::History { has_records } => {
            let mut rows = Vec::new();
            if *has_records {
                rows.push(moves("move", true));
                rows.push(simple('c', "c", "copy", Action::Copy));
                rows.push(Binding {
                    keys: "PgUp PgDn",
                    all_keys: "PgUp PgDn",
                    label: "read a long Transcript".into(),
                    in_footer: false,
                    chords: vec![
                        (plain(KeyCode::PageUp), Action::PageUp),
                        (plain(KeyCode::PageDown), Action::PageDown),
                    ],
                });
            }
            rows.extend([tabs(), help(), quit()]);
            rows
        }
        Context::Dictionary { has_terms } => {
            let mut rows = vec![simple('a', "a", "add", Action::AddTerm)];
            if *has_terms {
                rows.push(simple('d', "d", "remove", Action::AskRemove));
                rows.push(moves("move", false));
            }
            rows.extend([tabs(), help(), quit()]);
            rows
        }
        Context::Settings {
            enter: label,
            restart: can_restart,
        } => {
            // Five fit: the row's own action outranks the move hint, which the
            // marker already suggests and `?` still lists.
            let mut rows = vec![moves("move", label.is_none() || !*can_restart)];
            if let Some(label) = label {
                rows.push(enter(*label, Action::Activate));
            }
            if *can_restart {
                rows.push(restart(true));
            }
            rows.extend([tabs(), help(), quit()]);
            rows
        }
        Context::SetupChoice => vec![
            moves("pick", true),
            enter("choose", Action::Submit),
            close("leave setup"),
            interrupt(false),
        ],
        Context::SetupKey {
            enter: label,
            keep,
            editable,
        } => {
            // Arrows only: every letter here is part of a key.
            let mut rows = vec![Binding {
                keys: "↑↓",
                all_keys: "↑ ↓",
                label: "move".into(),
                in_footer: true,
                chords: vec![
                    (plain(KeyCode::Up), Action::Up),
                    (plain(KeyCode::Down), Action::Down),
                ],
            }];
            if *editable {
                rows.push(Binding {
                    keys: "Ctrl+V",
                    all_keys: "Ctrl+V",
                    label: "paste".into(),
                    // Five hints don't fit the narrowest screen, and replacing a
                    // key is the rarer case: the key still works.
                    in_footer: !*keep,
                    chords: vec![(ctrl('v'), Action::PasteClipboard)],
                });
            }
            if let Some(label) = label {
                rows.push(enter(*label, Action::Submit));
            }
            if *keep {
                rows.push(Binding {
                    keys: "→",
                    all_keys: "→",
                    label: "keep".into(),
                    in_footer: true,
                    chords: vec![(plain(KeyCode::Right), Action::Keep)],
                });
            }
            rows.push(close("leave setup"));
            rows.push(interrupt(false));
            rows
        }
        Context::SetupBusy => vec![stop()],
        Context::SetupWaiting => vec![close("later"), stop()],
        Context::SetupInstallFailed { esc } => vec![
            enter("try again", Action::Submit),
            close(esc),
            interrupt(false),
        ],
        Context::SetupDone => vec![enter("open Voisu", Action::Submit), quit()],
    }
}

/// The hints a context's footer shows.
pub fn footer(context: &Context) -> Vec<Binding> {
    bindings(context)
        .into_iter()
        .filter(|binding| binding.in_footer)
        .take(FOOTER_LIMIT)
        .collect()
}

/// What `key` does in this context, if anything. Shift is ignored because a
/// terminal may report `?` or `BackTab` with or without it.
pub fn action_for(context: &Context, key: KeyEvent) -> Option<Action> {
    let wanted = key.modifiers - KeyModifiers::SHIFT;
    bindings(context)
        .into_iter()
        .flat_map(|binding| binding.chords)
        .find(|(chord, _)| chord.code == key.code && chord.modifiers == wanted)
        .map(|(_, action)| action)
}

#[cfg(test)]
pub(super) fn every_context() -> Vec<Context> {
    let mut contexts = vec![
        Context::Guarded { restart: false },
        Context::Guarded { restart: true },
        Context::TooSmall,
        Context::AddTermPrompt,
        Context::KeyPrompt,
        Context::ConfirmPrompt,
        Context::ChoicePrompt,
        Context::Overlay,
        Context::SetupChoice,
        Context::SetupBusy,
        Context::SetupWaiting,
        Context::SetupInstallFailed { esc: "use Cloud" },
        Context::SetupDone,
    ];
    for enter in [None, Some("check"), Some("replace"), Some("save it")] {
        for (keep, editable) in [(false, false), (false, true), (true, true)] {
            contexts.push(Context::SetupKey {
                enter,
                keep,
                editable,
            });
        }
    }
    for fix in [None, Some("Run setup"), Some("Restart Voisu")] {
        for checks in [false, true] {
            for restart in [false, true] {
                contexts.push(Context::Home {
                    fix,
                    checks,
                    restart,
                });
            }
        }
    }
    for flag in [false, true] {
        contexts.push(Context::History { has_records: flag });
        contexts.push(Context::Dictionary { has_terms: flag });
    }
    for enter in [None, Some("change"), Some("enter key"), Some("run setup")] {
        for restart in [false, true] {
            contexts.push(Context::Settings { enter, restart });
        }
    }
    contexts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_footer_shows_more_than_five_hints() {
        for context in every_context() {
            let declared = bindings(&context)
                .iter()
                .filter(|binding| binding.in_footer)
                .count();
            assert!(declared <= FOOTER_LIMIT, "{context:?} declares {declared}");
        }
    }

    #[test]
    fn every_hint_in_the_footer_is_in_the_overlay_and_really_works() {
        for context in every_context() {
            let overlay = bindings(&context);
            for hint in footer(&context) {
                let row = overlay
                    .iter()
                    .find(|row| row.keys == hint.keys && row.label == hint.label)
                    .unwrap_or_else(|| panic!("{context:?} footer {} missing", hint.keys));
                assert!(!row.chords.is_empty());
                for (chord, action) in &row.chords {
                    assert_eq!(action_for(&context, *chord), Some(*action), "{context:?}");
                }
            }
        }
    }

    #[test]
    fn the_reference_home_footer_reads_as_specified() {
        let hints: Vec<String> = footer(&Context::Home {
            fix: None,
            checks: false,
            restart: true,
        })
        .iter()
        .map(|hint| format!("{} {}", hint.keys, hint.label))
        .collect();
        assert_eq!(
            hints,
            ["←→ tabs", "d checks", "r restart", "? help", "q quit"]
        );
    }

    #[test]
    fn r_restarts_from_home_and_settings_whenever_nothing_else_is() {
        for enter in [None, Some("change")] {
            let context = Context::Settings {
                enter,
                restart: true,
            };
            assert_eq!(action_for(&context, letter('r')), Some(Action::Restart));
            assert!(
                footer(&context).iter().any(|hint| hint.label == "restart"),
                "{context:?}"
            );
        }
        for fix in [None, Some("Restart Voisu")] {
            for checks in [false, true] {
                let context = Context::Home {
                    fix,
                    checks,
                    restart: true,
                };
                assert_eq!(action_for(&context, letter('r')), Some(Action::Restart));
            }
        }
        let busy = Context::Settings {
            enter: Some("change"),
            restart: false,
        };
        assert_eq!(action_for(&busy, letter('r')), None, "one at a time");
    }

    #[test]
    fn escape_never_quits_anywhere() {
        for context in every_context() {
            assert_ne!(
                action_for(&context, plain(KeyCode::Esc)),
                Some(Action::Quit),
                "{context:?}"
            );
        }
    }

    #[test]
    fn ctrl_c_quits_everywhere() {
        for context in every_context() {
            assert_eq!(
                action_for(&context, ctrl_c()),
                Some(Action::Quit),
                "{context:?}"
            );
        }
    }

    #[test]
    fn shift_is_ignored_when_matching() {
        let context = Context::History { has_records: false };
        let shifted = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT);
        assert_eq!(action_for(&context, shifted), Some(Action::OpenHelp));
        let back = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(action_for(&context, back), Some(Action::PrevTab));
    }

    #[test]
    fn typing_in_a_prompt_is_never_captured_as_a_command() {
        let setup_key = Context::SetupKey {
            enter: Some("check"),
            keep: true,
            editable: true,
        };
        for context in [Context::AddTermPrompt, Context::KeyPrompt, setup_key] {
            for ch in "qhjklr?acd1234".chars() {
                assert_eq!(action_for(&context, letter(ch)), None, "{context:?} {ch}");
            }
        }
    }
}
