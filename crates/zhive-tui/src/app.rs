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
    /// Open the session-list overlay (populated by an async `thread/list`).
    ///
    /// `filter` selects whether the async listing is scoped to the current
    /// working directory or lists every persisted thread.
    OpenSessionList {
        /// Which set of sessions to fetch (current cwd vs. all).
        filter: SessionFilter,
    },
    /// Resume a persisted thread: restore its history and switch the view to it.
    ResumeSession {
        /// The thread to resume.
        thread_id: zhive_proto::domain::ThreadId,
    },
}

/// Which set of persisted sessions the `/session` picker lists.
///
/// Defaults to [`SessionFilter::All`]: legacy recordings were stored with a
/// placeholder `cwd` (`"."`), so a cwd filter would hide them; listing all by
/// default keeps every session reachable. The user toggles to
/// [`SessionFilter::Cwd`] (current project only) with Tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SessionFilter {
    /// List every persisted thread, regardless of working directory.
    #[default]
    All,
    /// List only threads created under the current working directory.
    Cwd,
}

impl SessionFilter {
    /// Returns the other mode (the `All` ⇄ `Cwd` toggle).
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            Self::All => Self::Cwd,
            Self::Cwd => Self::All,
        }
    }

    /// A short label for the footer hint (`"all"` / `"cwd"`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Cwd => "cwd",
        }
    }
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
    /// The history-session picker (`/session`), a filterable select list.
    SessionList {
        /// All sessions returned by `thread/list`, newest first.
        entries: Vec<crate::rpc::SessionEntry>,
        /// Index of the highlighted entry among the filtered rows.
        selected: usize,
        /// Live fuzzy-filter query typed by the user.
        query: String,
        /// Whether the listing is scoped to the current cwd or lists all.
        filter_mode: SessionFilter,
    },
}

/// A slash command shown in the palette and dispatched on submit.
///
/// Owned strings allow both compile-time static commands and runtime-injected
/// skill commands to be stored uniformly in the same list.
#[derive(Debug, Clone)]
pub struct SlashCommand {
    /// Command name without the leading slash.
    pub name: String,
    /// One-line description for the palette.
    pub help: String,
}

impl SlashCommand {
    /// Builds a [`SlashCommand`] from static string literals.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_tui::app::SlashCommand;
    /// let cmd = SlashCommand::from_static("help", "show keybindings");
    /// assert_eq!(cmd.name, "help");
    /// ```
    #[must_use]
    pub fn from_static(name: &'static str, help: &'static str) -> Self {
        Self {
            name: name.to_owned(),
            help: help.to_owned(),
        }
    }
}

/// The built-in slash commands the conversation screen always understands.
///
/// Extra runtime commands (e.g. skill slash-commands discovered at startup) are
/// stored in [`App::extra_commands`] and merged at palette-match time.
#[must_use]
pub fn builtin_commands() -> Vec<SlashCommand> {
    [
        ("help", "show keybindings and commands"),
        ("model", "show the current provider and model"),
        ("settings", "show theme, accent, and keys"),
        ("theme", "switch theme — dark | light | mono"),
        ("accent", "switch accent — cyan | amber | lime | magenta"),
        ("compact", "summarize and condense the conversation"),
        ("session", "list and resume past sessions"),
        ("clear", "start a fresh thread"),
        ("quit", "exit zap"),
    ]
    .into_iter()
    .map(|(n, h)| SlashCommand::from_static(n, h))
    .collect()
}

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
    /// Most recently received token usage `(input, output)`, if any.
    ///
    /// Updated on every `events/usage` notification; `None` until the first
    /// such event is received.  Displayed in the top bar.
    pub last_usage: Option<(u64, u64)>,
    /// `true` once a [`crate::lib::ClientEvent::Disconnected`] event is seen.
    ///
    /// Triggers a persistent banner in the footer so the user is never left
    /// looking at a frozen UI without explanation.
    pub disconnected: bool,
    /// Runtime-injected slash commands (e.g. skill slash-commands).
    ///
    /// These are merged with the built-in commands at palette-match time.
    /// Populate via [`App::set_extra_commands`] or [`App::new_with_extra`].
    pub extra_commands: Vec<SlashCommand>,
}

impl App {
    /// Builds an app bound to `thread`, themed from `config`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_tui::app::App;
    /// use zhive_tui::TuiConfig;
    /// use zhive_proto::domain::ThreadId;
    /// let app = App::new(TuiConfig::default(), ThreadId(Arc::from("thread:native/t")));
    /// assert!(app.last_usage.is_none());
    /// assert!(!app.disconnected);
    /// ```
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
            last_usage: None,
            disconnected: false,
            extra_commands: Vec::new(),
            config,
        }
    }

    /// Registers runtime-injected slash commands (e.g. skill commands).
    ///
    /// Replaces any previously registered extra commands.  Built-in commands
    /// are unaffected.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_tui::app::{App, SlashCommand};
    /// use zhive_tui::TuiConfig;
    /// use zhive_proto::domain::ThreadId;
    /// let mut app = App::new(TuiConfig::default(), ThreadId(Arc::from("thread:native/t")));
    /// app.set_extra_commands(vec![SlashCommand::from_static("deploy", "deploy to staging")]);
    /// assert_eq!(app.extra_commands.len(), 1);
    /// ```
    pub fn set_extra_commands(&mut self, commands: Vec<SlashCommand>) {
        self.extra_commands = commands;
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
    ///
    /// Built-in commands are listed first, followed by extra runtime commands.
    #[must_use]
    pub fn palette_matches(&self) -> Vec<SlashCommand> {
        let Some(query) = self.palette_query() else {
            return Vec::new();
        };
        builtin_commands()
            .into_iter()
            .chain(self.extra_commands.iter().cloned())
            .filter(|c| c.name.starts_with(query))
            .collect()
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

    /// Records that the engine connection was lost.
    ///
    /// Sets the persistent [`Self::disconnected`] flag.  The footer renderer
    /// will display a permanent "engine disconnected" banner until the process exits.
    pub fn on_disconnected(&mut self) {
        self.disconnected = true;
        self.conversation.busy = false;
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
        // Capture token usage whenever the engine reports it.
        if let EngineNotification::Usage {
            input_tokens,
            output_tokens,
        } = event
        {
            self.last_usage = Some((*input_tokens, *output_tokens));
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
            Some(Overlay::SessionList {
                entries,
                selected,
                query,
                filter_mode,
            }) => self.on_session_list_key(key, entries, selected, query, filter_mode),
        }
    }

    /// Key handling for the `/session` picker: navigate, filter, resume, toggle
    /// the cwd/all scope, cancel.
    ///
    /// The overlay was `take`n by the caller; this re-installs it (with updated
    /// state) for every key except Enter (resume), Esc (cancel), and Tab (which
    /// closes this overlay and re-opens it with the toggled scope via an async
    /// re-fetch). Navigation and filtering keep the highlight within the
    /// filtered rows.
    fn on_session_list_key(
        &mut self,
        key: KeyEvent,
        entries: Vec<crate::rpc::SessionEntry>,
        selected: usize,
        mut query: String,
        filter_mode: SessionFilter,
    ) -> Action {
        let filtered = filter_sessions(&entries, &query);
        let max = filtered.len().saturating_sub(1);
        let mut selected = selected.min(max);
        match key.code {
            // Esc closes the overlay (already `take`n) with no further action.
            KeyCode::Esc => return Action::None,
            // Tab flips the listing scope. The overlay stays `take`n (closed);
            // the async re-fetch re-opens it with the new mode's results.
            KeyCode::Tab => {
                return Action::OpenSessionList {
                    filter: filter_mode.toggled(),
                };
            }
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(max),
            KeyCode::Enter => {
                if let Some(entry) = filtered.get(selected) {
                    return Action::ResumeSession {
                        thread_id: entry.id.clone(),
                    };
                }
                // Empty list: nothing to resume; keep the overlay open.
            }
            KeyCode::Backspace => {
                query.pop();
                selected = 0;
            }
            KeyCode::Char(c) => {
                query.push(c);
                selected = 0;
            }
            _ => {}
        }
        self.overlay = Some(Overlay::SessionList {
            entries,
            selected,
            query,
            filter_mode,
        });
        Action::None
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
            // Open the picker in the default scope (All); Tab toggles to cwd.
            "session" | "resume" => Action::OpenSessionList {
                filter: SessionFilter::default(),
            },
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

/// Filters session entries by a case-insensitive substring of title or preview.
///
/// An empty query matches everything, preserving the newest-first order. Used
/// by both the `/session` key handler and the overlay renderer so the
/// highlighted index always lines up with what is drawn.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_tui::app::filter_sessions;
/// use zhive_tui::rpc::SessionEntry;
/// use zhive_proto::domain::ThreadId;
/// let entries = vec![SessionEntry {
///     id: ThreadId(Arc::from("thread:native/a")),
///     title: Some("Refactor".to_owned()),
///     preview: "parser cleanup".to_owned(),
///     updated_at: 0,
///     subagent_parent: None,
/// }];
/// assert_eq!(filter_sessions(&entries, "refac").len(), 1);
/// assert_eq!(filter_sessions(&entries, "deploy").len(), 0);
/// assert_eq!(filter_sessions(&entries, "").len(), 1);
/// ```
#[must_use]
pub fn filter_sessions<'a>(
    entries: &'a [crate::rpc::SessionEntry],
    query: &str,
) -> Vec<&'a crate::rpc::SessionEntry> {
    if query.is_empty() {
        return entries.iter().collect();
    }
    let needle = query.to_lowercase();
    entries
        .iter()
        .filter(|e| {
            e.title
                .as_deref()
                .is_some_and(|t| t.to_lowercase().contains(&needle))
                || e.preview.to_lowercase().contains(&needle)
        })
        .collect()
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

    // ---- token usage ----

    #[test]
    fn usage_event_stores_last_usage() {
        let mut app = app();
        assert!(app.last_usage.is_none(), "no usage before first event");
        app.on_engine(&EngineNotification::Usage {
            input_tokens: 120,
            output_tokens: 45,
        });
        assert_eq!(app.last_usage, Some((120, 45)));
    }

    #[test]
    fn usage_event_updates_on_second_call() {
        let mut app = app();
        app.on_engine(&EngineNotification::Usage {
            input_tokens: 10,
            output_tokens: 5,
        });
        app.on_engine(&EngineNotification::Usage {
            input_tokens: 200,
            output_tokens: 80,
        });
        assert_eq!(app.last_usage, Some((200, 80)));
    }

    // ---- runtime slash commands ----

    #[test]
    fn extra_command_appears_in_palette_matches() {
        let mut app = app();
        app.set_extra_commands(vec![SlashCommand::from_static(
            "deploy",
            "deploy to staging",
        )]);
        // Simulate typing "/de"
        for c in "/de".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let matches = app.palette_matches();
        assert!(
            matches.iter().any(|c| c.name == "deploy"),
            "extra command must appear in palette"
        );
    }

    #[test]
    fn builtin_commands_still_work_with_extra_commands() {
        let mut app = app();
        app.set_extra_commands(vec![SlashCommand::from_static("xfoo", "extra foo")]);
        for c in "/help".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert!(
            matches!(app.overlay, Some(Overlay::Help)),
            "builtin /help must still open Help overlay"
        );
    }

    #[test]
    fn extra_command_does_not_shadow_builtin() {
        let mut app = app();
        // An extra command with a matching prefix must not hide the builtin.
        app.set_extra_commands(vec![SlashCommand::from_static("helpx", "extended help")]);
        for c in "/help".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let matches = app.palette_matches();
        assert!(
            matches.iter().any(|c| c.name == "help"),
            "builtin help must still appear"
        );
        assert!(
            matches.iter().any(|c| c.name == "helpx"),
            "extra command must also appear"
        );
    }

    // ---- disconnected handling ----

    #[test]
    fn on_disconnected_sets_flag_and_clears_busy() {
        let mut app = app();
        app.conversation.busy = true;
        assert!(!app.disconnected);
        app.on_disconnected();
        assert!(app.disconnected, "disconnected flag must be set");
        assert!(!app.conversation.busy, "busy must be cleared on disconnect");
    }

    // ---- session list (/session resume) ----

    fn session_entry(id: &str, title: &str, preview: &str) -> crate::rpc::SessionEntry {
        crate::rpc::SessionEntry {
            id: ThreadId(Arc::from(id)),
            title: Some(title.to_owned()),
            preview: preview.to_owned(),
            updated_at: 0,
            subagent_parent: None,
        }
    }

    #[test]
    fn slash_session_opens_session_list_action() {
        let mut app = app();
        for c in "/session".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        // The picker opens in the default (All) scope.
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::OpenSessionList {
                filter: SessionFilter::All
            }
        );
    }

    #[test]
    fn slash_resume_alias_also_opens_session_list() {
        let mut app = app();
        for c in "/resume".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::OpenSessionList {
                filter: SessionFilter::All
            }
        );
    }

    #[test]
    fn session_list_enter_resumes_selected_thread() {
        let mut app = app();
        app.overlay = Some(Overlay::SessionList {
            entries: vec![
                session_entry("thread:native/a", "alpha", "first"),
                session_entry("thread:native/b", "beta", "second"),
            ],
            selected: 1,
            query: String::new(),
            filter_mode: SessionFilter::All,
        });
        let action = app.on_key(key(KeyCode::Enter));
        match action {
            Action::ResumeSession { thread_id } => {
                assert_eq!(thread_id.0.as_ref(), "thread:native/b");
            }
            other => panic!("expected ResumeSession, got {other:?}"),
        }
        assert!(app.overlay.is_none(), "resume closes the overlay");
    }

    #[test]
    fn session_list_esc_cancels_without_action() {
        let mut app = app();
        app.overlay = Some(Overlay::SessionList {
            entries: vec![session_entry("thread:native/a", "alpha", "first")],
            selected: 0,
            query: String::new(),
            filter_mode: SessionFilter::All,
        });
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::None);
        assert!(app.overlay.is_none(), "esc closes the overlay");
    }

    #[test]
    fn session_list_tab_toggles_scope_and_requests_refetch() {
        let mut app = app();
        app.overlay = Some(Overlay::SessionList {
            entries: vec![session_entry("thread:native/a", "alpha", "first")],
            selected: 0,
            query: String::new(),
            filter_mode: SessionFilter::All,
        });
        // Tab from All requests a re-fetch scoped to the current cwd...
        assert_eq!(
            app.on_key(key(KeyCode::Tab)),
            Action::OpenSessionList {
                filter: SessionFilter::Cwd
            }
        );
        // ...and the overlay is closed pending the async re-open.
        assert!(app.overlay.is_none(), "tab closes the overlay for re-fetch");

        // Tab from Cwd toggles back to All.
        app.overlay = Some(Overlay::SessionList {
            entries: vec![session_entry("thread:native/a", "alpha", "first")],
            selected: 0,
            query: String::new(),
            filter_mode: SessionFilter::Cwd,
        });
        assert_eq!(
            app.on_key(key(KeyCode::Tab)),
            Action::OpenSessionList {
                filter: SessionFilter::All
            }
        );
    }

    #[test]
    fn session_list_navigation_clamps_within_filtered_rows() {
        let mut app = app();
        app.overlay = Some(Overlay::SessionList {
            entries: vec![
                session_entry("thread:native/a", "alpha", "first"),
                session_entry("thread:native/b", "beta", "second"),
            ],
            selected: 0,
            query: String::new(),
            filter_mode: SessionFilter::All,
        });
        // Down past the end clamps to the last row.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        match &app.overlay {
            Some(Overlay::SessionList { selected, .. }) => assert_eq!(*selected, 1),
            other => panic!("overlay must remain open, got {other:?}"),
        }
    }

    #[test]
    fn session_list_typing_filters_and_enter_resumes_match() {
        let mut app = app();
        app.overlay = Some(Overlay::SessionList {
            entries: vec![
                session_entry("thread:native/a", "alpha", "refactor parser"),
                session_entry("thread:native/b", "beta", "write docs"),
            ],
            selected: 0,
            query: String::new(),
            filter_mode: SessionFilter::All,
        });
        // Type "doc" → only the second entry survives, becoming index 0.
        for c in "doc".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let action = app.on_key(key(KeyCode::Enter));
        match action {
            Action::ResumeSession { thread_id } => {
                assert_eq!(thread_id.0.as_ref(), "thread:native/b");
            }
            other => panic!("expected ResumeSession for filtered match, got {other:?}"),
        }
    }

    #[test]
    fn filter_sessions_empty_query_keeps_all() {
        let entries = vec![
            session_entry("thread:native/a", "alpha", "x"),
            session_entry("thread:native/b", "beta", "y"),
        ];
        assert_eq!(filter_sessions(&entries, "").len(), 2);
    }

    #[test]
    fn filter_sessions_matches_preview_case_insensitively() {
        let entries = vec![
            session_entry("thread:native/a", "alpha", "Refactor Parser"),
            session_entry("thread:native/b", "beta", "write docs"),
        ];
        let hits = filter_sessions(&entries, "parser");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id.0.as_ref(), "thread:native/a");
    }
}

// Rust guideline compliant 2026-02-21
