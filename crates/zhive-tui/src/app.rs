//! UI state and input/event reduction for the conversation screen.
//!
//! [`App`] is pure state plus logic: it folds engine notifications into its
//! [`Conversation`] and turns key presses into either local edits (input,
//! overlays, theme) or an [`Action`] for the event loop to perform over the
//! client. Keeping side effects out of `App` (no `Client` here) makes the whole
//! reducer unit-testable without a running engine.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use zhive_proto::permission::{
    PermissionOption, PermissionOptionKind, PermissionOutcome, RequestPermissionRequest,
};

use crate::config::TuiConfig;
use crate::conversation::Conversation;
use crate::protocol::EngineNotification;
use crate::theme::{Accent, Palette, Theme};

/// A side-effecting command the event loop should perform over the client.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Action {
    /// Nothing to do; state already updated locally.
    None,
    /// Tear down the UI and exit.
    Quit,
    /// Submit the given text as a new turn.
    Submit(String),
    /// Cancel the active turn.
    Cancel,
    /// Manually compact the current thread.
    Compact,
    /// Start a fresh thread, discarding the current transcript view.
    Clear,
    /// Resolve a pending permission prompt.
    ResolvePermission {
        /// The request id to echo back.
        request_id: String,
        /// The user's decision.
        outcome: PermissionOutcome,
    },
}

/// A modal overlay layered above the conversation.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Overlay {
    /// Keybinding / help reference.
    Help,
    /// Current provider and model information.
    ModelInfo,
    /// Current settings: theme, accent, density, keybindings.
    Settings,
    /// A pending permission decision.
    Approval {
        /// The request id to resolve.
        request_id: String,
        /// The prompt to render.
        request: Box<RequestPermissionRequest>,
    },
}

/// A slash command shown in the palette and dispatched on submit.
#[derive(Debug, Clone, Copy)]
pub struct SlashCommand {
    /// Command name without the leading slash.
    pub name: &'static str,
    /// One-line description for the palette.
    pub help: &'static str,
}

/// The slash commands the conversation screen understands.
pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "help",
        help: "show keybindings and commands",
    },
    SlashCommand {
        name: "model",
        help: "show the current provider and model",
    },
    SlashCommand {
        name: "settings",
        help: "show theme, accent, and keys",
    },
    SlashCommand {
        name: "theme",
        help: "switch theme — dark | light | mono",
    },
    SlashCommand {
        name: "accent",
        help: "switch accent — cyan | amber | lime | magenta",
    },
    SlashCommand {
        name: "compact",
        help: "summarize and condense the conversation",
    },
    SlashCommand {
        name: "clear",
        help: "start a fresh thread",
    },
    SlashCommand {
        name: "quit",
        help: "exit zap",
    },
];

/// The whole TUI state for the conversation experience.
#[derive(Debug)]
pub struct App {
    /// Host-supplied presentation config (also drives the top bar).
    pub config: TuiConfig,
    /// The active theme (mutable via `/theme`).
    pub theme: Theme,
    /// The active accent (mutable via `/accent`).
    pub accent: Accent,
    /// Resolved palette derived from `theme` + `accent`.
    pub palette: Palette,
    /// Folded conversation state.
    pub conversation: Conversation,
    /// The composer input buffer.
    pub input: crate::input::Input,
    /// Active modal overlay, if any.
    pub overlay: Option<Overlay>,
    /// A transient one-line status message (command feedback, errors).
    pub flash: Option<String>,
    /// Spinner animation step.
    pub spinner_tick: usize,
    /// Lines scrolled up from the bottom of the transcript (0 = follow tail).
    pub scrollback: u16,
    /// Highlighted entry in the slash-command palette.
    pub palette_index: usize,
    /// Set once the user asks to quit.
    pub should_quit: bool,
}

impl App {
    /// Builds an app bound to `thread`, themed from `config`.
    #[must_use]
    pub fn new(config: TuiConfig, thread: zhive_proto::domain::ThreadId) -> Self {
        let theme = config.theme;
        let accent = config.accent;
        Self {
            palette: Palette::resolve(theme, accent),
            theme,
            accent,
            conversation: Conversation::new(thread),
            input: crate::input::Input::new(),
            overlay: None,
            flash: None,
            spinner_tick: 0,
            scrollback: 0,
            palette_index: 0,
            should_quit: false,
            config,
        }
    }

    /// The slash query (text after `/`) when the palette should be shown.
    ///
    /// Active only while composing the command name — once a space is typed the
    /// user is entering arguments, so the palette closes.
    #[must_use]
    pub fn palette_query(&self) -> Option<&str> {
        if self.overlay.is_some() {
            return None;
        }
        let rest = self.input.value().strip_prefix('/')?;
        if rest.contains(char::is_whitespace) {
            None
        } else {
            Some(rest)
        }
    }

    /// Commands whose name prefixes the current palette query.
    #[must_use]
    pub fn palette_matches(&self) -> Vec<SlashCommand> {
        match self.palette_query() {
            Some(query) => COMMANDS
                .iter()
                .filter(|c| c.name.starts_with(query))
                .copied()
                .collect(),
            None => Vec::new(),
        }
    }

    /// Replaces the input with the highlighted palette command plus a space.
    fn palette_autocomplete(&mut self) {
        let matches = self.palette_matches();
        if let Some(cmd) = matches.get(self.palette_index).or_else(|| matches.first()) {
            self.input.clear();
            self.input.insert_str(&format!("/{} ", cmd.name));
            self.palette_index = 0;
        }
    }

    /// Advances the spinner; called on each redraw tick.
    pub fn tick(&mut self) {
        if self.conversation.busy {
            self.spinner_tick = self.spinner_tick.wrapping_add(1);
        }
    }

    /// Re-binds the conversation to a fresh thread (after `/clear`).
    pub fn reset_thread(&mut self, thread: zhive_proto::domain::ThreadId) {
        self.conversation = Conversation::new(thread);
        self.scrollback = 0;
    }

    /// Folds an engine notification into state, opening overlays as needed.
    pub fn on_engine(&mut self, event: &EngineNotification) {
        if let EngineNotification::PermissionRequested {
            request_id,
            request,
        } = event
        {
            self.overlay = Some(Overlay::Approval {
                request_id: request_id.clone(),
                request: request.clone(),
            });
        }
        self.conversation.apply(event);
        // Snap to the tail when new output lands so the user sees it.
        if matches!(
            event,
            EngineNotification::ItemAppended { .. } | EngineNotification::ItemDelta { .. }
        ) {
            self.scrollback = 0;
        }
    }

    /// Handles a key press, returning an [`Action`] for the event loop.
    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        self.flash = None;
        if self.overlay.is_some() {
            return self.on_overlay_key(key);
        }
        self.on_conversation_key(key)
    }

    /// Key handling while an overlay is open.
    fn on_overlay_key(&mut self, key: KeyEvent) -> Action {
        match self.overlay.take() {
            // Any key dismisses an informational overlay (already taken above).
            Some(Overlay::Help | Overlay::ModelInfo | Overlay::Settings) | None => Action::None,
            Some(Overlay::Approval {
                request_id,
                request,
            }) => self.resolve_approval(key, request_id, &request),
        }
    }

    /// Maps an approval keypress to a [`PermissionOutcome`].
    fn resolve_approval(
        &mut self,
        key: KeyEvent,
        request_id: String,
        request: &RequestPermissionRequest,
    ) -> Action {
        let pick = |kinds: &[PermissionOptionKind]| -> Option<&PermissionOption> {
            request.options.iter().find(|o| kinds.contains(&o.kind))
        };
        let chosen = match key.code {
            KeyCode::Char('y' | 'Y') => pick(&[
                PermissionOptionKind::AllowOnce,
                PermissionOptionKind::AllowAlways,
            ]),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => pick(&[
                PermissionOptionKind::RejectOnce,
                PermissionOptionKind::RejectAlways,
            ]),
            KeyCode::Char(d @ '1'..='9') => {
                let idx = (d as usize) - ('1' as usize);
                request.options.get(idx)
            }
            _ => {
                // Unknown key: keep the overlay open.
                self.overlay = Some(Overlay::Approval {
                    request_id,
                    request: Box::new(request.clone()),
                });
                return Action::None;
            }
        };
        match chosen {
            Some(option) => Action::ResolvePermission {
                request_id,
                outcome: PermissionOutcome::Selected {
                    option_id: option.id.clone(),
                },
            },
            None => Action::ResolvePermission {
                request_id,
                outcome: PermissionOutcome::Cancelled,
            },
        }
    }

    /// Key handling on the conversation screen.
    #[expect(
        clippy::too_many_lines,
        reason = "exhaustive key-dispatch table; splitting it would harm readability"
    )]
    fn on_conversation_key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let palette = self.palette_query().is_some();

        match key.code {
            KeyCode::Char('c') if ctrl => Action::Quit,
            KeyCode::Char('d') if ctrl && self.input.is_blank() => Action::Quit,
            KeyCode::Esc => {
                if self.conversation.busy {
                    Action::Cancel
                } else {
                    Action::None
                }
            }
            // Palette navigation takes Up/Down/Tab while composing a command.
            KeyCode::Up if palette => {
                self.palette_index = self.palette_index.saturating_sub(1);
                Action::None
            }
            KeyCode::Down if palette => {
                let max = self.palette_matches().len().saturating_sub(1);
                self.palette_index = (self.palette_index + 1).min(max);
                Action::None
            }
            KeyCode::Tab if palette => {
                self.palette_autocomplete();
                Action::None
            }
            KeyCode::Enter if alt => {
                self.input.insert_newline();
                Action::None
            }
            KeyCode::Char('j') if ctrl => {
                self.input.insert_newline();
                Action::None
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.palette_index = 0;
                Action::None
            }
            KeyCode::Char('w') if ctrl => {
                self.input.delete_word();
                self.palette_index = 0;
                Action::None
            }
            KeyCode::Char(c) if !ctrl => {
                self.input.insert_char(c);
                self.palette_index = 0;
                Action::None
            }
            KeyCode::Backspace => {
                self.input.backspace();
                self.palette_index = 0;
                Action::None
            }
            KeyCode::Delete => {
                self.input.delete();
                Action::None
            }
            KeyCode::Left if ctrl => {
                self.input.move_word_left();
                Action::None
            }
            KeyCode::Right if ctrl => {
                self.input.move_word_right();
                Action::None
            }
            KeyCode::Left => {
                self.input.move_left();
                Action::None
            }
            KeyCode::Right => {
                self.input.move_right();
                Action::None
            }
            KeyCode::Home => {
                self.input.move_home();
                Action::None
            }
            KeyCode::End => {
                self.input.move_end();
                Action::None
            }
            // ↑ / PageUp: try history first (single-line input), else scrollback.
            KeyCode::Up | KeyCode::PageUp => {
                let step = if matches!(key.code, KeyCode::PageUp) {
                    5
                } else {
                    1
                };
                if !matches!(key.code, KeyCode::PageUp)
                    && !palette
                    && self.input.should_history_navigate()
                    && self.input.history_prev()
                {
                    return Action::None;
                }
                self.scrollback = self.scrollback.saturating_add(step);
                Action::None
            }
            // ↓ / PageDown: try history first, else scrollback.
            KeyCode::Down | KeyCode::PageDown => {
                let step = if matches!(key.code, KeyCode::PageDown) {
                    5
                } else {
                    1
                };
                if !matches!(key.code, KeyCode::PageDown)
                    && !palette
                    && self.input.should_history_navigate()
                    && self.input.history_next()
                {
                    return Action::None;
                }
                self.scrollback = self.scrollback.saturating_sub(step);
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Submits the composer buffer, dispatching slash commands locally.
    fn submit(&mut self) -> Action {
        if self.input.is_blank() {
            return Action::None;
        }
        let text = self.input.take();
        // Push to history before routing, so every submitted text is captured.
        self.input.push_history(&text);
        if let Some(cmd) = text.strip_prefix('/') {
            return self.run_slash(cmd);
        }
        Action::Submit(text)
    }

    /// Interprets a `/command [args]` string.
    fn run_slash(&mut self, cmd: &str) -> Action {
        let mut parts = cmd.split_whitespace();
        let name = parts.next().unwrap_or("");
        let arg = parts.next();
        match name {
            "quit" | "exit" | "q" => Action::Quit,
            "clear" | "new" => Action::Clear,
            "compact" => Action::Compact,
            "help" | "?" => {
                self.overlay = Some(Overlay::Help);
                Action::None
            }
            "model" => {
                self.overlay = Some(Overlay::ModelInfo);
                Action::None
            }
            "settings" => {
                self.overlay = Some(Overlay::Settings);
                Action::None
            }
            "theme" => {
                self.set_theme(arg);
                Action::None
            }
            "accent" => {
                self.set_accent(arg);
                Action::None
            }
            other => {
                self.flash = Some(format!("unknown command: /{other}"));
                Action::None
            }
        }
    }

    /// Applies a `/theme <name>` change, re-resolving the palette.
    fn set_theme(&mut self, name: Option<&str>) {
        let theme = match name {
            Some("dark") => Theme::Dark,
            Some("light") => Theme::Light,
            Some("mono") => Theme::Mono,
            _ => {
                self.flash = Some("usage: /theme dark|light|mono".to_owned());
                return;
            }
        };
        self.theme = theme;
        self.palette = Palette::resolve(self.theme, self.accent);
        self.flash = Some(format!("theme: {name:?}"));
    }

    /// Applies an `/accent <name>` change, re-resolving the palette.
    fn set_accent(&mut self, name: Option<&str>) {
        let accent = match name {
            Some("cyan") => Accent::Cyan,
            Some("amber") => Accent::Amber,
            Some("lime") => Accent::Lime,
            Some("magenta") => Accent::Magenta,
            _ => {
                self.flash = Some("usage: /accent cyan|amber|lime|magenta".to_owned());
                return;
            }
        };
        self.accent = accent;
        self.palette = Palette::resolve(self.theme, self.accent);
        self.flash = Some(format!("accent: {name:?}"));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::KeyEventKind;
    use zhive_proto::domain::ThreadId;

    use super::*;

    fn app() -> App {
        App::new(TuiConfig::default(), ThreadId(Arc::from("thread:native/t")))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn typing_then_enter_submits_text() {
        let mut app = app();
        for c in "hi".chars() {
            assert_eq!(app.on_key(key(KeyCode::Char(c))), Action::None);
        }
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Submit("hi".to_owned())
        );
        assert!(app.input.is_blank());
    }

    #[test]
    fn blank_enter_does_nothing() {
        let mut app = app();
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
    }

    #[test]
    fn ctrl_c_quits() {
        let mut app = app();
        assert_eq!(app.on_key(ctrl(KeyCode::Char('c'))), Action::Quit);
    }

    #[test]
    fn slash_quit_quits() {
        let mut app = app();
        for c in "/quit".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Quit);
    }

    #[test]
    fn slash_theme_switches_palette_without_action() {
        let mut app = app();
        let before = app.palette;
        for c in "/theme light".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert_ne!(app.palette, before);
        assert_eq!(app.theme, Theme::Light);
    }

    #[test]
    fn esc_cancels_only_when_busy() {
        let mut app = app();
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::None);
        app.conversation.busy = true;
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::Cancel);
    }

    #[test]
    fn alt_enter_inserts_newline_not_submit() {
        let mut app = app();
        app.on_key(key(KeyCode::Char('a')));
        let alt_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT);
        assert_eq!(app.on_key(alt_enter), Action::None);
        assert!(app.input.value().contains('\n'));
    }

    #[test]
    fn key_event_kind_is_available() {
        // Compile-time guard that we can match on Press in the loop.
        let k = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(k.kind, KeyEventKind::Press);
    }

    #[test]
    fn palette_activates_on_slash_and_filters() {
        let mut app = app();
        assert!(app.palette_query().is_none());
        app.on_key(key(KeyCode::Char('/')));
        assert_eq!(app.palette_query(), Some(""));
        for c in "mo".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.palette_query(), Some("mo"));
        let matches = app.palette_matches();
        assert!(matches.iter().any(|c| c.name == "model"));
        assert!(!matches.iter().any(|c| c.name == "help"));
        // A space ends command composition and closes the palette.
        app.on_key(key(KeyCode::Char(' ')));
        assert!(app.palette_query().is_none());
    }

    #[test]
    fn slash_model_opens_and_any_key_closes_overlay() {
        let mut app = app();
        for c in "/model".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert!(matches!(app.overlay, Some(Overlay::ModelInfo)));
        app.on_key(key(KeyCode::Char('x')));
        assert!(app.overlay.is_none());
    }

    #[test]
    fn tab_autocompletes_unique_command() {
        let mut app = app();
        for c in "/co".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.input.value(), "/compact ");
    }
}

// Rust guideline compliant 2026-02-21
