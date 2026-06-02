//! Presentation configuration handed to the TUI by its host (`zhive-cli`).
//!
//! `zhive-tui` deliberately knows nothing about TOML, config-file discovery, or
//! provider credentials (D-002: it depends only on `zhive-proto` and the native
//! client). The host parses all of that and passes the distilled, UI-relevant
//! result in as a [`TuiConfig`]: which palette to use and what to print in the
//! top-bar breadcrumb / model pill.

use std::path::PathBuf;

use crate::theme::{Accent, Density, Theme};

/// UI-facing configuration: palette selection plus status-bar metadata.
#[derive(Debug, Clone)]
pub struct TuiConfig {
    /// Base theme (dark / light / mono).
    pub theme: Theme,
    /// Accent palette (cyan / amber / lime / magenta).
    pub accent: Accent,
    /// Panel padding density.
    pub density: Density,
    /// Provider label for the model pill, e.g. `"anthropic"`.
    pub provider_label: String,
    /// Model label for the model pill, e.g. `"claude-sonnet-4-6"`.
    pub model_label: String,
    /// Working directory shown in the breadcrumb.
    pub cwd: PathBuf,
    /// Optional VCS branch shown in the breadcrumb.
    pub branch: Option<String>,
    /// Optional session name shown in the breadcrumb.
    pub session_name: Option<String>,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            theme: Theme::default(),
            accent: Accent::default(),
            density: Density::default(),
            provider_label: "scripted".to_owned(),
            model_label: "demo".to_owned(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            branch: None,
            session_name: None,
        }
    }
}

impl TuiConfig {
    /// Renders the working directory with `$HOME` collapsed to `~`.
    ///
    /// Falls back to the lossy display string when the path is not UTF-8.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::PathBuf;
    /// use zhive_tui::config::TuiConfig;
    /// let cfg = TuiConfig { cwd: PathBuf::from("/tmp/x"), ..Default::default() };
    /// assert_eq!(cfg.cwd_display(), "/tmp/x");
    /// ```
    #[must_use]
    pub fn cwd_display(&self) -> String {
        let full = self.cwd.to_string_lossy().into_owned();
        let Some(home) = std::env::var_os("HOME") else {
            return full;
        };
        let home = home.to_string_lossy();
        if !home.is_empty()
            && let Some(rest) = full.strip_prefix(home.as_ref())
        {
            return format!("~{rest}");
        }
        full
    }
}

// Rust guideline compliant 2026-02-21
