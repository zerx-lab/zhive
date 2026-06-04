//! UI state and input/event reduction for the conversation screen.
//!
//! [`App`] is pure state plus logic: it folds engine notifications into its
//! [`Conversation`] and turns key presses into either local edits (input,
//! overlays, theme) or an [`Action`] for the event loop to perform over the
//! client. Keeping side effects out of `App` (no `Client` here) makes the whole
//! reducer unit-testable without a running engine.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
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
    /// The skill picker (`/skills`): a filterable list of all discovered skills.
    ///
    /// Filters against [`App::skills`] (held on the app, not duplicated here);
    /// selecting an entry fills the composer with `/skill:<name> ` so the user
    /// can add arguments before submitting.
    SkillList {
        /// Index of the highlighted entry among the filtered rows.
        selected: usize,
        /// Live fuzzy-filter query typed by the user.
        query: String,
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
    /// Whether the command consumes a trailing argument (e.g. `/theme dark`).
    ///
    /// Drives palette Enter: an arg-taking command completes the input to
    /// `/name ` and waits for the argument; the rest dispatch immediately.
    pub takes_args: bool,
}

impl SlashCommand {
    /// Builds a [`SlashCommand`] from static string literals.
    ///
    /// `takes_args` marks commands like `/theme` that expect a trailing value.
    ///
    /// # Examples
    ///
    /// ```
    /// use zhive_tui::app::SlashCommand;
    /// let cmd = SlashCommand::from_static("help", "show keybindings", false);
    /// assert_eq!(cmd.name, "help");
    /// assert!(!cmd.takes_args);
    /// ```
    #[must_use]
    pub fn from_static(name: &'static str, help: &'static str, takes_args: bool) -> Self {
        Self {
            name: name.to_owned(),
            help: help.to_owned(),
            takes_args,
        }
    }
}

/// A discovered skill the user can run from `/skill:<name>` or the `/skills`
/// picker.
///
/// Host-supplied at startup. `invocation` is the fully-rendered `<skill>` block
/// (produced by the engine host); running the skill submits that block as a
/// user message, with any trailing args appended after a blank line. The TUI
/// keeps this type local so it never depends on `zhive_core` (D-002).
///
/// # Examples
///
/// ```
/// use zhive_tui::app::SkillCommand;
/// let s = SkillCommand {
///     name: "commit".to_owned(),
///     description: "create a git commit".to_owned(),
///     invocation: "<skill name=\"commit\" location=\"/x/SKILL.md\">\n…\n</skill>".to_owned(),
/// };
/// assert_eq!(s.name, "commit");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCommand {
    /// Skill identifier (the bare name, e.g. `commit`).
    pub name: String,
    /// One-line description shown in the `/skills` picker.
    pub description: String,
    /// Pre-rendered `<skill>` block injected as a user message when run.
    pub invocation: String,
}

/// The built-in slash commands the conversation screen always understands.
///
/// Extra runtime commands (e.g. skill slash-commands discovered at startup) are
/// stored in [`App::extra_commands`] and merged at palette-match time.
#[must_use]
pub fn builtin_commands() -> Vec<SlashCommand> {
    [
        ("help", "show keybindings and commands", false),
        ("model", "show the current provider and model", false),
        ("settings", "show theme, accent, and keys", false),
        ("theme", "switch theme — dark | light | mono", true),
        (
            "accent",
            "switch accent — cyan | amber | lime | magenta",
            true,
        ),
        ("compact", "summarize and condense the conversation", false),
        ("session", "list and resume past sessions", false),
        ("skills", "browse and run a skill", false),
        ("clear", "start a fresh thread", false),
        ("quit", "exit zap", false),
    ]
    .into_iter()
    .map(|(n, h, a)| SlashCommand::from_static(n, h, a))
    .collect()
}

/// The whole TUI state for the conversation experience.
#[derive(Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent UI toggles (quit/disconnect/queue-halt/skill-expand); \
              a state machine would couple unrelated concerns"
)]
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
    /// Whether collapsible detail blocks show their full content (ctrl+o).
    ///
    /// A single global toggle covering `/skill:<name>` chips, tool-call output,
    /// and command output: ctrl+o expands/collapses all of them at once (the
    /// transcript has no per-message focus).
    pub details_expanded: bool,
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
    /// All discovered skills, for `/skill:<name>` execution and the `/skills`
    /// picker. Populate via [`App::set_skills`]; empty when none were found.
    pub skills: Vec<SkillCommand>,
    /// Messages composed while a turn was in flight, awaiting dispatch.
    ///
    /// FIFO: each is sent as its own turn once the previous turn finishes
    /// normally (see [`App::take_next_queued`]). Cleared on interrupt,
    /// `/clear`, and disconnect so stale input is never sent silently.
    pub message_queue: std::collections::VecDeque<String>,
    /// Pauses automatic queue draining after a failed or rejected submit.
    ///
    /// A transient RPC failure or engine rejection leaves the prior turn
    /// `Completed`, which would otherwise let the drainer cascade through (and
    /// silently drop) the whole queue. While set, [`App::take_next_queued`]
    /// yields nothing; the user resumes with a blank Enter (which clears it).
    pub queue_halted: bool,
    /// Remaining frames of the click-triggered logo ripple (0 = at rest).
    ///
    /// The welcome wordmark is static at rest, so this is the only logo
    /// animation state; the render loop ticks only while it is nonzero.
    pub logo_pulse: u16,
    /// Wordmark cell `(col, row)` a ripple expands from (the click point).
    pub logo_origin: (u16, u16),
    /// Screen rect of the welcome logo, recorded each render for click hit-tests.
    ///
    /// Set by the welcome renderer (which only borrows `&App`) and read by the
    /// event loop on a left click; honored only while [`Self::welcome_active`].
    pub logo_hit: Cell<Option<Rect>>,
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
            details_expanded: false,
            palette_index: 0,
            should_quit: false,
            last_usage: None,
            disconnected: false,
            extra_commands: Vec::new(),
            skills: Vec::new(),
            message_queue: std::collections::VecDeque::new(),
            queue_halted: false,
            logo_pulse: 0,
            logo_origin: (0, 0),
            logo_hit: Cell::new(None),
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
    /// app.set_extra_commands(vec![SlashCommand::from_static("deploy", "deploy to staging", false)]);
    /// assert_eq!(app.extra_commands.len(), 1);
    /// ```
    pub fn set_extra_commands(&mut self, commands: Vec<SlashCommand>) {
        self.extra_commands = commands;
    }

    /// Registers the discovered skills for `/skill:<name>` and the `/skills`
    /// picker.
    ///
    /// Replaces any previously registered skills.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_tui::app::{App, SkillCommand};
    /// use zhive_tui::TuiConfig;
    /// use zhive_proto::domain::ThreadId;
    /// let mut app = App::new(TuiConfig::default(), ThreadId(Arc::from("thread:native/t")));
    /// app.set_skills(vec![SkillCommand {
    ///     name: "commit".to_owned(),
    ///     description: "create a git commit".to_owned(),
    ///     invocation: "<skill name=\"commit\" location=\"/x/SKILL.md\">\nbody\n</skill>".to_owned(),
    /// }]);
    /// assert_eq!(app.skills.len(), 1);
    /// ```
    pub fn set_skills(&mut self, skills: Vec<SkillCommand>) {
        self.skills = skills;
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

    /// Moves the palette highlight one step, wrapping around the match list.
    ///
    /// A negative `delta` moves toward the top (and wraps to the bottom); a
    /// positive `delta` moves down (and wraps to the top). Only the sign is
    /// used — callers pass `-1` / `1`.
    fn palette_move(&mut self, delta: i32) {
        let len = self.palette_matches().len();
        if len == 0 {
            self.palette_index = 0;
            return;
        }
        // `+ len - 1` is a wrapping decrement without an unsigned underflow.
        self.palette_index = if delta < 0 {
            (self.palette_index + len - 1) % len
        } else {
            (self.palette_index + 1) % len
        };
    }

    /// Dispatches the highlighted palette command on a single Enter.
    ///
    /// Arg-taking commands (e.g. `/theme`) complete the input to `/name ` and
    /// wait for a second Enter; the rest run immediately. With no match it falls
    /// back to submitting the typed text (which surfaces `unknown command`).
    fn palette_submit(&mut self) -> Action {
        let matches = self.palette_matches();
        let Some(cmd) = matches
            .get(self.palette_index)
            .or_else(|| matches.first())
            .cloned()
        else {
            return self.submit();
        };
        if cmd.takes_args {
            self.palette_autocomplete();
            return Action::None;
        }
        self.input.clear();
        self.palette_index = 0;
        self.run_slash(&cmd.name)
    }

    /// Advances animation clocks; called on each redraw tick.
    ///
    /// A running logo ripple counts down, and the spinner only moves while a
    /// turn is in flight. At rest neither moves, so the loop parks the timer.
    pub fn tick(&mut self) {
        self.logo_pulse = self.logo_pulse.saturating_sub(1);
        if self.conversation.busy {
            self.spinner_tick = self.spinner_tick.wrapping_add(1);
        }
    }

    /// Returns `true` while the welcome screen is the active view.
    ///
    /// The welcome wordmark is shown only before the first turn and when no
    /// overlay is capturing the screen, so the loop accepts logo clicks exactly
    /// then.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_tui::app::App;
    /// use zhive_tui::TuiConfig;
    /// use zhive_proto::domain::ThreadId;
    /// let app = App::new(TuiConfig::default(), ThreadId(Arc::from("thread:native/t")));
    /// assert!(app.welcome_active());
    /// ```
    #[must_use]
    pub fn welcome_active(&self) -> bool {
        self.conversation.is_empty() && self.overlay.is_none()
    }

    /// Starts a click ripple from wordmark cell `(col, row)`.
    pub fn trigger_logo_pulse(&mut self, col: u16, row: u16) {
        self.logo_pulse = crate::logo::SWEEP_FRAMES;
        self.logo_origin = (col, row);
    }

    /// Re-binds the conversation to a fresh thread (after `/clear` or resume).
    ///
    /// Also drops any queued messages: they belonged to the old thread and must
    /// not leak into the new one.
    pub fn reset_thread(&mut self, thread: zhive_proto::domain::ThreadId) {
        self.conversation = Conversation::new(thread);
        self.scrollback = 0;
        self.message_queue.clear();
        self.queue_halted = false;
    }

    /// Records that the engine connection was lost.
    ///
    /// Sets the persistent [`Self::disconnected`] flag.  The footer renderer
    /// will display a permanent "engine disconnected" banner until the process exits.
    pub fn on_disconnected(&mut self) {
        self.disconnected = true;
        self.conversation.busy = false;
        // Drop the queue so a reconnect cannot silently fire stale input. The
        // persistent footer banner supersedes any flash, so none is set here.
        self.message_queue.clear();
        self.queue_halted = false;
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
        // Queue behaviour at terminal turn states: a failure keeps the queue but
        // surfaces it (the user resumes with Enter); an interrupt drops it. A
        // normal completion auto-drains via `take_next_queued` in the loop.
        match event {
            EngineNotification::TurnFailed { .. } if !self.message_queue.is_empty() => {
                // Keep the queue but halt auto-drain; resume with a blank Enter.
                self.queue_halted = true;
                self.flash = Some(format!(
                    "turn failed · {} still queued (↵ to continue)",
                    self.message_queue.len()
                ));
            }
            EngineNotification::TurnRejected { .. } if !self.message_queue.is_empty() => {
                // A rejection (e.g. rate limit) must not silently re-fire the
                // queue; halt and let the user resume deliberately.
                self.queue_halted = true;
                self.flash = Some(format!(
                    "turn rejected · {} still queued (↵ to continue)",
                    self.message_queue.len()
                ));
            }
            EngineNotification::SessionAborted(_) if !self.message_queue.is_empty() => {
                let n = self.message_queue.len();
                self.message_queue.clear();
                self.queue_halted = false;
                self.flash = Some(format!("stopped · cleared {n} queued"));
            }
            _ => {}
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
            Some(Overlay::SkillList { selected, query }) => {
                self.on_skill_list_key(key, selected, query)
            }
        }
    }

    /// Key handling for the `/skills` picker: navigate, filter, fill the
    /// composer with `/skill:<name> ` on Enter, or cancel on Esc.
    ///
    /// The overlay was `take`n by the caller; this re-installs it (with updated
    /// state) for every key except Enter (fills the composer and stays closed so
    /// the user can add arguments) and Esc (cancel). Navigation and filtering
    /// keep the highlight within the filtered rows.
    fn on_skill_list_key(&mut self, key: KeyEvent, selected: usize, mut query: String) -> Action {
        let filtered = filter_skills(&self.skills, &query);
        let max = filtered.len().saturating_sub(1);
        let mut selected = selected.min(max);
        match key.code {
            KeyCode::Esc => return Action::None,
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(max),
            // Ctrl+P / Ctrl+N mirror Up / Down (consistent with the palette).
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                selected = (selected + 1).min(max);
            }
            KeyCode::Enter => {
                // Bind the name (ends the `filtered` borrow) before mutating self.
                let chosen = filtered.get(selected).map(|s| s.name.clone());
                if let Some(name) = chosen {
                    self.input.clear();
                    self.input.insert_str(&format!("/skill:{name} "));
                    self.palette_index = 0;
                }
                // Stay closed regardless: the composer now holds the command.
                return Action::None;
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
        self.overlay = Some(Overlay::SkillList { selected, query });
        Action::None
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
            // Ctrl+P / Ctrl+N mirror Up / Down so navigation is consistent with
            // the slash palette. They must precede the `Char(c)` filter arm.
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                selected = selected.saturating_sub(1);
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                selected = (selected + 1).min(max);
            }
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
                    // Interrupt the turn; an interrupt also discards the queue
                    // (a stop means stop — never fire pending input afterwards).
                    if !self.message_queue.is_empty() {
                        let n = self.message_queue.len();
                        self.message_queue.clear();
                        self.queue_halted = false;
                        self.flash = Some(format!("stopped · cleared {n} queued"));
                    }
                    Action::Cancel
                } else if !self.message_queue.is_empty() {
                    // Idle with a leftover queue (e.g. after a failed turn): Esc
                    // clears it rather than interrupting nothing.
                    let n = self.message_queue.len();
                    self.message_queue.clear();
                    self.queue_halted = false;
                    self.flash = Some(format!("cleared {n} queued"));
                    Action::None
                } else {
                    Action::None
                }
            }
            // Palette navigation: arrows or Ctrl+P/Ctrl+N wrap around the list;
            // Tab completes; Enter dispatches the highlighted command.
            KeyCode::Up if palette => {
                self.palette_move(-1);
                Action::None
            }
            KeyCode::Char('p') if ctrl && palette => {
                self.palette_move(-1);
                Action::None
            }
            KeyCode::Down if palette => {
                self.palette_move(1);
                Action::None
            }
            KeyCode::Char('n') if ctrl && palette => {
                self.palette_move(1);
                Action::None
            }
            KeyCode::Tab if palette => {
                self.palette_autocomplete();
                Action::None
            }
            // Alt+Enter always inserts a newline, even with the palette open.
            KeyCode::Enter if palette && !alt => self.palette_submit(),
            KeyCode::Enter if alt => {
                self.input.insert_newline();
                Action::None
            }
            // Ctrl+J newline is suppressed while the palette is open (a newline
            // would terminate the command name and dismiss the palette).
            KeyCode::Char('j') if ctrl && !palette => {
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
            KeyCode::Char('x') if ctrl => {
                self.unqueue();
                Action::None
            }
            // Toggle expansion of every `/skill:<name>` invocation chip.
            KeyCode::Char('o') if ctrl => {
                self.details_expanded = !self.details_expanded;
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

    /// Submits the composer buffer, queuing or dispatching as appropriate.
    ///
    /// A blank buffer resumes the queue when the engine is idle (the
    /// `↵ to continue` affordance after a failed turn). Slash commands always
    /// run immediately; a plain message typed while the engine is busy is
    /// enqueued instead of racing the in-flight turn.
    fn submit(&mut self) -> Action {
        if self.input.is_blank() {
            // Empty Enter resumes a stalled queue (e.g. after a failed turn).
            if !self.conversation.busy
                && let Some(next) = self.message_queue.pop_front()
            {
                self.queue_halted = false;
                return Action::Submit(next);
            }
            return Action::None;
        }
        let text = self.input.take();
        // Push to history before routing, so every submitted text is captured.
        self.input.push_history(&text);
        if let Some(cmd) = text.strip_prefix('/') {
            return self.run_slash(cmd);
        }
        if self.conversation.busy {
            // Queue rather than race the in-flight turn; flushed one-per-turn.
            self.message_queue.push_back(text);
            self.flash = Some(format!("queued · {} pending", self.message_queue.len()));
            return Action::None;
        }
        // A fresh idle submit clears any halt so the queue resumes after it.
        self.queue_halted = false;
        Action::Submit(text)
    }

    /// Pops the next queued message when idle after a normally-completed turn.
    ///
    /// Returns `Some(text)` only when the engine is not busy, the queue is
    /// non-empty, and the most recent turn finished normally — a failed or
    /// interrupted turn does not auto-drain (the user resumes with Enter).
    /// Called by the event loop after each iteration to dispatch one per turn.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_tui::app::App;
    /// use zhive_tui::TuiConfig;
    /// use zhive_proto::domain::ThreadId;
    /// let mut app = App::new(TuiConfig::default(), ThreadId(Arc::from("t")));
    /// assert!(app.take_next_queued().is_none()); // empty queue → nothing to drain
    /// ```
    #[must_use]
    pub fn take_next_queued(&mut self) -> Option<String> {
        if self.queue_halted || self.conversation.busy || self.message_queue.is_empty() {
            return None;
        }
        match self.conversation.turns.last().map(|t| &t.status) {
            Some(crate::conversation::TurnLifecycle::Completed) => self.message_queue.pop_front(),
            _ => None,
        }
    }

    /// Cancels queued messages: pull the last back to edit, or clear the rest.
    ///
    /// First `Ctrl+X` pops the most recent queued message back into the composer
    /// (replacing the draft). A second `Ctrl+X` (composer already holds that
    /// draft and more remain) clears the remaining queue.
    fn unqueue(&mut self) {
        if !self.input.is_blank() && !self.message_queue.is_empty() {
            let n = self.message_queue.len();
            self.message_queue.clear();
            self.queue_halted = false;
            self.flash = Some(format!("cleared {n} queued"));
        } else if let Some(last) = self.message_queue.pop_back() {
            self.input.clear();
            self.input.insert_str(&last);
            self.palette_index = 0;
            self.flash = Some(format!("unqueued · {} pending", self.message_queue.len()));
        } else {
            self.flash = Some("nothing queued".to_owned());
        }
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
            // Open the skill picker.
            "skills" => {
                self.open_skill_list();
                Action::None
            }
            // A bare skill name (`/commit`, opencode-style) or the explicit
            // `/skill:<name>` form (pi-style) runs the skill directly. Built-in
            // commands above take precedence on a name clash; the `skill:` prefix
            // forces the skill even when a built-in shares the name. Args are
            // everything after the first whitespace.
            other => {
                let skill_name = other.strip_prefix("skill:").unwrap_or(other);
                if self.skills.iter().any(|s| s.name == skill_name) {
                    let args = cmd
                        .split_once(char::is_whitespace)
                        .map_or("", |(_, rest)| rest.trim());
                    return self.run_skill(skill_name, args);
                }
                self.flash = Some(format!("unknown command: /{other}"));
                Action::None
            }
        }
    }

    /// Runs a discovered skill by injecting its `<skill>` block as a user
    /// message, with any `args` appended after a blank line.
    ///
    /// Queues the message when the engine is busy, mirroring [`Self::submit`],
    /// so a skill run never races an in-flight turn. Flashes a notice when the
    /// name is unknown.
    fn run_skill(&mut self, name: &str, args: &str) -> Action {
        let Some(invocation) = self
            .skills
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.invocation.clone())
        else {
            self.flash = Some(format!("unknown skill: {name}"));
            return Action::None;
        };
        let text = if args.is_empty() {
            invocation
        } else {
            format!("{invocation}\n\n{args}")
        };
        if self.conversation.busy {
            self.message_queue.push_back(text);
            self.flash = Some(format!("queued · {} pending", self.message_queue.len()));
            return Action::None;
        }
        self.queue_halted = false;
        Action::Submit(text)
    }

    /// Opens the `/skills` picker overlay, or flashes when no skills exist.
    fn open_skill_list(&mut self) {
        if self.skills.is_empty() {
            self.flash = Some("no skills discovered".to_owned());
            return;
        }
        self.overlay = Some(Overlay::SkillList {
            selected: 0,
            query: String::new(),
        });
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

/// Filters skills by a case-insensitive substring over name + description.
///
/// An empty query keeps every skill in registration order.
///
/// # Examples
///
/// ```
/// use zhive_tui::app::{filter_skills, SkillCommand};
/// let skills = vec![SkillCommand {
///     name: "commit".to_owned(),
///     description: "create a git commit".to_owned(),
///     invocation: String::new(),
/// }];
/// assert_eq!(filter_skills(&skills, "git").len(), 1);
/// assert_eq!(filter_skills(&skills, "deploy").len(), 0);
/// ```
#[must_use]
pub fn filter_skills<'a>(skills: &'a [SkillCommand], query: &str) -> Vec<&'a SkillCommand> {
    if query.is_empty() {
        return skills.iter().collect();
    }
    let needle = query.to_lowercase();
    skills
        .iter()
        .filter(|s| {
            s.name.to_lowercase().contains(&needle)
                || s.description.to_lowercase().contains(&needle)
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

    // ---- skill slash / picker ----

    fn app_with_skills() -> App {
        let mut a = app();
        let skills = vec![
            SkillCommand {
                name: "commit".to_owned(),
                description: "make a git commit".to_owned(),
                invocation: "<skill name=\"commit\" location=\"/x/SKILL.md\">\nbody\n</skill>"
                    .to_owned(),
            },
            SkillCommand {
                name: "review".to_owned(),
                description: "review the code".to_owned(),
                invocation: "<skill name=\"review\" location=\"/y/SKILL.md\">\nrbody\n</skill>"
                    .to_owned(),
            },
        ];
        // Mirror `lib::run`'s palette registration so tests exercise the real
        // `/`-palette dispatch (`palette_submit`), not just `run_slash` directly.
        a.set_extra_commands(
            skills
                .iter()
                .map(|s| SlashCommand {
                    name: s.name.clone(),
                    help: s.description.clone(),
                    takes_args: false,
                })
                .collect(),
        );
        a.set_skills(skills);
        a
    }

    #[test]
    fn slash_skill_submits_invocation_block() {
        let mut a = app_with_skills();
        match a.run_slash("skill:commit") {
            Action::Submit(text) => {
                assert!(text.contains("<skill name=\"commit\""));
                assert!(text.contains("body"));
            }
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn slash_skill_appends_trailing_args() {
        let mut a = app_with_skills();
        match a.run_slash("skill:commit fix the bug") {
            Action::Submit(text) => assert!(text.ends_with("</skill>\n\nfix the bug")),
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn slash_bare_skill_name_executes() {
        // opencode-style: `/commit` (no `skill:` prefix) runs the skill directly.
        let mut a = app_with_skills();
        match a.run_slash("commit") {
            Action::Submit(text) => assert!(text.contains("<skill name=\"commit\"")),
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn typing_bare_skill_then_enter_runs_it_through_palette() {
        // Real keystroke path: `/commit` is a registered (takes_args=false)
        // palette command, so Enter dispatches via `palette_submit → run_slash`.
        let mut a = app_with_skills();
        for c in "/commit".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        match a.on_key(key(KeyCode::Enter)) {
            Action::Submit(text) => assert!(text.contains("<skill name=\"commit\"")),
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn typing_bare_skill_with_args_then_enter_runs_it() {
        // With a trailing arg the palette closes (space typed), so Enter routes
        // through `submit → run_slash`; the arg lands after the block.
        let mut a = app_with_skills();
        for c in "/commit fix the bug".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        match a.on_key(key(KeyCode::Enter)) {
            Action::Submit(text) => assert!(text.ends_with("</skill>\n\nfix the bug")),
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn slash_bare_skill_name_with_args() {
        let mut a = app_with_skills();
        match a.run_slash("commit polish the docs") {
            Action::Submit(text) => assert!(text.ends_with("</skill>\n\npolish the docs")),
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn slash_unknown_skill_flashes_and_does_not_submit() {
        let mut a = app_with_skills();
        assert_eq!(a.run_slash("skill:nonexistent"), Action::None);
        assert!(a.flash.is_some());
        // A bare unknown name is likewise rejected (not executed).
        let mut b = app_with_skills();
        assert_eq!(b.run_slash("definitely-not-a-skill"), Action::None);
        assert!(b.flash.is_some());
    }

    #[test]
    fn slash_skill_queues_when_busy() {
        let mut a = app_with_skills();
        a.conversation.busy = true;
        assert_eq!(a.run_slash("skill:commit"), Action::None);
        assert_eq!(a.message_queue.len(), 1);
    }

    #[test]
    fn slash_skills_opens_picker_when_skills_exist() {
        let mut a = app_with_skills();
        assert_eq!(a.run_slash("skills"), Action::None);
        assert!(matches!(a.overlay, Some(Overlay::SkillList { .. })));
    }

    #[test]
    fn slash_skills_flashes_when_none_discovered() {
        let mut a = app(); // no skills registered
        a.run_slash("skills");
        assert!(a.overlay.is_none());
        assert!(a.flash.is_some());
    }

    #[test]
    fn skill_picker_enter_fills_composer_with_command() {
        let mut a = app_with_skills();
        a.run_slash("skills"); // highlight defaults to the first skill (commit)
        assert_eq!(a.on_overlay_key(key(KeyCode::Enter)), Action::None);
        assert!(a.overlay.is_none());
        assert_eq!(a.input.value(), "/skill:commit ");
    }

    #[test]
    fn skill_picker_filters_by_query() {
        let mut a = app_with_skills();
        a.run_slash("skills");
        // Type "rev" → only "review" matches; Enter fills it.
        for c in "rev".chars() {
            a.on_overlay_key(key(KeyCode::Char(c)));
        }
        a.on_overlay_key(key(KeyCode::Enter));
        assert_eq!(a.input.value(), "/skill:review ");
    }

    #[test]
    fn ctrl_o_toggles_skill_chip_expansion() {
        let mut a = app();
        assert!(!a.details_expanded);
        assert_eq!(a.on_key(ctrl(KeyCode::Char('o'))), Action::None);
        assert!(a.details_expanded);
        a.on_key(ctrl(KeyCode::Char('o')));
        assert!(!a.details_expanded);
    }

    #[test]
    fn skill_picker_esc_cancels() {
        let mut a = app_with_skills();
        a.run_slash("skills");
        assert_eq!(a.on_overlay_key(key(KeyCode::Esc)), Action::None);
        assert!(a.overlay.is_none());
        assert!(a.input.is_blank());
    }

    // ---- message-queue helpers ----

    fn turn_id(s: &str) -> zhive_proto::domain::TurnId {
        zhive_proto::domain::TurnId(Arc::from(s))
    }

    /// Starts a turn on the app's thread (sets `busy`) and returns its id.
    fn start_turn(app: &mut App) -> zhive_proto::domain::TurnId {
        let turn = turn_id("turn:native/t/0");
        app.on_engine(&EngineNotification::TurnStarted {
            thread_id: app.conversation.thread_id.clone(),
            turn_id: turn.clone(),
        });
        turn
    }

    fn complete_turn(app: &mut App, turn: &zhive_proto::domain::TurnId) {
        app.on_engine(&EngineNotification::TurnCompleted {
            thread_id: app.conversation.thread_id.clone(),
            turn_id: turn.clone(),
        });
    }

    /// Types `text` into the composer one key at a time.
    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn busy_enter_enqueues_instead_of_submitting() {
        let mut app = app();
        start_turn(&mut app);
        assert!(app.conversation.busy);
        type_text(&mut app, "later");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert_eq!(app.message_queue.len(), 1);
        // Busy → nothing drains yet.
        assert!(app.take_next_queued().is_none());
    }

    #[test]
    fn queue_drains_one_per_turn_with_busy_gate() {
        let mut app = app();
        let turn = start_turn(&mut app);
        type_text(&mut app, "one");
        app.on_key(key(KeyCode::Enter));
        type_text(&mut app, "two");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.message_queue.len(), 2);
        complete_turn(&mut app, &turn);
        // Completion drains exactly one (FIFO).
        assert_eq!(app.take_next_queued().as_deref(), Some("one"));
        // Simulate the loop's perform() re-marking the app busy.
        app.conversation.busy = true;
        // One-per-turn: the second does not drain while busy.
        assert!(app.take_next_queued().is_none());
        assert_eq!(app.message_queue.len(), 1);
    }

    #[test]
    fn failed_turn_keeps_queue_and_blank_enter_resumes() {
        let mut app = app();
        let turn = start_turn(&mut app);
        type_text(&mut app, "keep");
        app.on_key(key(KeyCode::Enter));
        app.on_engine(&EngineNotification::TurnFailed {
            thread_id: app.conversation.thread_id.clone(),
            turn_id: turn,
            error: zhive_proto::domain::TurnError {
                message: "boom".to_owned(),
                additional_details: None,
            },
        });
        assert_eq!(app.message_queue.len(), 1, "failure keeps the queue");
        assert!(
            app.take_next_queued().is_none(),
            "a failed turn does not auto-drain"
        );
        // Blank Enter manually resumes the queue.
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Submit("keep".to_owned())
        );
        assert!(app.message_queue.is_empty());
    }

    #[test]
    fn esc_while_busy_clears_queue() {
        let mut app = app();
        start_turn(&mut app);
        type_text(&mut app, "drop");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.message_queue.len(), 1);
        assert_eq!(app.on_key(key(KeyCode::Esc)), Action::Cancel);
        assert!(app.message_queue.is_empty());
    }

    #[test]
    fn ctrl_x_unqueues_last_into_composer() {
        let mut app = app();
        start_turn(&mut app);
        type_text(&mut app, "edit me");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.message_queue.len(), 1);
        assert!(app.input.is_blank());
        app.on_key(ctrl(KeyCode::Char('x')));
        assert!(app.message_queue.is_empty());
        assert_eq!(app.input.value(), "edit me");
    }

    #[test]
    fn slash_command_runs_immediately_even_while_busy() {
        let mut app = app();
        start_turn(&mut app);
        // A slash command is a local control op — it must not be queued.
        type_text(&mut app, "/clear");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::Clear);
        assert!(app.message_queue.is_empty());
    }

    #[test]
    fn palette_enter_executes_highlighted_command() {
        let mut app = app();
        type_text(&mut app, "/he");
        // Single Enter dispatches the highlighted no-arg command (opens Help).
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert!(matches!(app.overlay, Some(Overlay::Help)));
    }

    #[test]
    fn palette_enter_on_arg_command_completes_without_dispatch() {
        let mut app = app();
        type_text(&mut app, "/th");
        // /theme takes an arg → Enter completes to "/theme " and waits.
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::None);
        assert_eq!(app.input.value(), "/theme ");
        assert!(app.overlay.is_none());
    }

    #[test]
    fn ctrl_n_p_wrap_palette_selection() {
        let mut app = app();
        app.on_key(key(KeyCode::Char('/')));
        let n = app.palette_matches().len();
        assert!(n > 1, "several builtins match an empty query");
        // Ctrl+P from the top wraps to the bottom.
        app.on_key(ctrl(KeyCode::Char('p')));
        assert_eq!(app.palette_index, n - 1);
        // Ctrl+N wraps back to the top.
        app.on_key(ctrl(KeyCode::Char('n')));
        assert_eq!(app.palette_index, 0);
    }

    #[test]
    fn halted_queue_does_not_auto_drain_then_blank_enter_resumes() {
        let mut app = app();
        let turn = start_turn(&mut app);
        type_text(&mut app, "a");
        app.on_key(key(KeyCode::Enter));
        complete_turn(&mut app, &turn);
        // Simulate a failed/rejected submit halting the queue (as the loop does).
        app.queue_halted = true;
        assert!(
            app.take_next_queued().is_none(),
            "a halted queue must not auto-drain even after a completed turn"
        );
        // Blank Enter clears the halt and resumes the queue.
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::Submit("a".to_owned())
        );
        assert!(!app.queue_halted, "resuming clears the halt");
    }

    #[test]
    fn rejected_turn_halts_queue_instead_of_auto_resending() {
        let mut app = app();
        start_turn(&mut app);
        type_text(&mut app, "x");
        app.on_key(key(KeyCode::Enter));
        app.on_engine(&EngineNotification::TurnRejected {
            reason: "rate limited".to_owned(),
        });
        assert!(app.queue_halted, "a rejection halts the queue");
        assert!(
            app.take_next_queued().is_none(),
            "rejected turn must not auto-resend the queue"
        );
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
            false,
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
        app.set_extra_commands(vec![SlashCommand::from_static("xfoo", "extra foo", false)]);
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
        app.set_extra_commands(vec![SlashCommand::from_static(
            "helpx",
            "extended help",
            false,
        )]);
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
    fn session_list_ctrl_n_p_navigate_like_arrows() {
        let mut app = app();
        app.overlay = Some(Overlay::SessionList {
            entries: vec![
                session_entry("thread:native/a", "alpha", "first"),
                session_entry("thread:native/b", "beta", "second"),
                session_entry("thread:native/c", "gamma", "third"),
            ],
            selected: 0,
            query: String::new(),
            filter_mode: SessionFilter::All,
        });
        // Ctrl+N steps down like Down (and must not type 'n' into the filter).
        app.on_key(ctrl(KeyCode::Char('n')));
        app.on_key(ctrl(KeyCode::Char('n')));
        match &app.overlay {
            Some(Overlay::SessionList {
                selected, query, ..
            }) => {
                assert_eq!(*selected, 2);
                assert!(query.is_empty(), "Ctrl+N must navigate, not filter");
            }
            other => panic!("overlay must remain open, got {other:?}"),
        }
        // Ctrl+P steps back up like Up.
        app.on_key(ctrl(KeyCode::Char('p')));
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
