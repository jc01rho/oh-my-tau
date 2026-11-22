//! Tool-approval dialog — port of `pi-tui/src/overlays/approval-dialog.ts`.
//!
//! The approval gate (omp-cli `approval.rs`) asks through a `select` frame
//! tagged `kind: "approval"`: the title's first line names the call (`Allow
//! tool: bash`), every later line is a detail (`Reason:`, `Command:`, file
//! content …), and `fullPrompt` optionally carries the untruncated details.
//! The dialog keeps the details to [`MAX_DETAIL_ROWS`] clipped rows that
//! Page Up/Down scroll, swaps in every detail wrapped on `app.tools.expand`,
//! and lists the choices with Approve highlighted. `app.approval.approve`
//! approves outright but only [`APPROVE_ARM`] after the dialog appears: its
//! default chords double as the follow-up chord, so a key meant for the
//! composer as the prompt pops up never approves it.

use std::cell::Cell;
use std::time::{Duration, Instant};

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::UnicodeWidthChar;

use super::{is_approve_label, is_deny_label, KeyResult};
use crate::keybindings::{format_key_hint, format_key_hints, KeybindingsManager};
use crate::keys::{KeyCode, KeyEvent, KeyModifiers};
use crate::theme::Theme;

/// Detail rows shown before the rest scrolls (TS `PREVIEW_LIMITS.COLLAPSED_ITEMS`).
pub(super) const MAX_DETAIL_ROWS: usize = 8;
/// The approve chord is ignored this long after the dialog appears (TS
/// `APPROVE_ARM_MS`).
pub(super) const APPROVE_ARM: Duration = Duration::from_millis(500);

/// Whether a `select` frame is a tool-approval prompt.
pub(super) fn is_approval(params: &Value) -> bool {
    params.get("kind").and_then(Value::as_str) == Some("approval")
}

/// A prompt's detail rows: every line after the title, trailing blanks
/// dropped (TS `promptDetails`).
fn details(prompt: &str) -> Vec<String> {
    let mut rows: Vec<String> = prompt
        .lines()
        .skip(1)
        .map(|l| l.replace('\t', "    "))
        .collect();
    while rows.last().is_some_and(|l| l.trim().is_empty()) {
        rows.pop();
    }
    rows
}

fn wrap(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() || width == 0 {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0;
    for ch in line.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push(ch);
        used += w;
    }
    rows.push(row);
    rows
}

fn clip(line: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    let total: usize = line.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= width {
        return line.to_owned();
    }
    for ch in line.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// The approval dialog state (TS `ApprovalDialogComponent`).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ApprovalDialog {
    /// The prompt's first line (`Allow tool: bash`).
    pub(super) title: String,
    summary: Vec<String>,
    full: Option<Vec<String>>,
    expanded: bool,
    scroll: usize,
    /// Detail rows on screen at the last render (Page Up/Down step).
    body_rows: Cell<usize>,
    choices: Vec<String>,
    choice: usize,
    armed_at: Instant,
}

impl ApprovalDialog {
    /// Build from a `select` frame's params; `shown` is when it became the
    /// active dialog (the approve chord arms [`APPROVE_ARM`] later).
    pub(super) fn new(params: &Value, options: Vec<String>, shown: Instant) -> Self {
        let prompt = params
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let title = prompt.lines().next().unwrap_or_default().to_owned();
        let full = params
            .get("fullPrompt")
            .and_then(Value::as_str)
            .map(details)
            .filter(|full| *full != details(prompt));
        let choice = options
            .iter()
            .position(|o| is_approve_label(o))
            .unwrap_or(0);
        Self {
            title,
            summary: details(prompt),
            full,
            expanded: false,
            scroll: 0,
            body_rows: Cell::new(MAX_DETAIL_ROWS),
            choices: options,
            choice,
            armed_at: shown + APPROVE_ARM,
        }
    }

    fn approve_label(&self) -> Option<&String> {
        self.choices.iter().find(|o| is_approve_label(o))
    }

    fn deny_label(&self) -> Option<&String> {
        self.choices.iter().find(|o| is_deny_label(o))
    }

    /// `app.approval.approve` at `now`: the approve label once armed, `None`
    /// while the chord is still ignored.
    pub(super) fn approve(&self, now: Instant) -> Option<String> {
        if now < self.armed_at {
            return None;
        }
        self.approve_label().cloned()
    }

    fn answer(label: &str) -> KeyResult {
        KeyResult::Answer(serde_json::json!({ "value": label }))
    }

    /// Esc / Ctrl+C deny (TS `#finish("deny")`); without a deny choice the
    /// dialog is cancelled, which the gate also treats as a denial.
    fn deny(&self) -> KeyResult {
        self.deny_label()
            .map_or(KeyResult::Cancel, |l| Self::answer(l))
    }

    /// Swap the clipped details for every detail wrapped, or back (TS
    /// `toggleDetails`).
    pub(super) fn toggle_details(&mut self) {
        self.expanded = !self.expanded;
        self.scroll = 0;
    }

    pub(super) fn handle_key(
        &mut self,
        key: KeyEvent,
        keys: &KeybindingsManager,
        now: Instant,
    ) -> KeyResult {
        if keys.matches_event(&key, "app.approval.approve") {
            return self
                .approve(now)
                .map_or(KeyResult::Consumed, |l| Self::answer(&l));
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.body_rows.get().saturating_sub(1).max(1);
        let count = self.choices.len().max(1);
        match key.code {
            KeyCode::Esc => return self.deny(),
            KeyCode::Char('c') if ctrl => return self.deny(),
            KeyCode::Up => self.choice = (self.choice + count - 1) % count,
            KeyCode::Down => self.choice = (self.choice + 1) % count,
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(page),
            KeyCode::PageDown => self.scroll += page,
            KeyCode::Enter => {
                return self
                    .choices
                    .get(self.choice)
                    .map_or(KeyResult::Consumed, |l| Self::answer(l));
            }
            _ => {}
        }
        KeyResult::Consumed
    }

    /// The details at `width`: one clipped row each, or wrapped rows once
    /// expanded (TS `#detailLines`).
    fn detail_rows(&self, width: usize) -> Vec<String> {
        let source = match (&self.full, self.expanded) {
            (Some(full), true) => full,
            _ => &self.summary,
        };
        if self.expanded {
            source.iter().flat_map(|l| wrap(l, width)).collect()
        } else {
            source.iter().map(|l| clip(l, width)).collect()
        }
    }

    /// Body lines for an inner area `width` wide and `rows` tall (the hint
    /// row excluded): details with a scrollbar when they overflow, a blank
    /// row, then the choices.
    pub(super) fn lines(
        &self,
        theme: &Theme,
        width: usize,
        rows: usize,
    ) -> (Vec<Line<'static>>, bool) {
        let fg = |token: &str| Style::default().fg(theme.color(token));
        let choice_rows = self.choices.len() + 1;
        let budget = if self.expanded {
            rows.saturating_sub(choice_rows).max(1)
        } else {
            rows.saturating_sub(choice_rows).clamp(1, MAX_DETAIL_ROWS)
        };
        let mut detail = self.detail_rows(width);
        let overflows = detail.len() > budget;
        if overflows {
            detail = self.detail_rows(width.saturating_sub(1));
        }
        let shown = detail.len().min(budget);
        self.body_rows.set(shown.max(1));
        let max_scroll = detail.len().saturating_sub(shown);
        let scroll = self.scroll.min(max_scroll);
        let thumb = if overflows {
            (shown * shown / detail.len()).clamp(1, shown)
        } else {
            0
        };
        let thumb_start = (scroll * (shown - thumb) + max_scroll / 2)
            .checked_div(max_scroll)
            .unwrap_or(0);
        let mut lines: Vec<Line<'static>> = Vec::new();
        for (i, row) in detail.iter().skip(scroll).take(shown).enumerate() {
            let mut spans = vec![Span::styled(row.clone(), fg("accent"))];
            if overflows {
                let used: usize = row.chars().map(|c| c.width().unwrap_or(0)).sum();
                spans.push(Span::raw(
                    " ".repeat(width.saturating_sub(1).saturating_sub(used)),
                ));
                spans.push(if i >= thumb_start && i < thumb_start + thumb {
                    Span::styled("█", fg("accent"))
                } else {
                    Span::styled("│", fg("text_muted"))
                });
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::default());
        for (i, label) in self.choices.iter().enumerate() {
            let deny = self.deny_label() == Some(label);
            lines.push(if i == self.choice {
                Line::styled(
                    clip(&format!("{} {label}", theme.glyph("prompt")), width),
                    fg(if deny { "error" } else { "accent" }).add_modifier(Modifier::BOLD),
                )
            } else {
                Line::styled(clip(&format!("  {label}"), width), fg("text"))
            });
        }
        (lines, overflows)
    }

    /// Footer hint (TS `#footerHint`) fitted to `width`: the choose/select
    /// keys, which the highlighted list already implies, go first when the
    /// row is too narrow for every key.
    pub(super) fn hint(
        &self,
        keys: &KeybindingsManager,
        theme: &Theme,
        overflows: bool,
        width: usize,
    ) -> String {
        let k = |key: &str| format_key_hint(key, theme);
        let approve = format_key_hints(&keys.get_keys("app.approval.approve"), theme);
        let expand = format_key_hints(&keys.get_keys("app.tools.expand"), theme);
        let mut segments = vec![
            format!("{}/{} choose", k("up"), k("down")),
            format!("{} select", k("enter")),
        ];
        if !approve.is_empty() {
            segments.push(format!("{approve} approve"));
        }
        segments.push(format!("{} deny", k("escape")));
        if !expand.is_empty() {
            segments.push(format!(
                "{expand} {}",
                if self.expanded { "less" } else { "details" }
            ));
        }
        if overflows {
            segments.push(format!("{}/{} scroll", k("pageUp"), k("pageDown")));
        }
        let fits = |s: &[String]| {
            s.join(" · ")
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum::<usize>()
                <= width
        };
        let mut drop = 0;
        while drop < 2 && !fits(&segments[drop..]) {
            drop += 1;
        }
        segments[drop..].join(" · ")
    }
}
