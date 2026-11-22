//! `local://` file picker for `/local` and `app.local.open` (TS
//! `local-file-editor.ts` `pickLocalFile`: a hook selector titled "Open
//! local:// file in external editor", one row per file with its size).

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::component::{Component, EventOutcome};
use crate::keys::{KeyCode, KeyEvent};
use crate::port::LocalFile;
use crate::theme::Theme;

/// The picker title (TS `PICKER_TITLE`).
pub const PICKER_TITLE: &str = "Open local:// file in external editor";

/// TS `formatBytes`.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    #[allow(clippy::cast_precision_loss)]
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes}B")
    } else if b < KB * KB {
        format!("{:.1}KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1}MB", b / (KB * KB))
    } else {
        format!("{:.1}GB", b / (KB * KB * KB))
    }
}

/// The picker state.
#[derive(Debug, Clone, Default)]
pub struct LocalFilePicker {
    files: Vec<LocalFile>,
    selected: usize,
    picked: Option<String>,
}

impl LocalFilePicker {
    /// Open over `files` (newest first) with the first row selected.
    pub fn open(&mut self, files: Vec<LocalFile>) {
        self.files = files;
        self.selected = 0;
        self.picked = None;
    }

    /// The picked file's exact name, once.
    pub fn take_pick(&mut self) -> Option<String> {
        self.picked.take()
    }
}

impl Component for LocalFilePicker {
    fn render(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let rows = u16::try_from(self.files.len()).unwrap_or(u16::MAX);
        let height = rows.saturating_add(2).min(area.height);
        if height < 3 || area.width < 20 {
            return;
        }
        let panel = Rect {
            x: area.x,
            y: area.y + area.height - height,
            width: area.width,
            height,
        };
        frame.render_widget(Clear, panel);
        let block = Block::default()
            .title(format!("─ {PICKER_TITLE} "))
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(theme.color("border")));
        let inner = block.inner(panel);
        frame.render_widget(block, panel);
        let visible = usize::from(inner.height).max(1);
        let offset = self.selected.saturating_sub(visible - 1);
        let accent = Style::default().fg(theme.color("accent"));
        let text = Style::default().fg(theme.color("text"));
        let dim = Style::default().fg(theme.color("dim"));
        let lines: Vec<Line<'_>> = self
            .files
            .iter()
            .enumerate()
            .skip(offset)
            .take(visible)
            .map(|(i, file)| {
                let current = i == self.selected;
                let style = if current { accent } else { text };
                let label = sanitize(&format!("local://{}", file.name));
                Line::from(vec![
                    Span::styled(format!(" {}", if current { "❯ " } else { "  " }), style),
                    Span::styled(label, style),
                    Span::raw("  "),
                    Span::styled(format_bytes(file.size), if current { accent } else { dim }),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn handle_key(&mut self, key: KeyEvent) -> EventOutcome {
        let len = self.files.len();
        match key.code {
            KeyCode::Up if len > 0 => self.selected = (self.selected + len - 1) % len,
            KeyCode::Down if len > 0 => self.selected = (self.selected + 1) % len,
            KeyCode::Enter => self.picked = self.files.get(self.selected).map(|f| f.name.clone()),
            KeyCode::Esc => return EventOutcome::Ignored,
            _ => {}
        }
        EventOutcome::Consumed
    }

    fn wants_focus(&self) -> bool {
        true
    }
}

/// Strip control characters and tabs so a filename with escapes cannot
/// corrupt the picker (TS `replaceTabs(sanitizeText(...))`).
fn sanitize(label: &str) -> String {
    label
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: u64) -> LocalFile {
        LocalFile {
            name: name.into(),
            size,
        }
    }

    #[test]
    fn bytes_format_like_ts() {
        assert_eq!(format_bytes(0), "0B");
        assert_eq!(format_bytes(1023), "1023B");
        assert_eq!(format_bytes(1536), "1.5KB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0MB");
    }

    #[test]
    fn picks_by_exact_name_and_esc_bubbles() {
        let mut p = LocalFilePicker::default();
        p.open(vec![file("a.md", 1), file("dir/b\tc.md", 2)]);
        p.handle_key(KeyEvent::from(KeyCode::Up));
        p.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(p.take_pick().as_deref(), Some("dir/b\tc.md"));
        assert_eq!(p.take_pick(), None);
        assert!(!p.handle_key(KeyEvent::from(KeyCode::Esc)).consumed());
        assert_eq!(sanitize("local://x\u{1b}[31m\tz"), "local://x[31m z");
    }
}
