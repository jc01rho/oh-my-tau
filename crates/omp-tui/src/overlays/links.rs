//! Extension link picker — the `app.links.hint` overlay (TS
//! `modes/interactive-mode.ts` `startLinkHints` / `openLinkBrowser`).
//!
//! Links whose scheme an extension registered with `pi.registerLinkScheme`
//! cannot be opened by the terminal, so the user picks one here and the app
//! raises `link_activate` on the owning extension. Each row carries a hint tag
//! (a, b, … z, aa, …); typing the tag — or Enter on the selected row —
//! activates that link, Esc cancels.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::component::{Component, EventOutcome};
use crate::keys::{KeyCode, KeyEvent};
use crate::theme::Theme;

/// One pickable link: its hint tag and the URL to activate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRow {
    /// Hint tag typed to pick this row (`a`..`z`, then `aa`..).
    pub tag: String,
    /// The URL handed to the owning extension.
    pub url: String,
}

/// The picker panel state.
#[derive(Debug, Clone, Default)]
pub struct LinkHints {
    links: Vec<LinkRow>,
    selected: usize,
    picked: Option<String>,
}

/// The hint tag for row `index`: `a`..`z`, then `aa`, `ab`, … (base 26,
/// bijective, so every row has a short distinct tag).
#[must_use]
pub fn tag_for(index: usize) -> String {
    let mut tag = String::new();
    let mut n = index;
    loop {
        tag.insert(0, char::from(b'a' + u8::try_from(n % 26).unwrap_or(0)));
        if n < 26 {
            return tag;
        }
        n = n / 26 - 1;
    }
}

impl LinkHints {
    /// Fill the picker with `urls` (already filtered to registered schemes)
    /// and select the first row.
    pub fn open(&mut self, urls: Vec<String>) {
        self.links = urls
            .into_iter()
            .enumerate()
            .map(|(i, url)| LinkRow {
                tag: tag_for(i),
                url,
            })
            .collect();
        self.selected = 0;
        self.picked = None;
    }

    /// The URL the user activated, once.
    pub fn take_pick(&mut self) -> Option<String> {
        self.picked.take()
    }

    /// The rows currently offered.
    #[must_use]
    pub fn links(&self) -> &[LinkRow] {
        &self.links
    }

    /// Rows plus borders plus the hint line.
    #[must_use]
    pub fn height(&self) -> u16 {
        u16::try_from(self.links.len() + 3).unwrap_or(u16::MAX)
    }

    /// First row drawn, so the selected row stays visible in a short panel.
    fn window_start(&self, visible: usize) -> usize {
        if visible == 0 || self.links.len() <= visible {
            return 0;
        }
        self.selected.saturating_sub(visible - 1)
    }
}

impl Component for LinkHints {
    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        if self.links.is_empty() || area.width < 24 || area.height < 5 {
            return;
        }
        let width = area.width * 7 / 10;
        let height = self.height().min(area.height);
        let panel = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, panel);
        let block = Block::default()
            .title("─ Extension links ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.color("border")));
        let inner = block.inner(panel);
        frame.render_widget(block, panel);
        let accent = Style::default()
            .fg(theme.color("accent"))
            .add_modifier(Modifier::BOLD);
        let text = Style::default().fg(theme.color("text"));
        let dim = Style::default().fg(theme.color("dim"));
        let visible = usize::from(inner.height.saturating_sub(1));
        let start = self.window_start(visible);
        let mut lines: Vec<Line<'_>> = self
            .links
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(i, row)| {
                let current = i == self.selected;
                let style = if current { accent } else { text };
                Line::from(vec![
                    Span::styled(format!(" {} ", if current { "❯" } else { " " }), style),
                    Span::styled(format!("{:<3}", row.tag), style),
                    Span::styled(row.url.clone(), if current { accent } else { dim }),
                ])
            })
            .collect();
        lines.push(Line::from(Span::styled(
            " type a tag · Enter opens · Esc cancels",
            dim,
        )));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventOutcome {
        let len = self.links.len();
        match key.code {
            KeyCode::Up if len > 0 => self.selected = (self.selected + len - 1) % len,
            KeyCode::Down if len > 0 => self.selected = (self.selected + 1) % len,
            KeyCode::Enter => self.picked = self.links.get(self.selected).map(|r| r.url.clone()),
            KeyCode::Char(c) if !key.modifiers.contains(crate::keys::KeyModifiers::CONTROL) => {
                let typed = c.to_ascii_lowercase().to_string();
                match self.links.iter().find(|r| r.tag == typed) {
                    Some(row) => self.picked = Some(row.url.clone()),
                    // A tag that matches nothing cancels the picker (TS
                    // `typeLinkHint` on a miss); Esc does the same.
                    None => return EventOutcome::Ignored,
                }
            }
            // Esc bubbles so the host pops the panel.
            _ => return EventOutcome::Ignored,
        }
        EventOutcome::Consumed
    }

    fn wants_focus(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn picker(urls: &[&str]) -> LinkHints {
        let mut p = LinkHints::default();
        p.open(urls.iter().map(|u| (*u).to_owned()).collect());
        p
    }

    fn screen(p: &LinkHints) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        terminal
            .draw(|f| p.render(f, f.area(), &Theme::default()))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..10)
            .map(|y| (0..80).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn tags_run_a_to_z_then_aa() {
        assert_eq!(tag_for(0), "a");
        assert_eq!(tag_for(25), "z");
        assert_eq!(tag_for(26), "aa");
        assert_eq!(tag_for(27), "ab");
        assert_eq!(tag_for(51), "az");
        assert_eq!(tag_for(52), "ba");
    }

    #[test]
    fn typing_a_tag_activates_that_link() {
        let mut p = picker(&["cc-stretch:open/1", "cc-stretch:open/2"]);
        let outcome = p.handle_key(KeyEvent::from(KeyCode::Char('b')));
        assert!(outcome.consumed());
        assert_eq!(p.take_pick().as_deref(), Some("cc-stretch:open/2"));
        assert!(p.take_pick().is_none(), "a pick is delivered once");
    }

    #[test]
    fn enter_picks_the_selected_row_and_arrows_wrap() {
        let mut p = picker(&["cc-stretch:open/1", "cc-stretch:open/2"]);
        p.handle_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(p.selected, 1);
        p.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(p.take_pick().as_deref(), Some("cc-stretch:open/2"));
    }

    #[test]
    fn esc_and_unmatched_tags_bubble_so_the_host_closes() {
        let mut p = picker(&["cc-stretch:open/1"]);
        assert!(!p.handle_key(KeyEvent::from(KeyCode::Esc)).consumed());
        assert!(!p.handle_key(KeyEvent::from(KeyCode::Char('q'))).consumed());
        assert!(p.take_pick().is_none());
    }

    #[test]
    fn renders_tags_and_the_activation_hint() {
        let p = picker(&["cc-stretch:open/1792/0/2"]);
        let text = screen(&p);
        assert!(text.contains("Extension links"), "{text}");
        assert!(text.contains("a  cc-stretch:open/1792/0/2"), "{text}");
        assert!(text.contains("Esc cancels"), "{text}");
    }
}
