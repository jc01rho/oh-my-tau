//! Terminal capability matrix (port of `pi-tui` `terminal-capabilities.ts`).
//!
//! One place that identifies the host terminal from its environment and
//! decides every renderer feature gated on it: DEC 2026 synchronized output,
//! kitty keyboard protocol, inline image protocol, OSC 8 hyperlinks, the
//! notification channel and 24-bit color. [`render::sync_output_enabled`],
//! [`images::detect`] and the notification sink consult this module instead
//! of probing the environment themselves.
//!
//! Detection is a pure function of an environment lookup
//! ([`TerminalCapabilities::detect_with`]) so the matrix is unit-testable;
//! [`TerminalCapabilities::detect`] binds it to the process environment.
//!
//! [`render::sync_output_enabled`]: crate::render::sync_output_enabled
//! [`images::detect`]: crate::images::detect

use crate::chrome::notify::NotifyProtocol;
use crate::images::ImageProtocol;
use crate::render::Multiplexer;

/// Host terminal emulator, as far as the environment reveals it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalId {
    Kitty,
    Ghostty,
    WezTerm,
    ITerm2,
    Foot,
    Rio,
    Contour,
    Warp,
    Alacritty,
    VsCode,
    WindowsTerminal,
    ConEmu,
    Konsole,
    Vte,
    Rxvt,
    AppleTerminal,
    Unknown,
}

impl TerminalId {
    /// Identify the emulator. Emulator-owned identity variables win over
    /// `TERM_PROGRAM`, which in turn wins over the `TERM` name (both can be
    /// inherited across ssh / sudo and are therefore weaker evidence).
    #[must_use]
    pub fn detect_with(env: &dyn Fn(&str) -> Option<String>) -> Self {
        let set = |k: &str| env(k).is_some_and(|v| !v.is_empty());
        let term = env("TERM").unwrap_or_default().to_ascii_lowercase();
        let program = env("TERM_PROGRAM").unwrap_or_default().to_ascii_lowercase();
        if set("KITTY_WINDOW_ID") || term.contains("kitty") || program == "kitty" {
            return Self::Kitty;
        }
        if set("GHOSTTY_RESOURCES_DIR") || term.contains("ghostty") || program == "ghostty" {
            return Self::Ghostty;
        }
        if set("WEZTERM_EXECUTABLE") || term.contains("wezterm") || program == "wezterm" {
            return Self::WezTerm;
        }
        if set("ITERM_SESSION_ID") || program == "iterm.app" {
            return Self::ITerm2;
        }
        if program == "warpterminal" {
            return Self::Warp;
        }
        if program == "vscode" {
            return Self::VsCode;
        }
        if program == "apple_terminal" {
            return Self::AppleTerminal;
        }
        if program == "rio" || term.contains("rio") {
            return Self::Rio;
        }
        if term.starts_with("foot") {
            return Self::Foot;
        }
        if term.contains("contour") {
            return Self::Contour;
        }
        if set("ALACRITTY_WINDOW_ID") || term.contains("alacritty") {
            return Self::Alacritty;
        }
        if set("WT_SESSION") {
            return Self::WindowsTerminal;
        }
        if env("ConEmuANSI").as_deref() == Some("ON") {
            return Self::ConEmu;
        }
        if set("KONSOLE_VERSION") {
            return Self::Konsole;
        }
        if set("VTE_VERSION") {
            return Self::Vte;
        }
        if term.starts_with("rxvt") {
            return Self::Rxvt;
        }
        Self::Unknown
    }
}

/// Every capability decision for one terminal session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub terminal: TerminalId,
    pub multiplexer: Multiplexer,
    /// DEC 2026 synchronized output (`OMP_TUI_SYNC_OUTPUT=0|1` overrides).
    pub sync_output: bool,
    /// Terminal is known to implement the kitty keyboard protocol. The
    /// runtime probe in `render.rs` still has the final word.
    pub kitty_keyboard: bool,
    /// Inline image protocol (`PI_FORCE_IMAGE_PROTOCOL` overrides).
    pub images: ImageProtocol,
    /// OSC 8 hyperlinks (`FORCE_HYPERLINK=0|1` overrides).
    pub hyperlinks: bool,
    /// Notification escape sequence (`OMP_TUI_NOTIFY_PROTOCOL` overrides).
    pub notify: NotifyProtocol,
    /// 24-bit color (`COLORTERM=truecolor|24bit`, or a known emulator).
    pub truecolor: bool,
}

fn env_bool(value: Option<String>) -> Option<bool> {
    match value?.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

impl TerminalCapabilities {
    /// Detect from the process environment.
    #[must_use]
    pub fn detect() -> Self {
        Self::detect_with(&|k| std::env::var(k).ok())
    }

    /// Detect from an injected environment lookup.
    #[must_use]
    pub fn detect_with(env: &dyn Fn(&str) -> Option<String>) -> Self {
        use TerminalId as T;
        let terminal = TerminalId::detect_with(env);
        let multiplexer = Multiplexer::detect_with(env);
        let term = env("TERM").unwrap_or_default();

        // Inside a mux the mux owns the grid; tmux >= 3.3 (and the others)
        // implement synchronized updates themselves.
        let sync_output = env_bool(env("OMP_TUI_SYNC_OUTPUT")).unwrap_or_else(|| {
            multiplexer.owns_grid()
                || matches!(
                    terminal,
                    T::Kitty
                        | T::Ghostty
                        | T::WezTerm
                        | T::Foot
                        | T::Rio
                        | T::Contour
                        | T::ITerm2
                        | T::Warp
                )
        });

        let kitty_keyboard = matches!(
            terminal,
            T::Kitty | T::Ghostty | T::WezTerm | T::Foot | T::Rio | T::Alacritty | T::ITerm2
        );

        let images = match env("PI_FORCE_IMAGE_PROTOCOL").as_deref() {
            Some("kitty") => ImageProtocol::Kitty,
            Some("sixel") => ImageProtocol::Sixel,
            Some("iterm2") => ImageProtocol::Iterm2,
            Some("none") => ImageProtocol::None,
            _ => match terminal {
                T::Kitty => ImageProtocol::Kitty,
                _ if term.contains("sixel") => ImageProtocol::Sixel,
                T::ITerm2 => ImageProtocol::Iterm2,
                _ => ImageProtocol::None,
            },
        };

        let vte_ok = env("VTE_VERSION")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .is_some_and(|v| v >= 5000);
        let hyperlinks = env_bool(env("FORCE_HYPERLINK")).unwrap_or(match terminal {
            T::Vte => vte_ok,
            T::AppleTerminal | T::Rxvt | T::ConEmu | T::Unknown => false,
            _ => true,
        });

        let notify = match env("OMP_TUI_NOTIFY_PROTOCOL").as_deref() {
            Some("bell") => NotifyProtocol::Bell,
            Some("osc9") => NotifyProtocol::Osc9,
            Some("osc99") => NotifyProtocol::Osc99,
            Some("osc777") => NotifyProtocol::Osc777,
            // A mux swallows OSC notifications without passthrough; BEL
            // still reaches tmux monitor-bell and the outer terminal.
            _ if multiplexer.owns_grid() => NotifyProtocol::Bell,
            _ => match terminal {
                T::Kitty => NotifyProtocol::Osc99,
                T::ITerm2 | T::ConEmu | T::WindowsTerminal => NotifyProtocol::Osc9,
                T::Ghostty | T::WezTerm | T::Foot | T::Rxvt => NotifyProtocol::Osc777,
                _ => NotifyProtocol::Bell,
            },
        };

        let colorterm = env("COLORTERM").unwrap_or_default().to_ascii_lowercase();
        let truecolor = colorterm == "truecolor"
            || colorterm == "24bit"
            || !matches!(terminal, T::AppleTerminal | T::Rxvt | T::Unknown);

        Self {
            terminal,
            multiplexer,
            sync_output,
            kitty_keyboard,
            images,
            hyperlinks,
            notify,
            truecolor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(vars: &[(&str, &str)]) -> TerminalCapabilities {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        TerminalCapabilities::detect_with(&move |k| {
            vars.iter()
                .find(|(name, _)| name == k)
                .map(|(_, v)| v.clone())
        })
    }

    #[test]
    fn kitty_gets_full_matrix() {
        let c = caps(&[("TERM", "xterm-kitty")]);
        assert_eq!(c.terminal, TerminalId::Kitty);
        assert!(c.sync_output && c.kitty_keyboard && c.hyperlinks && c.truecolor);
        assert_eq!(c.images, ImageProtocol::Kitty);
        assert_eq!(c.notify, NotifyProtocol::Osc99);
    }

    #[test]
    fn identity_vars_beat_generic_term() {
        let c = caps(&[("TERM", "xterm-256color"), ("KITTY_WINDOW_ID", "1")]);
        assert_eq!(c.terminal, TerminalId::Kitty);
        let c = caps(&[("TERM", "xterm-256color"), ("TERM_PROGRAM", "WarpTerminal")]);
        assert_eq!(c.terminal, TerminalId::Warp);
        assert!(c.sync_output);
    }

    #[test]
    fn iterm_and_ghostty_rows() {
        let c = caps(&[("TERM_PROGRAM", "iTerm.app")]);
        assert_eq!(
            (c.images, c.notify),
            (ImageProtocol::Iterm2, NotifyProtocol::Osc9)
        );
        let c = caps(&[("TERM", "xterm-ghostty")]);
        assert_eq!(
            (c.images, c.notify),
            (ImageProtocol::None, NotifyProtocol::Osc777)
        );
        assert!(c.sync_output && c.kitty_keyboard);
    }

    #[test]
    fn unknown_terminal_is_conservative() {
        let c = caps(&[("TERM", "xterm-256color")]);
        assert_eq!(c.terminal, TerminalId::Unknown);
        assert!(!c.sync_output && !c.kitty_keyboard && !c.hyperlinks && !c.truecolor);
        assert_eq!(
            (c.images, c.notify),
            (ImageProtocol::None, NotifyProtocol::Bell)
        );
        let c = caps(&[("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]);
        assert!(c.truecolor);
    }

    #[test]
    fn multiplexer_forces_sync_and_bell() {
        let c = caps(&[("TERM", "tmux-256color"), ("TMUX", "/tmp/s,1,0")]);
        assert_eq!(c.multiplexer, Multiplexer::Tmux);
        assert!(c.sync_output);
        let c = caps(&[("TERM", "xterm-kitty"), ("ZELLIJ", "0")]);
        assert_eq!(c.notify, NotifyProtocol::Bell);
    }

    #[test]
    fn env_overrides_win() {
        let c = caps(&[("TERM", "xterm-kitty"), ("OMP_TUI_SYNC_OUTPUT", "0")]);
        assert!(!c.sync_output);
        let c = caps(&[("TERM", "xterm"), ("OMP_TUI_SYNC_OUTPUT", "1")]);
        assert!(c.sync_output);
        let c = caps(&[("TERM", "xterm"), ("FORCE_HYPERLINK", "1")]);
        assert!(c.hyperlinks);
        let c = caps(&[("TERM", "xterm"), ("PI_FORCE_IMAGE_PROTOCOL", "sixel")]);
        assert_eq!(c.images, ImageProtocol::Sixel);
        let c = caps(&[("TERM", "xterm"), ("OMP_TUI_NOTIFY_PROTOCOL", "osc777")]);
        assert_eq!(c.notify, NotifyProtocol::Osc777);
    }

    #[test]
    fn vte_hyperlinks_need_0_50() {
        assert!(!caps(&[("VTE_VERSION", "4802")]).hyperlinks);
        assert!(caps(&[("VTE_VERSION", "6800")]).hyperlinks);
    }

    #[test]
    fn sixel_term_selects_sixel() {
        let c = caps(&[("TERM", "foot-sixel")]);
        assert_eq!(c.terminal, TerminalId::Foot);
        assert_eq!(c.images, ImageProtocol::Sixel);
    }
}
