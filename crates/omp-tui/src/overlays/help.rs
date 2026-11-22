//! Help / keyhints overlay (`?`, `/help`, `/hotkeys`) — port of
//! `pi-tui/src/hotkeys-markdown.ts` (`hotkeyGroups` + `HotkeysSheetComponent`).
//!
//! The sheet is built from the effective [`KeybindingsManager`], so user
//! overrides from `keybindings.yml` show up and an unbound action reads
//! `Disabled`, with keys formatted through the theme's `key.*` symbols (TS
//! `formatKeyHint`). It lists the TS groups (Navigation, Editing, Other) but
//! only the shortcuts omp-rs implements: rows for TS actions that have no
//! omp-rs behavior yet (model cycling, history search, external editor, …)
//! are left out rather than advertised. Close keys: `?`, `q`, `esc`, `ctrl+c`.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::component::{Component, EventOutcome};
use crate::keybindings::{format_key_hint, KeybindingsManager};
use crate::keys::{KeyCode, KeyEvent, KeyModifiers};
use crate::theme::Theme;

/// One row of the sheet: a key list plus what it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyRow {
    /// Displayed keys, already formatted (`⌃C`, `Alt+↑/⇧↑`, `Disabled`).
    pub keys: String,
    /// The action label.
    pub action: String,
}

/// A titled section of the sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyGroup {
    /// Group title, e.g. `"Navigation"`.
    pub title: String,
    /// Rows in display order.
    pub rows: Vec<HotkeyRow>,
}

/// The sheet's groups from the effective bindings (TS `hotkeyGroups`, rows
/// limited to what omp-rs implements). `act` rows follow the user's
/// `keybindings.yml`; `fixed` rows are editor chords omp-rs hard-wires.
#[must_use]
pub fn hotkey_groups(keys: &KeybindingsManager, theme: &Theme) -> Vec<HotkeyGroup> {
    let fixed = |ids: &[&str]| {
        ids.iter()
            .map(|k| format_key_hint(k, theme))
            .collect::<Vec<_>>()
            .join("/")
    };
    let act = |id: &str| {
        let shown = keys.display_string(id, theme);
        if shown.is_empty() {
            "Disabled".to_owned()
        } else {
            shown
        }
    };
    let row = |keys: String, action: &str| HotkeyRow {
        keys,
        action: action.to_owned(),
    };
    vec![
        HotkeyGroup {
            title: "Navigation".into(),
            rows: vec![
                row(
                    fixed(&["up", "down", "left", "right"]),
                    &format!(
                        "Move cursor / browse history ({} when empty)",
                        format_key_hint("up", theme)
                    ),
                ),
                row(fixed(&["alt+left", "alt+right"]), "Move by word"),
                row(fixed(&["ctrl+a", "home"]), "Start of line"),
                row(fixed(&["ctrl+e", "end"]), "End of line"),
            ],
        },
        HotkeyGroup {
            title: "Editing".into(),
            rows: vec![
                row(fixed(&["enter"]), "Send message"),
                row(fixed(&["shift+enter", "ctrl+j"]), "New line"),
                row(
                    fixed(&["ctrl+enter", "ctrl+q"]),
                    "Send as a follow-up (queued while the agent works)",
                ),
                row(fixed(&["ctrl+w", "alt+backspace"]), "Delete word backwards"),
                row(fixed(&["ctrl+u"]), "Delete to start of line"),
                row(fixed(&["ctrl+k"]), "Delete to end of line"),
                row(fixed(&["ctrl+-"]), "Undo"),
            ],
        },
        HotkeyGroup {
            title: "Other".into(),
            rows: vec![
                row(fixed(&["tab"]), "Path completion / accept autocomplete"),
                row(
                    act("app.interrupt"),
                    "Cancel autocomplete / interrupt active work",
                ),
                row(
                    format!("{0} {0}", fixed(&["escape"])),
                    "Rewind to an earlier prompt (empty editor)",
                ),
                row(act("app.clear"), "Clear editor (first) / exit (second)"),
                row(act("app.exit"), "Exit"),
                row(act("app.thinking.cycle"), "Cycle thinking level"),
                row(
                    act("app.message.dequeue"),
                    "Restore the last queued message to the editor",
                ),
                row(act("app.tools.expand"), "Toggle tool output expansion"),
                row(act("app.ask.hide"), "Hide/show a pending dialog"),
                row(act("app.approval.approve"), "Approve a pending tool call"),
                row(
                    act("app.clipboard.pasteImage"),
                    "Paste image or text from clipboard",
                ),
                row(
                    act("app.clipboard.pasteTextRaw"),
                    "Paste clipboard text raw (no collapse)",
                ),
                row(act("app.session.tree"), "Show the session tree (/tree)"),
                row("?".into(), "Toggle this help (empty editor)"),
                row("/".into(), "Slash commands"),
            ],
        },
    ]
}

/// Widest keys column before key lists are clipped.
const MAX_KEYS_COLS: usize = 28;

fn clip_width(s: &str, width: usize) -> String {
    if UnicodeWidthStr::width(s) <= width {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// The help/keyhints overlay.
#[derive(Debug, Clone)]
pub struct Help {
    groups: Vec<HotkeyGroup>,
    /// Index of the first visible row (flat across groups).
    scroll: usize,
}

impl Default for Help {
    fn default() -> Self {
        Self {
            groups: hotkey_groups(&KeybindingsManager::defaults(), &Theme::default()),
            scroll: 0,
        }
    }
}

impl Help {
    /// Rebuild from the effective bindings and theme (opening the sheet).
    pub fn load(&mut self, keys: &KeybindingsManager, theme: &Theme) {
        self.groups = hotkey_groups(keys, theme);
        self.scroll = 0;
    }

    /// The sheet's groups.
    #[must_use]
    pub fn groups(&self) -> &[HotkeyGroup] {
        &self.groups
    }

    /// Flatten groups into `(is_header, keys, action)` display rows, one blank
    /// row between groups.
    fn flat_rows(&self) -> Vec<(bool, &str, &str)> {
        let mut out = Vec::new();
        for (i, group) in self.groups.iter().enumerate() {
            if i > 0 {
                out.push((false, "", ""));
            }
            out.push((true, "", group.title.as_str()));
            for row in &group.rows {
                out.push((false, row.keys.as_str(), row.action.as_str()));
            }
        }
        out
    }

    /// Width of the keys column: the widest key list (TS `GitHelpSheet` rule),
    /// capped so a long override list cannot push every action off screen.
    fn keys_width(&self) -> usize {
        self.groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .map(|r| UnicodeWidthStr::width(r.keys.as_str()))
            .max()
            .unwrap_or(0)
            .clamp(3, MAX_KEYS_COLS)
    }
}

impl Component for Help {
    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let popup = crate::overlays::centered(area, 80, 80);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .title(" Keyboard shortcuts ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.color("border")));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);

        let keys_w = self.keys_width();
        let rows = self.flat_rows();
        let visible = usize::from(inner.height);
        let pad_keys = |keys: &str| {
            let shown = clip_width(keys, keys_w);
            let w = UnicodeWidthStr::width(shown.as_str());
            format!("{}{shown}  ", " ".repeat(keys_w.saturating_sub(w)))
        };
        let window: Vec<Line<'_>> = rows
            .iter()
            .skip(self.scroll)
            .take(visible.saturating_sub(1))
            .map(|(header, keys, action)| {
                if *header {
                    Line::from(Span::styled(
                        action.to_string(),
                        Style::default()
                            .fg(theme.color("accent"))
                            .add_modifier(Modifier::BOLD),
                    ))
                } else {
                    Line::from(vec![
                        Span::styled(pad_keys(keys), Style::default().fg(theme.color("text"))),
                        Span::styled(
                            (*action).to_string(),
                            Style::default().fg(theme.color("text_muted")),
                        ),
                    ])
                }
            })
            .chain(std::iter::once(Line::from(Span::styled(
                format!(
                    "{}/{} scroll · {} / ? / q close",
                    format_key_hint("up", theme),
                    format_key_hint("down", theme),
                    format_key_hint("escape", theme)
                ),
                Style::default().fg(theme.color("text_muted")),
            ))))
            .collect();
        frame.render_widget(Paragraph::new(window), inner);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventOutcome {
        let rows = self.flat_rows().len();
        let page = 10usize;
        match key.code {
            // Close keys (`?`/q/esc/ctrl+c per the TS sheet): intentionally
            // ignored so they bubble to the host, which owns open/close
            // state. Esc/q/? are unmodified; `c` requires ctrl.
            KeyCode::Esc | KeyCode::Char('q' | '?') => EventOutcome::Ignored,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                EventOutcome::Ignored
            }
            KeyCode::Up => {
                self.scroll = self.scroll.saturating_sub(1);
                EventOutcome::Consumed
            }
            KeyCode::Down => {
                self.scroll = (self.scroll + 1).min(rows.saturating_sub(1));
                EventOutcome::Consumed
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(page);
                EventOutcome::Consumed
            }
            KeyCode::PageDown => {
                self.scroll = (self.scroll + page).min(rows.saturating_sub(1));
                EventOutcome::Consumed
            }
            _ => EventOutcome::Ignored,
        }
    }

    fn wants_focus(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::UserBindings;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_help(help: &Help, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| help.render(f, f.area(), &Theme::default()))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    fn row<'a>(groups: &'a [HotkeyGroup], action: &str) -> &'a HotkeyRow {
        groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .find(|r| r.action == action)
            .unwrap_or_else(|| panic!("no row {action:?}"))
    }

    #[test]
    fn renders_bordered_sheet_with_groups() {
        let rows = render_help(&Help::default(), 100, 40);
        let body = rows.join("\n");
        assert!(body.contains("Keyboard shortcuts"), "title in {body}");
        for title in ["Navigation", "Editing", "Other"] {
            assert!(body.contains(title), "{title} in {body}");
        }
        assert!(body.contains("Send message"), "action column");
        assert!(body.contains("close"), "footer");
        assert!(
            rows.iter().any(|r| r.contains('┌')),
            "border drawn: {rows:?}"
        );
    }

    #[test]
    fn rows_follow_the_effective_bindings() {
        let theme = Theme::default();
        let defaults = hotkey_groups(&KeybindingsManager::defaults(), &theme);
        assert_eq!(
            row(&defaults, "Cycle thinking level").keys,
            format_key_hint("shift+tab", &theme)
        );
        // `app.session.tree` ships unbound (TS default): the row says so.
        assert_eq!(
            row(&defaults, "Show the session tree (/tree)").keys,
            "Disabled"
        );
        let mut user = UserBindings::new();
        user.insert(
            "app.thinking.cycle".into(),
            vec!["alt+t".into(), "f2".into()],
        );
        user.insert("app.tools.expand".into(), Vec::new());
        let custom = hotkey_groups(&KeybindingsManager::new(user), &theme);
        assert_eq!(
            row(&custom, "Cycle thinking level").keys,
            format!("{}/F2", format_key_hint("alt+t", &theme))
        );
        assert_eq!(
            row(&custom, "Toggle tool output expansion").keys,
            "Disabled"
        );
    }

    #[test]
    fn no_row_advertises_an_unimplemented_shortcut() {
        let groups = hotkey_groups(&KeybindingsManager::defaults(), &Theme::default());
        let actions: Vec<&str> = groups
            .iter()
            .flat_map(|g| g.rows.iter())
            .map(|r| r.action.as_str())
            .collect();
        for missing in [
            "Search prompt history",
            "Edit message in external editor",
            "Toggle plan mode",
            "Run bash command",
            "command palette",
        ] {
            assert!(
                !actions.iter().any(|a| a.contains(missing)),
                "{missing} is not an omp-rs shortcut"
            );
        }
    }

    #[test]
    fn scrolling_moves_window() {
        let mut help = Help::default();
        let before = render_help(&help, 80, 12).join("\n");
        help.handle_key(KeyEvent::from(KeyCode::PageDown));
        let after = render_help(&help, 80, 12).join("\n");
        assert_ne!(before, after);
        help.handle_key(KeyEvent::from(KeyCode::PageUp));
        let back = render_help(&help, 80, 12).join("\n");
        assert_eq!(before, back);
    }

    #[test]
    fn arrows_scroll_and_other_keys_bubble() {
        let mut help = Help::default();
        assert!(help.handle_key(KeyEvent::from(KeyCode::Down)).consumed());
        assert_eq!(help.scroll, 1);
        assert!(!help
            .handle_key(KeyEvent::from(KeyCode::Char('x')))
            .consumed());
        assert!(!help.handle_key(KeyEvent::from(KeyCode::Esc)).consumed());
    }
}
