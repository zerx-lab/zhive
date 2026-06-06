//! UI state and input/event reduction for the conversation screen.
//!
//! [`App`] is pure state plus logic: it folds engine notifications into its
//! [`Conversation`] and turns key presses into either local edits (input,
//! overlays, theme) or an [`Action`] for the event loop to perform over the
//! client. Keeping side effects out of `App` (no `Client` here) makes the whole
//! reducer unit-testable without a running engine.

use std::cell::{Cell, RefCell};

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
    /// Open the model picker (populated by an async `models/list`).
    OpenModelList,
    /// Hot-swap the engine's active model to the picked one.
    SwitchModel {
        /// The chosen model: its id drives the RPC, its capabilities update the
        /// top-bar pill and reasoning-depth cycle on success.
        model: Box<zhive_proto::rpc::ModelDescriptor>,
    },
    /// Write `text` to the system clipboard (transcript selection or `/copy`).
    ///
    /// The event loop performs the actual OSC 52 / native clipboard write; the
    /// reducer only decides *what* to copy so it stays free of terminal I/O.
    Copy(String),
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

/// A transcript text selection, in content-relative coordinates.
///
/// Each endpoint is `(line_idx, cell_x)`, where `line_idx` indexes the fully
/// wrapped transcript lines — so the selection survives scrolling — and `cell_x`
/// is the display column counted from the first *body* cell (after the role
/// gutter). `anchor` is where the drag began; `cursor` follows the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    /// Where the drag started.
    anchor: (usize, u16),
    /// Where the pointer currently is.
    cursor: (usize, u16),
    /// `true` while the mouse button is held (a drag is in progress).
    dragging: bool,
}

impl Selection {
    /// Returns the endpoints ordered so the first is `<=` the second.
    ///
    /// `(usize, u16)` orders lexicographically — by line, then column — which is
    /// exactly reading order.
    fn ordered(self) -> ((usize, u16), (usize, u16)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
}

/// Transcript render geometry captured each frame for mouse→line mapping.
///
/// Set by the body renderer (which only borrows `&App`) and read by the event
/// loop when translating a mouse position into a selection endpoint. Mirrors the
/// [`App::logo_hit`] render-to-handler hand-off.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SelGeom {
    /// Inner rect of the conversation body (excludes the panel border).
    body: Rect,
    /// Index of the transcript line shown at the top row of `body`.
    scroll_y: u16,
}

impl SelGeom {
    /// Builds geometry from the body rect and top-line index.
    pub(crate) fn new(body: Rect, scroll_y: u16) -> Self {
        Self { body, scroll_y }
    }
}

/// A clickable transcript region that toggles one item's expansion.
///
/// Rebuilt each frame by the transcript renderer, one per collapsible item
/// (tool call, command, diff, skill chip), and read by the event loop to map a
/// left click onto the item whose block was clicked. Mirrors the [`SelGeom`]
/// render-to-handler hand-off.
#[derive(Debug, Clone)]
pub(crate) struct ToggleZone {
    /// First transcript line index of the item's block (inclusive).
    start: usize,
    /// One-past-the-last transcript line index of the block (exclusive).
    end: usize,
    /// The item whose expansion a click in `[start, end)` toggles.
    id: zhive_proto::domain::ItemId,
}

impl ToggleZone {
    /// Builds a zone spanning `[start, end)` for item `id`.
    pub(crate) fn new(start: usize, end: usize, id: zhive_proto::domain::ItemId) -> Self {
        Self { start, end, id }
    }
}

/// Maps a mouse cell to a content-relative `(line_idx, cell_x)`, if inside `body`.
///
/// Clicks landing in the role gutter clamp to column 0. Returns `None` when the
/// position is outside the transcript body.
fn hit_to_content(geom: SelGeom, col: u16, row: u16) -> Option<(usize, u16)> {
    let body = geom.body;
    if body.width == 0 || body.height == 0 {
        return None;
    }
    let inside = col >= body.x
        && col < body.x.saturating_add(body.width)
        && row >= body.y
        && row < body.y.saturating_add(body.height);
    if !inside {
        return None;
    }
    let line_idx = usize::from(geom.scroll_y) + usize::from(row - body.y);
    let content_x0 = body.x.saturating_add(crate::ui::GUTTER);
    let cell_x = col.saturating_sub(content_x0);
    Some((line_idx, cell_x))
}

/// Like [`hit_to_content`] but clamps the position into `body` first.
///
/// Used while dragging so the selection keeps tracking when the pointer strays
/// just past the body's edges.
fn hit_to_content_clamped(geom: SelGeom, col: u16, row: u16) -> Option<(usize, u16)> {
    let body = geom.body;
    if body.width == 0 || body.height == 0 {
        return None;
    }
    let col = col.clamp(body.x, body.x.saturating_add(body.width).saturating_sub(1));
    let row = row.clamp(body.y, body.y.saturating_add(body.height).saturating_sub(1));
    hit_to_content(geom, col, row)
}

/// Returns the content-cell range `[from, to)` selected on `line_idx`, if any.
///
/// `line_width` bounds full-line rows (interior lines of a multi-line
/// selection). Returns `None` for lines outside the selection or with an empty
/// range (e.g. a blank line), so the highlight paints nothing there.
pub(crate) fn cell_range_for_line(
    sel: Selection,
    line_idx: usize,
    line_width: u16,
) -> Option<(u16, u16)> {
    let (lo, hi) = sel.ordered();
    if line_idx < lo.0 || line_idx > hi.0 {
        return None;
    }
    let from = if line_idx == lo.0 { lo.1 } else { 0 };
    let to = if line_idx == hi.0 { hi.1 } else { line_width };
    let from = from.min(line_width);
    let to = to.min(line_width);
    if to <= from {
        return None;
    }
    Some((from, to))
}

/// Extracts the selected text from the per-line body texts.
///
/// Joins the sliced lines with `\n`. Line indices are clamped to `lines` as a
/// backstop against a stale selection. A zero-width selection yields `""`.
fn extract_selection(sel: Selection, lines: &[String]) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let (lo, hi) = sel.ordered();
    let last = lines.len() - 1;
    let lo_line = lo.0.min(last);
    let hi_line = hi.0.min(last);
    let mut out = String::new();
    for line_idx in lo_line..=hi_line {
        let text = lines.get(line_idx).map_or("", String::as_str);
        let from = if line_idx == lo_line { lo.1 } else { 0 };
        let to = if line_idx == hi_line { hi.1 } else { u16::MAX };
        out.push_str(slice_by_cells(text, from, to));
        if line_idx != hi_line {
            out.push('\n');
        }
    }
    out
}

/// Returns the substring of `text` whose display columns fall in `[from, to)`.
///
/// Width-aware: a char is included when its *starting* column is within range,
/// so the selection snaps to character boundaries. Returns `""` when the range
/// is empty or starts past the text.
fn slice_by_cells(text: &str, from_cell: u16, to_cell: u16) -> &str {
    use unicode_width::UnicodeWidthChar;
    if to_cell <= from_cell {
        return "";
    }
    let from = u32::from(from_cell);
    let to = u32::from(to_cell);
    let mut col: u32 = 0;
    let mut start_byte: Option<usize> = None;
    for (idx, ch) in text.char_indices() {
        if start_byte.is_none() && col >= from {
            start_byte = Some(idx);
        }
        if start_byte.is_some() && col >= to {
            // First char at or past `to` ends the slice (exclusive).
            return start_byte.map_or("", |s| text.get(s..idx).unwrap_or(""));
        }
        col += u32::try_from(ch.width().unwrap_or(0)).unwrap_or(0);
    }
    start_byte.map_or("", |s| text.get(s..).unwrap_or(""))
}

/// A modal overlay layered above the conversation.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Overlay {
    /// Keybinding / help reference.
    Help,
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
    /// The model picker (`/models`): a filterable list of provider models.
    ///
    /// Populated by an async `models/list`; selecting a row hot-swaps the
    /// engine's active model. The active model is flagged in each entry.
    ModelList {
        /// All models the provider advertises, in endpoint order.
        models: Vec<zhive_proto::rpc::ModelDescriptor>,
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
        ("model", "switch the active model", false),
        ("settings", "show theme, accent, and keys", false),
        ("theme", "switch theme — dark | light | mono", true),
        (
            "accent",
            "switch accent — cyan | amber | lime | magenta",
            true,
        ),
        ("compact", "summarize and condense the conversation", false),
        ("copy", "copy the last assistant message", false),
        ("session", "list and resume past sessions", false),
        ("skills", "browse and run a skill", false),
        ("clear", "start a fresh thread", false),
        ("quit", "exit zhive", false),
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
    /// Max scrollback of the last rendered frame, for jump-to-top.
    ///
    /// Set by the conversation renderer (which only borrows `&App`) and read by
    /// the key handler so `ctrl+Home` can pin to the exact top rather than an
    /// arbitrary large value (which would leave relative scrolling stuck). It
    /// mirrors the [`Self::logo_hit`] render-to-handler hand-off pattern.
    pub viewport_max_scroll: Cell<u16>,
    /// Transcript line count of the last rendered frame.
    ///
    /// Paired with [`Self::last_width`] to anchor the viewport: when the user
    /// has scrolled up and the transcript grows (streaming output landing below
    /// the fold), the renderer bumps [`Self::scrollback`] by the growth so the
    /// viewed content stays fixed instead of sliding up.
    pub(crate) last_total: u16,
    /// Content width of the last rendered frame, paired with [`Self::last_total`].
    ///
    /// Anchoring is skipped when the width changed: a resize reflows every line,
    /// so a line-count delta then is not newly streamed output.
    pub(crate) last_width: u16,
    /// Baseline expansion of collapsible detail blocks, toggled by ctrl+o.
    ///
    /// Covers `/skill:<name>` chips, tool-call output, command output, and
    /// diffs. ctrl+o flips this baseline for every block at once (and clears
    /// [`Self::item_expanded`]); a mouse click instead overrides a single block.
    pub details_expanded: bool,
    /// Per-item expansion overrides layered on [`Self::details_expanded`].
    ///
    /// A left click on a collapsible block flips just that item here; the
    /// effective state is the override when present, else the baseline (see
    /// [`Self::item_is_expanded`]). Cleared by ctrl+o and on thread reset.
    item_expanded: std::collections::HashMap<zhive_proto::domain::ItemId, bool>,
    /// Clickable expand/collapse regions captured each frame, for hit-tests.
    ///
    /// Rebuilt every render by the transcript renderer (which only borrows
    /// `&App`) and read by the event loop on a left click. Mirrors the
    /// [`Self::sel_lines`] render-to-handler hand-off.
    pub(crate) toggle_zones: RefCell<Vec<ToggleZone>>,
    /// Current reasoning depth, cycled with Ctrl+T and sent with every turn.
    ///
    /// Defaults to [`zhive_proto::domain::ThinkingEffort::Off`]; the top bar
    /// shows the active level whenever it is above `Off`.
    pub thinking_effort: zhive_proto::domain::ThinkingEffort,
    /// Reasoning-depth cycle for the active model, from a live `/models` fetch.
    ///
    /// `Some` once a model is switched via the picker (the endpoint's
    /// `capabilities.effort` drives the exact `Off`-first cycle); `None` falls
    /// back to the static [`zhive_proto::domain::ThinkingEffort::cycle_for`]
    /// table keyed on the provider/model labels.
    pub active_effort_cycle: Option<Vec<zhive_proto::domain::ThinkingEffort>>,
    /// Set once the user changes the model or reasoning depth this session.
    ///
    /// Gates persistence on exit: the host writes the selection back to config
    /// only when this is `true`, so an untouched session never rewrites config
    /// (and a boot-time clamp from a failed `/models` fetch cannot erase the
    /// remembered depth). The boot-time clamp in [`App::new`] does not set it.
    pub selection_dirty: bool,
    /// Highlighted entry in the slash-command palette.
    pub palette_index: usize,
    /// Highlighted entry in the `@`-mention file picker.
    pub mention_index: usize,
    /// Cached workspace file index for the `@`-mention picker.
    ///
    /// `None` until the first `@` becomes active, then a flat, sorted list of
    /// project-relative file and folder paths (see [`crate::files::scan`]).
    /// Built once and reused; never refreshed for the app's lifetime.
    pub file_index: Option<Vec<String>>,
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
    /// Live click ripples on the welcome wordmark (empty at rest).
    ///
    /// The wordmark is static at rest, so this is the only logo animation
    /// state; the render loop ticks only while a ripple is playing. Each left
    /// click spawns an independent ripple, so rapid clicks layer without
    /// cancelling one another.
    pub(crate) logo: crate::logo::Ripples,
    /// Screen rect of the welcome logo, recorded each render for click hit-tests.
    ///
    /// Set by the welcome renderer (which only borrows `&App`) and read by the
    /// event loop on a left click; honored only while [`Self::welcome_active`].
    pub logo_hit: Cell<Option<Rect>>,
    /// Memoizes finalized-message markdown renders (cleared on palette change).
    pub(crate) render_cache: crate::render_cache::MarkdownCache,
    /// Active transcript text selection, if the user is selecting/has selected.
    ///
    /// Coordinates are content-relative (see [`Selection`]); `None` at rest. The
    /// body renderer paints the highlight and the event loop drives drag updates.
    pub(crate) selection: Option<Selection>,
    /// Transcript geometry from the last frame, for mouse→line mapping.
    pub(crate) sel_geom: Cell<SelGeom>,
    /// Per-line body text (gutter stripped) from the last frame.
    ///
    /// Populated only while a [`Self::selection`] exists (there is always a
    /// redraw between mouse-down and copy), so an idle TUI pays nothing.
    pub(crate) sel_lines: RefCell<Vec<String>>,
    /// Live context-compaction progress, shown as a streaming panel.
    ///
    /// `Some` from `CompactionStarted` until `CompactionCompleted`; on failure
    /// it stays `Some` with `error` set so the reason persists until the next
    /// turn or compaction. `None` at rest.
    pub compaction: Option<CompactionView>,
}

/// Live state of an in-progress (or just-failed) context compaction.
///
/// Drives the streaming summary panel: the summary text grows as
/// `CompactionDelta` events arrive, and `error` flips the panel to a failure
/// notice that persists in the transcript.
#[derive(Debug, Clone)]
pub struct CompactionView {
    /// Why compaction fired (manual `/compact` vs automatic threshold).
    pub trigger: zhive_proto::hook::CompactTrigger,
    /// Transcript items being folded into the summary.
    pub entries: u32,
    /// Summary text streamed so far.
    pub summary: String,
    /// Failure reason if compaction failed; `None` while in progress/succeeded.
    pub error: Option<String>,
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
        // The active model's depth cycle: the live endpoint cycle fetched at boot
        // when available, else the static per-provider table.
        let active_effort_cycle = config.effort_cycle.clone();
        // Clamp the restored depth to a level the active model actually supports,
        // so a remembered depth the model cannot honor resets to Off rather than
        // being sent on the next turn.
        let thinking_effort = {
            use zhive_proto::domain::ThinkingEffort;
            let levels: Vec<ThinkingEffort> = active_effort_cycle.clone().unwrap_or_else(|| {
                ThinkingEffort::cycle_for(&config.provider_label, &config.model_label).to_vec()
            });
            if levels.contains(&config.thinking_effort) {
                config.thinking_effort
            } else {
                ThinkingEffort::Off
            }
        };
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
            viewport_max_scroll: Cell::new(0),
            last_total: 0,
            last_width: 0,
            details_expanded: false,
            item_expanded: std::collections::HashMap::new(),
            toggle_zones: RefCell::new(Vec::new()),
            thinking_effort,
            active_effort_cycle,
            selection_dirty: false,
            palette_index: 0,
            mention_index: 0,
            file_index: None,
            should_quit: false,
            last_usage: None,
            disconnected: false,
            extra_commands: Vec::new(),
            skills: Vec::new(),
            message_queue: std::collections::VecDeque::new(),
            queue_halted: false,
            logo: crate::logo::Ripples::default(),
            logo_hit: Cell::new(None),
            render_cache: crate::render_cache::MarkdownCache::default(),
            selection: None,
            sel_geom: Cell::new(SelGeom::default()),
            sel_lines: RefCell::new(Vec::new()),
            compaction: None,
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

    /// Advances the reasoning depth one step through the active model's levels.
    ///
    /// Bound to Ctrl+T. The cycle is model-specific
    /// ([`zhive_proto::domain::ThinkingEffort::cycle_for`]) so the displayed
    /// level is always one the model actually supports — what you see is exactly
    /// what is sent. Sets a transient [`Self::flash`] announcing the new level,
    /// or a "not supported" notice for models without depth control.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_tui::app::App;
    /// use zhive_tui::TuiConfig;
    /// use zhive_proto::domain::{ThinkingEffort, ThreadId};
    /// // The default config's provider is non-Anthropic, which offers the
    /// // portable low/medium/high tiers.
    /// let mut app = App::new(TuiConfig::default(), ThreadId(Arc::from("thread:native/t")));
    /// assert_eq!(app.thinking_effort, ThinkingEffort::Off);
    /// app.cycle_thinking_effort();
    /// assert_eq!(app.thinking_effort, ThinkingEffort::Low);
    /// ```
    pub fn cycle_thinking_effort(&mut self) {
        use zhive_proto::domain::ThinkingEffort;
        // Prefer the active model's live cycle (from a `/models` switch); fall
        // back to the static per-provider table keyed on the labels. Cloned to
        // an owned Vec (at most a handful of entries) so the two sources share a
        // type without borrow gymnastics.
        let levels: Vec<ThinkingEffort> = self.active_effort_cycle.clone().unwrap_or_else(|| {
            ThinkingEffort::cycle_for(&self.config.provider_label, &self.config.model_label)
                .to_vec()
        });
        // A single-entry cycle is `[Off]`: the model has no depth control.
        if levels.len() <= 1 {
            self.flash = Some(format!(
                "thinking not supported by {}",
                self.config.model_label
            ));
            return;
        }
        self.thinking_effort = self.thinking_effort.cycle_next(&levels);
        self.selection_dirty = true;
        self.flash = Some(format!("thinking: {}", self.thinking_effort.label()));
    }

    /// Applies a completed model switch to the top-bar pill and reasoning cycle.
    ///
    /// Updates [`TuiConfig::model_label`] to the new model id, installs the
    /// model's `Off`-first reasoning-depth cycle (empty/`[Off]` means no depth
    /// control), and clamps the current [`Self::thinking_effort`] to a level the
    /// new model supports (resetting to `Off` when the prior level is gone).
    pub fn apply_model_switch(
        &mut self,
        model_id: String,
        supported_efforts: Vec<zhive_proto::domain::ThinkingEffort>,
    ) {
        use zhive_proto::domain::ThinkingEffort;
        self.config.model_label = model_id;
        self.selection_dirty = true;
        // Clamp the current depth to the new model's supported set.
        let supports_current = supported_efforts.contains(&self.thinking_effort);
        if !supports_current {
            self.thinking_effort = ThinkingEffort::Off;
        }
        self.active_effort_cycle = if supported_efforts.is_empty() {
            // An empty set means the endpoint reported nothing; defer to the
            // static table rather than locking the model out of depth control.
            None
        } else {
            Some(supported_efforts)
        };
        self.flash = Some(format!("switched to {}", self.config.model_label));
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

    /// The active `@`-mention query, or `None` when no mention is being typed.
    ///
    /// A mention is the run of non-whitespace characters after the last `@`
    /// before the cursor, where that `@` starts a token (it is at the buffer
    /// start or follows whitespace). Suppressed while an overlay or the slash
    /// palette is open, so the two pickers never fight over the same keystrokes.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_tui::app::App;
    /// use zhive_tui::TuiConfig;
    /// use zhive_proto::domain::ThreadId;
    /// let mut app = App::new(TuiConfig::default(), ThreadId(Arc::from("thread:native/t")));
    /// app.input.insert_str("look @sr");
    /// assert_eq!(app.mention_query(), Some("sr"));
    /// // A space ends the token, closing the picker.
    /// app.input.insert_str(" done");
    /// assert_eq!(app.mention_query(), None);
    /// ```
    #[must_use]
    pub fn mention_query(&self) -> Option<&str> {
        if self.overlay.is_some() || self.palette_query().is_some() {
            return None;
        }
        let before = self.input.before_cursor();
        let at = before.rfind('@')?;
        // The `@` must open a token: at the start or right after whitespace.
        if !before[..at]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace)
        {
            return None;
        }
        let query = &before[at + 1..];
        if query.contains(char::is_whitespace) {
            None
        } else {
            Some(query)
        }
    }

    /// `true` once a mention is active but the file index has not been built.
    ///
    /// The event loop polls this to scan the workspace lazily (once) before the
    /// next draw, so the popup has paths to rank.
    #[must_use]
    pub fn needs_file_index(&self) -> bool {
        self.mention_query().is_some() && self.file_index.is_none()
    }

    /// Installs the scanned workspace file index for the `@`-mention picker.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use zhive_tui::app::App;
    /// use zhive_tui::TuiConfig;
    /// use zhive_proto::domain::ThreadId;
    /// let mut app = App::new(TuiConfig::default(), ThreadId(Arc::from("thread:native/t")));
    /// app.input.insert_str("@");
    /// assert!(app.needs_file_index());
    /// app.set_file_index(vec!["src/main.rs".to_owned()]);
    /// assert!(!app.needs_file_index());
    /// assert_eq!(app.mention_matches(), vec!["src/main.rs"]);
    /// ```
    pub fn set_file_index(&mut self, files: Vec<String>) {
        self.file_index = Some(files);
    }

    /// Workspace paths ranked against the active mention query, best first.
    ///
    /// Empty when no mention is active or the file index is not yet built.
    #[must_use]
    pub fn mention_matches(&self) -> Vec<&str> {
        let Some(query) = self.mention_query() else {
            return Vec::new();
        };
        let Some(files) = self.file_index.as_deref() else {
            return Vec::new();
        };
        crate::files::fuzzy_filter(files, query)
    }

    /// Moves the mention highlight one step, wrapping around the match list.
    ///
    /// Mirrors [`Self::palette_move`]: only the sign of `delta` matters.
    fn mention_move(&mut self, delta: i32) {
        let len = self.mention_matches().len();
        if len == 0 {
            self.mention_index = 0;
            return;
        }
        self.mention_index = if delta < 0 {
            (self.mention_index + len - 1) % len
        } else {
            (self.mention_index + 1) % len
        };
    }

    /// Inserts the highlighted file path in place of the active mention token.
    ///
    /// Replaces `@<query>` with `@<path> `; a no-op when nothing matches.
    fn mention_accept(&mut self) -> Action {
        let chosen = {
            let matches = self.mention_matches();
            matches
                .get(self.mention_index)
                .or_else(|| matches.first())
                .map(|s| (*s).to_owned())
        };
        if let Some(path) = chosen {
            let token_len = self.mention_query().map_or(0, |q| q.chars().count() + 1);
            self.input.replace_mention(token_len, &path);
            self.mention_index = 0;
        }
        Action::None
    }

    /// Advances animation clocks; called on each redraw tick.
    ///
    /// Live logo ripples age (and prune), and the spinner only moves while a
    /// turn is in flight. At rest neither moves, so the loop parks the timer.
    pub fn tick(&mut self) {
        self.logo.tick();
        if self.conversation.busy {
            self.spinner_tick = self.spinner_tick.wrapping_add(1);
        }
        // Surface a slice more of the streamed buffer each tick (smooth reveal).
        self.conversation.advance_reveal();
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

    /// Spawns a click ripple from wordmark cell `(col, row)`.
    ///
    /// Independent of any ripple already playing, so rapid clicks accumulate
    /// into a continuous shimmer rather than cancelling one another.
    pub fn spawn_logo_ripple(&mut self, col: u16, row: u16) {
        self.logo.spawn(col, row);
    }

    /// Begins a transcript selection at a mouse cell, if it hit the body.
    ///
    /// Replaces any prior selection. A no-op when the click lands outside the
    /// transcript (e.g. the composer), leaving an existing selection intact.
    pub(crate) fn selection_start(&mut self, col: u16, row: u16) {
        if let Some(hit) = hit_to_content(self.sel_geom.get(), col, row) {
            self.selection = Some(Selection {
                anchor: hit,
                cursor: hit,
                dragging: true,
            });
        }
    }

    /// Extends the in-progress selection to a mouse cell while dragging.
    pub(crate) fn selection_update(&mut self, col: u16, row: u16) {
        let geom = self.sel_geom.get();
        let Some(sel) = self.selection.as_mut() else {
            return;
        };
        if sel.dragging
            && let Some(hit) = hit_to_content_clamped(geom, col, row)
        {
            sel.cursor = hit;
        }
    }

    /// Ends the drag. A click with no movement selects nothing and is dropped.
    pub(crate) fn selection_finish(&mut self) {
        match self.selection {
            // Zero-width: a plain click. Drop it so `Ctrl+C` still quits and no
            // stray highlight lingers.
            Some(sel) if sel.anchor == sel.cursor => self.selection = None,
            Some(_) => {
                if let Some(sel) = self.selection.as_mut() {
                    sel.dragging = false;
                }
            }
            None => {}
        }
    }

    /// Whether a transcript selection is currently active.
    pub(crate) fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    /// Drops any active selection (called when the transcript layout shifts).
    pub(crate) fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Effective expansion of a collapsible block: its override, else baseline.
    ///
    /// A click records a per-item override in [`Self::item_expanded`]; absent
    /// one, the block follows the ctrl+o baseline [`Self::details_expanded`].
    pub(crate) fn item_is_expanded(&self, id: &zhive_proto::domain::ItemId) -> bool {
        self.item_expanded
            .get(id)
            .copied()
            .unwrap_or(self.details_expanded)
    }

    /// Ends a left-button gesture, toggling a collapsible block on a plain click.
    ///
    /// A drag (the selection moved) finalizes the text selection; a click that
    /// did not move and landed on a collapsible block flips just that block's
    /// expansion instead.
    pub(crate) fn pointer_release(&mut self, col: u16, row: u16) {
        let was_click = matches!(self.selection, Some(sel) if sel.anchor == sel.cursor);
        self.selection_finish();
        if was_click {
            self.toggle_detail_at(col, row);
        }
    }

    /// Toggles the collapsible block whose rendered region contains `(col, row)`.
    ///
    /// A no-op when the click misses every recorded [`ToggleZone`]. Toggling
    /// shifts the lines below, so any selection is dropped (as ctrl+o does).
    fn toggle_detail_at(&mut self, col: u16, row: u16) {
        let Some((line_idx, _)) = hit_to_content(self.sel_geom.get(), col, row) else {
            return;
        };
        let hit = self
            .toggle_zones
            .borrow()
            .iter()
            .find(|z| line_idx >= z.start && line_idx < z.end)
            .map(|z| z.id.clone());
        let Some(id) = hit else {
            return;
        };
        let next = !self.item_is_expanded(&id);
        self.item_expanded.insert(id, next);
        self.clear_selection();
    }

    /// Takes the selected text and clears the selection.
    ///
    /// Returns `None` when there is no selection or it is empty, so the caller
    /// can fall through (e.g. `Ctrl+C` then quits).
    pub(crate) fn take_selection_text(&mut self) -> Option<String> {
        let sel = self.selection.take()?;
        let text = extract_selection(sel, &self.sel_lines.borrow());
        (!text.is_empty()).then_some(text)
    }

    /// Copies the most recent assistant message (opencode's `messages.copy`).
    ///
    /// Returns [`Action::Copy`] with the message text, or sets a flash and
    /// returns [`Action::None`] when there is no assistant message yet.
    fn copy_last_message(&mut self) -> Action {
        match self.conversation.last_agent_text() {
            Some(text) if !text.is_empty() => {
                self.flash = Some("copied last message".to_owned());
                Action::Copy(text)
            }
            _ => {
                self.flash = Some("no assistant message to copy".to_owned());
                Action::None
            }
        }
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
        // The line indices a selection referenced no longer exist.
        self.selection = None;
        // The old thread's item ids are gone; drop their expansion overrides.
        self.item_expanded.clear();
        // Token usage belongs to the old thread; a fresh thread has none yet, so
        // the top bar must not keep showing the previous thread's counts.
        self.last_usage = None;
    }

    /// Records that the engine connection was lost.
    ///
    /// Sets the persistent [`Self::disconnected`] flag.  The footer renderer
    /// will display a permanent "engine disconnected" banner until the process exits.
    pub fn on_disconnected(&mut self) {
        self.disconnected = true;
        self.conversation.busy = false;
        // Reset the partial stream + reveal cursor like every other busy->false
        // transition, so a mid-stream disconnect cannot strand them.
        self.conversation.clear_streaming();
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
        // Track in-progress / failed compaction for the streaming progress
        // panel. Auto and manual compaction both surface here.
        match event {
            EngineNotification::CompactionStarted {
                trigger, entries, ..
            } => {
                self.compaction = Some(CompactionView {
                    trigger: *trigger,
                    entries: *entries,
                    summary: String::new(),
                    error: None,
                });
            }
            EngineNotification::CompactionDelta { delta, .. } => {
                if let Some(view) = self.compaction.as_mut() {
                    view.summary.push_str(delta);
                }
            }
            EngineNotification::CompactionCompleted {
                entries_compacted, ..
            } => {
                // The marker + summary items arrive via ItemAppended; the live
                // panel has done its job, so drop it.
                self.compaction = None;
                // The pre-compaction token count no longer reflects the trimmed
                // context; clear it so the top bar updates immediately instead
                // of showing the stale (large) pre-compaction figure until the
                // next provider call reports fresh usage.
                self.last_usage = None;
                self.flash = Some(format!("compacted {entries_compacted} entries"));
            }
            EngineNotification::CompactionFailed { reason, .. } => {
                // Keep a panel carrying the reason so the failure is visible in
                // the transcript, not just a transient flash.
                match self.compaction.as_mut() {
                    Some(view) => view.error = Some(reason.clone()),
                    None => {
                        self.compaction = Some(CompactionView {
                            trigger: zhive_proto::hook::CompactTrigger::Manual,
                            entries: 0,
                            summary: String::new(),
                            error: Some(reason.clone()),
                        });
                    }
                }
                self.flash = Some(format!("compact failed: {reason}"));
            }
            // A new turn supersedes any lingering compaction panel.
            EngineNotification::TurnStarted { .. } => {
                self.compaction = None;
            }
            _ => {}
        }
        self.conversation.apply(event);
        // New output does NOT yank the view to the tail. When the user is at the
        // bottom (`scrollback == 0`) the renderer keeps following it; when they
        // have scrolled up to read history, the renderer anchors their position
        // so streaming output below the fold cannot snatch it away.
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
            Some(Overlay::Help | Overlay::Settings) | None => Action::None,
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
            Some(Overlay::ModelList {
                models,
                selected,
                query,
            }) => self.on_model_list_key(key, models, selected, query),
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

    /// Key handling for the `/models` picker: navigate, filter, switch, cancel.
    ///
    /// The overlay was `take`n by the caller; this re-installs it (with updated
    /// state) for every key except Enter (hot-swap the highlighted model) and
    /// Esc (cancel). Navigation and filtering keep the highlight within the
    /// filtered rows.
    fn on_model_list_key(
        &mut self,
        key: KeyEvent,
        models: Vec<zhive_proto::rpc::ModelDescriptor>,
        selected: usize,
        mut query: String,
    ) -> Action {
        let filtered = filter_models(&models, &query);
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
                // Clone the chosen model (ends the `filtered` borrow) before
                // returning the switch action.
                let chosen = filtered.get(selected).map(|m| (*m).clone());
                if let Some(model) = chosen {
                    return Action::SwitchModel {
                        model: Box::new(model),
                    };
                }
                // Empty list: nothing to switch to; keep the overlay open.
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
        self.overlay = Some(Overlay::ModelList {
            models,
            selected,
            query,
        });
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
        let mention = self.mention_query().is_some();

        match key.code {
            // Ctrl+C copies an active transcript selection (opencode parity).
            // With no selection it clears a non-empty composer, or does nothing
            // on an empty one — it never quits. Quitting is Ctrl+D's job, on a
            // blank composer, so an unfinished message is never lost to a stray
            // Ctrl+C.
            KeyCode::Char('c') if ctrl => {
                if let Some(text) = self.take_selection_text() {
                    self.flash = Some("copied selection".to_owned());
                    Action::Copy(text)
                } else if !self.input.is_blank() {
                    self.input.clear();
                    self.palette_index = 0;
                    Action::None
                } else {
                    Action::None
                }
            }
            KeyCode::Char('d') if ctrl && self.input.is_blank() => Action::Quit,
            // Esc clears an active selection first (before turn/queue handling).
            KeyCode::Esc if self.has_selection() => {
                self.clear_selection();
                Action::None
            }
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
            // The `@`-mention file picker mirrors the slash palette: arrows or
            // Ctrl+P/Ctrl+N move the highlight, Tab/Enter insert the path. These
            // precede the generic Enter/Up/Down arms and are mutually exclusive
            // with the palette (a mention is suppressed while a command is typed).
            KeyCode::Up if mention => {
                self.mention_move(-1);
                Action::None
            }
            KeyCode::Char('p') if ctrl && mention => {
                self.mention_move(-1);
                Action::None
            }
            KeyCode::Down if mention => {
                self.mention_move(1);
                Action::None
            }
            KeyCode::Char('n') if ctrl && mention => {
                self.mention_move(1);
                Action::None
            }
            KeyCode::Tab if mention => self.mention_accept(),
            KeyCode::Enter if mention && !alt => self.mention_accept(),
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
            // Flip the expansion baseline for every collapsible block at once,
            // discarding any per-click overrides so ctrl+o is a true "all".
            KeyCode::Char('o') if ctrl => {
                self.details_expanded = !self.details_expanded;
                self.item_expanded.clear();
                // Expanding/collapsing shifts every line below, so a selection's
                // absolute line indices would point at the wrong rows.
                self.clear_selection();
                Action::None
            }
            // Cycle reasoning depth: Off → Low → Medium → High → Xhigh → Off.
            KeyCode::Char('t') if ctrl => {
                self.cycle_thinking_effort();
                Action::None
            }
            KeyCode::Char(c) if !ctrl => {
                self.input.insert_char(c);
                self.palette_index = 0;
                self.mention_index = 0;
                Action::None
            }
            KeyCode::Backspace => {
                self.input.backspace();
                self.palette_index = 0;
                self.mention_index = 0;
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
            // ctrl+Home / ctrl+End jump the transcript to the very top / tail.
            // Placed before the bare Home/End cursor moves so the guard wins.
            KeyCode::Home if ctrl => {
                // The rendered max is the oldest scrollable line; pin to it so
                // later relative scrolling resumes cleanly from the top.
                self.scrollback = self.viewport_max_scroll.get();
                Action::None
            }
            KeyCode::End if ctrl => {
                self.scrollback = 0;
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
                // A deliberate send returns to the tail to show the new turn.
                self.scrollback = 0;
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
        // A deliberate send returns to the tail to show the new turn.
        self.scrollback = 0;
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
            "copy" => self.copy_last_message(),
            // Open the picker in the default scope (All); Tab toggles to cwd.
            "session" | "resume" => Action::OpenSessionList {
                filter: SessionFilter::default(),
            },
            "help" | "?" => {
                self.overlay = Some(Overlay::Help);
                Action::None
            }
            // Open the model picker (populated by an async `models/list`). The
            // `models` plural is kept as a hidden alias for muscle memory.
            "model" | "models" => Action::OpenModelList,
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
        self.render_cache.clear();
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
        self.render_cache.clear();
        self.flash = Some(format!("accent: {name:?}"));
    }
}

/// Filters session entries by a case-insensitive substring of id, title, or
/// preview.
///
/// An empty query matches everything, preserving the newest-first order. The
/// thread id is matched too so the id printed on the exit farewell is a working
/// search key (the user can paste it to find the session to resume). Used by
/// both the `/session` key handler and the overlay renderer so the highlighted
/// index always lines up with what is drawn.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_tui::app::filter_sessions;
/// use zhive_tui::rpc::SessionEntry;
/// use zhive_proto::domain::ThreadId;
/// let entries = vec![SessionEntry {
///     id: ThreadId(Arc::from("thread:native/1749106321000-0")),
///     title: Some("Refactor".to_owned()),
///     preview: "parser cleanup".to_owned(),
///     updated_at: 0,
///     subagent_parent: None,
/// }];
/// assert_eq!(filter_sessions(&entries, "refac").len(), 1);
/// // The exit-printed thread id is searchable verbatim.
/// assert_eq!(filter_sessions(&entries, "native/1749106321000-0").len(), 1);
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
            e.id.0.to_lowercase().contains(&needle)
                || e.title
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

/// Filters models by a case-insensitive substring over id + display name.
///
/// An empty query keeps every model in endpoint order.
///
/// # Examples
///
/// ```
/// use zhive_tui::app::filter_models;
/// use zhive_proto::rpc::ModelDescriptor;
/// let models = vec![
///     ModelDescriptor::new("claude-opus-4-8".to_owned())
///         .with_display_name(Some("Claude Opus 4.8".to_owned())),
///     ModelDescriptor::new("gpt-4o".to_owned()),
/// ];
/// assert_eq!(filter_models(&models, "opus").len(), 1);
/// assert_eq!(filter_models(&models, "gpt").len(), 1);
/// assert_eq!(filter_models(&models, "").len(), 2);
/// ```
#[must_use]
pub fn filter_models<'a>(
    models: &'a [zhive_proto::rpc::ModelDescriptor],
    query: &str,
) -> Vec<&'a zhive_proto::rpc::ModelDescriptor> {
    if query.is_empty() {
        return models.iter().collect();
    }
    let needle = query.to_lowercase();
    models
        .iter()
        .filter(|m| {
            m.id.to_lowercase().contains(&needle)
                || m.display_name
                    .as_deref()
                    .is_some_and(|d| d.to_lowercase().contains(&needle))
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
    fn ctrl_home_end_jump_transcript_to_top_and_tail() {
        let mut a = app();
        // Simulate a rendered frame whose oldest scrollable line is 42 rows up.
        a.viewport_max_scroll.set(42);

        // ctrl+End pins to the live tail.
        a.scrollback = 17;
        assert_eq!(a.on_key(ctrl(KeyCode::End)), Action::None);
        assert_eq!(a.scrollback, 0);

        // ctrl+Home pins to the rendered maximum (the very top).
        assert_eq!(a.on_key(ctrl(KeyCode::Home)), Action::None);
        assert_eq!(a.scrollback, 42);

        // Bare Home/End move the input cursor and leave scrollback untouched.
        assert_eq!(a.on_key(key(KeyCode::Home)), Action::None);
        assert_eq!(a.scrollback, 42);
        assert_eq!(a.on_key(key(KeyCode::End)), Action::None);
        assert_eq!(a.scrollback, 42);
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

    /// Two sample models for the picker tests: an active Opus and a Haiku.
    fn sample_models() -> Vec<zhive_proto::rpc::ModelDescriptor> {
        use zhive_proto::domain::ThinkingEffort::{High, Low, Medium, Off};
        vec![
            zhive_proto::rpc::ModelDescriptor::new("claude-opus-4-8".to_owned())
                .with_display_name(Some("Claude Opus 4.8".to_owned()))
                .with_context_window(Some(1_000_000))
                .with_supported_efforts(vec![Off, Low, Medium, High])
                .with_active(true),
            zhive_proto::rpc::ModelDescriptor::new("claude-haiku-4-5".to_owned())
                .with_context_window(Some(200_000)),
        ]
    }

    fn open_model_list(a: &mut App) {
        a.overlay = Some(Overlay::ModelList {
            models: sample_models(),
            selected: 0,
            query: String::new(),
        });
    }

    #[test]
    fn slash_models_requests_the_picker() {
        let mut a = app();
        assert_eq!(a.run_slash("models"), Action::OpenModelList);
    }

    #[test]
    fn model_picker_enter_returns_switch_for_highlighted() {
        let mut a = app();
        open_model_list(&mut a);
        match a.on_overlay_key(key(KeyCode::Enter)) {
            Action::SwitchModel { model } => assert_eq!(model.id, "claude-opus-4-8"),
            other => panic!("expected SwitchModel, got {other:?}"),
        }
    }

    #[test]
    fn model_picker_filters_then_switches() {
        let mut a = app();
        open_model_list(&mut a);
        for c in "haiku".chars() {
            a.on_overlay_key(key(KeyCode::Char(c)));
        }
        match a.on_overlay_key(key(KeyCode::Enter)) {
            Action::SwitchModel { model } => assert_eq!(model.id, "claude-haiku-4-5"),
            other => panic!("expected SwitchModel, got {other:?}"),
        }
    }

    #[test]
    fn model_picker_esc_cancels() {
        let mut a = app();
        open_model_list(&mut a);
        assert_eq!(a.on_overlay_key(key(KeyCode::Esc)), Action::None);
        assert!(a.overlay.is_none());
    }

    #[test]
    fn apply_model_switch_clamps_unsupported_effort() {
        use zhive_proto::domain::ThinkingEffort;
        let mut a = app();
        a.thinking_effort = ThinkingEffort::High;
        // The new model supports only Off/Low → High is no longer valid.
        a.apply_model_switch(
            "m-low".to_owned(),
            vec![ThinkingEffort::Off, ThinkingEffort::Low],
        );
        assert_eq!(a.config.model_label, "m-low");
        assert_eq!(a.thinking_effort, ThinkingEffort::Off);
        // Cycling now follows the live cycle: Off → Low.
        a.cycle_thinking_effort();
        assert_eq!(a.thinking_effort, ThinkingEffort::Low);
    }

    #[test]
    fn boot_seeds_live_cycle_and_keeps_supported_depth() {
        use zhive_proto::domain::{ThinkingEffort, ThreadId};
        // A restored High depth with a live cycle that supports it: kept, the
        // live cycle is installed, and nothing is marked dirty by boot alone.
        let config = TuiConfig {
            thinking_effort: ThinkingEffort::High,
            effort_cycle: Some(vec![
                ThinkingEffort::Off,
                ThinkingEffort::Low,
                ThinkingEffort::High,
            ]),
            ..Default::default()
        };
        let a = App::new(config, ThreadId(Arc::from("thread:native/t")));
        assert_eq!(a.thinking_effort, ThinkingEffort::High);
        assert!(a.active_effort_cycle.is_some());
        assert!(
            !a.selection_dirty,
            "boot clamp must not mark the session dirty"
        );
    }

    #[test]
    fn boot_clamps_depth_unsupported_by_live_cycle() {
        use zhive_proto::domain::{ThinkingEffort, ThreadId};
        // Restored High but the live cycle tops out at Low → clamp to Off.
        let config = TuiConfig {
            thinking_effort: ThinkingEffort::High,
            effort_cycle: Some(vec![ThinkingEffort::Off, ThinkingEffort::Low]),
            ..Default::default()
        };
        let a = App::new(config, ThreadId(Arc::from("thread:native/t")));
        assert_eq!(a.thinking_effort, ThinkingEffort::Off);
        assert!(!a.selection_dirty);
    }

    #[test]
    fn apply_model_switch_keeps_supported_effort() {
        use zhive_proto::domain::ThinkingEffort;
        let mut a = app();
        a.thinking_effort = ThinkingEffort::Low;
        a.apply_model_switch(
            "m".to_owned(),
            vec![
                ThinkingEffort::Off,
                ThinkingEffort::Low,
                ThinkingEffort::Medium,
            ],
        );
        // Low is in the new model's set, so it survives the switch.
        assert_eq!(a.thinking_effort, ThinkingEffort::Low);
        assert_eq!(
            a.active_effort_cycle.as_deref(),
            Some(
                [
                    ThinkingEffort::Off,
                    ThinkingEffort::Low,
                    ThinkingEffort::Medium
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn item_override_beats_baseline_and_ctrl_o_clears_it() {
        let mut a = app();
        let id = zhive_proto::domain::ItemId(Arc::from("item:x"));
        let other = zhive_proto::domain::ItemId(Arc::from("item:y"));
        // A per-item override expands just this block off the collapsed baseline.
        a.item_expanded.insert(id.clone(), true);
        assert!(a.item_is_expanded(&id), "override wins over baseline");
        assert!(!a.item_is_expanded(&other), "others follow the baseline");
        // ctrl+o flips the baseline for all blocks and drops every override.
        a.on_key(ctrl(KeyCode::Char('o')));
        assert!(a.details_expanded);
        assert!(
            a.item_expanded.is_empty(),
            "ctrl+o clears per-item overrides"
        );
        assert!(a.item_is_expanded(&id));
        assert!(a.item_is_expanded(&other));
    }

    #[test]
    fn new_output_does_not_yank_scrolled_up_view() {
        let mut a = app();
        let turn = start_turn(&mut a);
        let tid = a.conversation.thread_id.clone();
        // The user scrolled up to read history.
        a.scrollback = 10;
        // Streaming output lands; the view must NOT snap back to the tail.
        a.on_engine(&EngineNotification::ItemAppended {
            thread_id: tid,
            turn_id: turn,
            item: Box::new(zhive_proto::domain::Item::AgentMessage {
                id: zhive_proto::domain::ItemId(Arc::from("item:a0")),
                text: "streamed".to_owned(),
            }),
        });
        assert_eq!(a.scrollback, 10, "scrolled-up view preserved on new output");
    }

    #[test]
    fn deliberate_submit_returns_to_tail() {
        let mut a = app();
        // The user had scrolled up before composing a message.
        a.scrollback = 25;
        type_text(&mut a, "hello");
        assert!(matches!(a.submit(), Action::Submit(_)));
        assert_eq!(a.scrollback, 0, "sending a message snaps back to the tail");
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
    fn on_engine_drives_compaction_panel_lifecycle() {
        use zhive_proto::hook::CompactTrigger;
        let mut app = app();
        let tid = app.conversation.thread_id.clone();

        app.on_engine(&EngineNotification::CompactionStarted {
            thread_id: tid.clone(),
            trigger: CompactTrigger::Manual,
            entries: 5,
        });
        let view = app.compaction.as_ref().expect("panel opens on start");
        assert_eq!(view.entries, 5);
        assert!(view.error.is_none());
        assert!(view.summary.is_empty());

        app.on_engine(&EngineNotification::CompactionDelta {
            thread_id: tid.clone(),
            delta: "Hello ".to_owned(),
        });
        app.on_engine(&EngineNotification::CompactionDelta {
            thread_id: tid.clone(),
            delta: "world".to_owned(),
        });
        assert_eq!(app.compaction.as_ref().unwrap().summary, "Hello world");

        // Completed clears the live panel; the persisted summary arrives as an
        // ItemAppended item the conversation reducer renders normally.
        app.on_engine(&EngineNotification::CompactionCompleted {
            thread_id: tid,
            entries_compacted: 5,
        });
        assert!(app.compaction.is_none());
        assert_eq!(app.flash.as_deref(), Some("compacted 5 entries"));
    }

    #[test]
    fn on_engine_compaction_failure_persists_reason() {
        use zhive_proto::hook::CompactTrigger;
        let mut app = app();
        let tid = app.conversation.thread_id.clone();
        app.on_engine(&EngineNotification::CompactionStarted {
            thread_id: tid.clone(),
            trigger: CompactTrigger::Auto,
            entries: 3,
        });
        app.on_engine(&EngineNotification::CompactionFailed {
            thread_id: tid.clone(),
            reason: "provider exploded".to_owned(),
        });
        // Failure keeps the panel with the reason so it stays visible.
        let view = app.compaction.as_ref().expect("panel persists on failure");
        assert_eq!(view.error.as_deref(), Some("provider exploded"));
        assert_eq!(
            app.flash.as_deref(),
            Some("compact failed: provider exploded")
        );

        // A new turn supersedes the lingering failure panel.
        app.on_engine(&EngineNotification::TurnStarted {
            thread_id: tid,
            turn_id: turn_id("turn:native/t/0"),
        });
        assert!(app.compaction.is_none());
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
    fn ctrl_c_clears_input_but_never_quits() {
        let mut app = app();
        // Empty composer, no selection: Ctrl+C is a no-op (it must not quit).
        assert_eq!(app.on_key(ctrl(KeyCode::Char('c'))), Action::None);
        // With composer content: Ctrl+C clears it and stays on screen.
        for c in "draft".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_key(ctrl(KeyCode::Char('c'))), Action::None);
        assert!(app.input.is_blank(), "Ctrl+C cleared the composer");
        // Quitting is Ctrl+D's job, on a blank composer.
        assert_eq!(app.on_key(ctrl(KeyCode::Char('d'))), Action::Quit);
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
    fn slash_model_requests_the_picker() {
        // `/model` now opens the switch picker (the read-only info overlay is
        // gone); the async `OpenModelList` action populates it from `models/list`.
        let mut app = app();
        for c in "/model".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(app.on_key(key(KeyCode::Enter)), Action::OpenModelList);
        assert!(app.overlay.is_none());
    }

    #[test]
    fn slash_model_singular_and_plural_both_open_picker() {
        let mut a = app();
        assert_eq!(a.run_slash("model"), Action::OpenModelList);
        assert_eq!(a.run_slash("models"), Action::OpenModelList);
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

    #[test]
    fn reset_thread_clears_last_usage() {
        let mut app = app();
        app.on_engine(&EngineNotification::Usage {
            input_tokens: 120,
            output_tokens: 45,
        });
        app.reset_thread(ThreadId(Arc::from("thread:native/t2")));
        assert!(
            app.last_usage.is_none(),
            "a fresh thread must not carry the old thread's token counts"
        );
    }

    #[test]
    fn compaction_completed_clears_last_usage() {
        use zhive_proto::hook::CompactTrigger;
        let mut app = app();
        let tid = app.conversation.thread_id.clone();
        app.on_engine(&EngineNotification::Usage {
            input_tokens: 5_000,
            output_tokens: 200,
        });
        app.on_engine(&EngineNotification::CompactionStarted {
            thread_id: tid.clone(),
            trigger: CompactTrigger::Manual,
            entries: 5,
        });
        app.on_engine(&EngineNotification::CompactionCompleted {
            thread_id: tid,
            entries_compacted: 5,
        });
        assert!(
            app.last_usage.is_none(),
            "compaction trims the context; the stale pre-compaction count must clear"
        );
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

    // --- transcript selection helpers ---

    fn sel(anchor: (usize, u16), cursor: (usize, u16)) -> Selection {
        Selection {
            anchor,
            cursor,
            dragging: false,
        }
    }

    #[test]
    fn slice_by_cells_ascii_range() {
        assert_eq!(slice_by_cells("hello world", 0, 5), "hello");
        assert_eq!(slice_by_cells("hello world", 6, 11), "world");
        assert_eq!(slice_by_cells("hello", 2, 100), "llo");
        assert_eq!(slice_by_cells("hello", 3, 3), ""); // empty range
        assert_eq!(slice_by_cells("hello", 10, 12), ""); // starts past end
    }

    #[test]
    fn slice_by_cells_is_width_aware() {
        // CJK chars are width 2; a one-cell start inside one still takes it whole.
        assert_eq!(slice_by_cells("你好", 0, 2), "你");
        assert_eq!(slice_by_cells("你好", 2, 4), "好");
        assert_eq!(slice_by_cells("你好", 0, 1), "你");
    }

    #[test]
    fn extract_selection_single_and_multi_line() {
        let lines = vec![
            "hello world".to_owned(),
            "second line".to_owned(),
            "third".to_owned(),
        ];
        assert_eq!(extract_selection(sel((0, 0), (0, 5)), &lines), "hello");
        assert_eq!(
            extract_selection(sel((0, 6), (2, 3)), &lines),
            "world\nsecond line\nthi"
        );
        // Reversed endpoints normalize to the same span.
        assert_eq!(
            extract_selection(sel((2, 3), (0, 6)), &lines),
            "world\nsecond line\nthi"
        );
    }

    #[test]
    fn extract_selection_clamps_stale_indices() {
        let lines = vec!["only".to_owned()];
        assert_eq!(extract_selection(sel((5, 0), (9, 4)), &lines), "only");
        assert_eq!(extract_selection(sel((0, 0), (0, 0)), &lines), ""); // zero-width
    }

    #[test]
    fn cell_range_for_line_boundaries() {
        let s = sel((1, 2), (3, 4));
        assert_eq!(cell_range_for_line(s, 0, 10), None); // above the selection
        assert_eq!(cell_range_for_line(s, 1, 10), Some((2, 10))); // first → to EOL
        assert_eq!(cell_range_for_line(s, 2, 10), Some((0, 10))); // interior → whole
        assert_eq!(cell_range_for_line(s, 3, 10), Some((0, 4))); // last → to cursor
        assert_eq!(cell_range_for_line(s, 4, 10), None); // below the selection
    }

    #[test]
    fn hit_to_content_inside_and_gutter() {
        let geom = SelGeom::new(Rect::new(0, 0, 40, 10), 5);
        // row 2 → line 7; col 5 → content cell 3 (col − gutter 2).
        assert_eq!(hit_to_content(geom, 5, 2), Some((7, 3)));
        // A click in the gutter clamps to content cell 0.
        assert_eq!(hit_to_content(geom, 1, 0), Some((5, 0)));
        // Outside the body → no hit.
        assert_eq!(hit_to_content(geom, 50, 2), None);
    }

    #[test]
    fn selection_drag_then_copy() {
        let mut a = app();
        a.sel_geom.set(SelGeom::new(Rect::new(0, 0, 40, 10), 0));
        *a.sel_lines.borrow_mut() = vec!["hello world".to_owned(), "next".to_owned()];
        a.selection_start(2, 0); // line 0, content cell 0
        a.selection_update(6, 0); // drag to content cell 4
        a.selection_finish();
        assert!(a.has_selection());
        assert_eq!(a.take_selection_text().as_deref(), Some("hell"));
        assert!(!a.has_selection()); // taken
    }

    #[test]
    fn plain_click_selects_nothing() {
        let mut a = app();
        a.sel_geom.set(SelGeom::new(Rect::new(0, 0, 40, 10), 0));
        a.selection_start(5, 0);
        a.selection_finish(); // no drag → dropped
        assert!(!a.has_selection());
    }

    #[test]
    fn ctrl_c_copies_selection_else_clears_input() {
        let mut a = app();
        // No selection, empty composer: Ctrl+C does nothing (never quits).
        assert_eq!(a.on_key(ctrl(KeyCode::Char('c'))), Action::None);
        // With a selection: Ctrl+C copies its text and consumes the selection.
        a.sel_geom.set(SelGeom::new(Rect::new(0, 0, 40, 10), 0));
        *a.sel_lines.borrow_mut() = vec!["copy me".to_owned()];
        a.selection_start(2, 0);
        a.selection_update(9, 0);
        a.selection_finish();
        assert_eq!(
            a.on_key(ctrl(KeyCode::Char('c'))),
            Action::Copy("copy me".to_owned())
        );
        assert!(!a.has_selection());
    }

    #[test]
    fn copy_last_message_command() {
        let mut a = app();
        // No assistant message yet.
        assert_eq!(a.run_slash("copy"), Action::None);
        // After an agent message, /copy yields its text. Seed via the public
        // history loader so the item lands in a turn keyed off its encoded id.
        a.conversation
            .load_history(vec![zhive_proto::domain::Item::AgentMessage {
                id: zhive_proto::domain::ItemId(Arc::from("item:t/1/0")),
                text: "the answer".to_owned(),
            }]);
        assert_eq!(a.run_slash("copy"), Action::Copy("the answer".to_owned()));
    }

    // ---- @ file mention ----

    fn app_with_files(files: &[&str]) -> App {
        let mut a = app();
        a.set_file_index(files.iter().map(|s| (*s).to_owned()).collect());
        a
    }

    #[test]
    fn typing_at_activates_mention_after_whitespace_only() {
        let mut a = app();
        // Mid-word `@` (e.g. an email) does not open the picker.
        for c in "mail@host".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(a.mention_query(), None);
        // A fresh token after a space does.
        a.on_key(key(KeyCode::Char(' ')));
        a.on_key(key(KeyCode::Char('@')));
        a.on_key(key(KeyCode::Char('s')));
        assert_eq!(a.mention_query(), Some("s"));
    }

    #[test]
    fn mention_enter_inserts_path_instead_of_submitting() {
        let mut a = app_with_files(&["src/main.rs", "src/lib.rs"]);
        for c in "see @lib".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        // Enter accepts the highlighted match rather than sending the message.
        assert_eq!(a.on_key(key(KeyCode::Enter)), Action::None);
        assert_eq!(a.input.value(), "see @src/lib.rs ");
        // The trailing space closed the picker.
        assert_eq!(a.mention_query(), None);
    }

    #[test]
    fn mention_arrow_then_accept_picks_second_row() {
        let mut a = app_with_files(&["alpha.rs", "alphabet.rs"]);
        for c in "@alpha".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        // Two matches; Down moves to the second (longer) one before accepting.
        a.on_key(key(KeyCode::Down));
        a.on_key(key(KeyCode::Tab));
        assert_eq!(a.input.value(), "@alphabet.rs ");
    }

    #[test]
    fn mention_suppressed_while_slash_palette_open() {
        let mut a = app();
        for c in "/he @x".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        // A space after `/he` already closed the palette, so `@x` is a mention.
        assert_eq!(a.mention_query(), Some("x"));
        // But a pure command name keeps the slash palette and no mention.
        let mut b = app();
        for c in "/help".chars() {
            b.on_key(key(KeyCode::Char(c)));
        }
        assert!(b.palette_query().is_some());
        assert_eq!(b.mention_query(), None);
    }

    #[test]
    fn needs_file_index_only_while_mention_active_and_unbuilt() {
        let mut a = app();
        assert!(!a.needs_file_index());
        a.on_key(key(KeyCode::Char('@')));
        assert!(a.needs_file_index());
        a.set_file_index(vec!["x.rs".to_owned()]);
        assert!(!a.needs_file_index());
    }
}

// Rust guideline compliant 2026-02-21
