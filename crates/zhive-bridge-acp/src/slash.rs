//! Slash-command parsing for ACP prompt requests.
//!
//! When an ACP client sends a `session/prompt` whose sole content block is a
//! `Text` starting with `/`, this module parses the command name and returns
//! the [`SlashAction`] the bridge should take instead of starting a normal LLM
//! turn.
//!
//! # Supported commands
//!
//! | Command | Action |
//! |---------|--------|
//! | `/compact` | Compact the session's transcript |
//! | `/new`, `/clear` | Rebind the session to a fresh thread |
//! | `/skills` | List discovered skills |
//! | `/help`, `/?` | Show available slash commands |
//! | `/<name>`, `/skill:<name>` | Run a skill (requires `skills` feature) |
//! | anything else | `Unknown` (bridge sends an error notification) |

use std::sync::Arc;

use agent_client_protocol::schema::ContentBlock;

/// A skill available for slash-command dispatch.
///
/// Each entry pairs a command name with the pre-rendered `<skill>` invocation
/// block that will be injected as the user message when the skill is run.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
/// use zhive_bridge_acp::slash::Skill;
///
/// let s = Skill {
///     name: Arc::from("commit"),
///     invocation: Arc::from("<skill name=\"commit\" location=\"/x/SKILL.md\">\nbody\n</skill>"),
/// };
/// assert_eq!(&*s.name, "commit");
/// ```
#[derive(Debug, Clone)]
pub struct Skill {
    /// Slash-command name without the leading `/` (matches the SKILL.md `name`).
    pub name: Arc<str>,
    /// Pre-rendered `<skill>…</skill>` invocation block; no trailing args.
    pub invocation: Arc<str>,
}

/// The action the bridge should take for a prompt starting with `/`.
///
/// Produced by [`parse_prompt`]. The bridge matches on this and dispatches
/// accordingly, bypassing the normal LLM turn path.
///
/// # Examples
///
/// ```
/// use agent_client_protocol::schema::{ContentBlock, TextContent};
/// use zhive_bridge_acp::slash::{Skill, SlashAction, parse_prompt};
///
/// let blocks = vec![ContentBlock::Text(TextContent::new("/compact"))];
/// assert!(matches!(parse_prompt(&blocks, &[]), Some(SlashAction::Compact)));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashAction {
    /// Compact the session's transcript via [`Engine::compact`].
    Compact,
    /// Rebind the session to a fresh thread (clear conversation history).
    Clear,
    /// List all available skills as an agent text notification.
    ListSkills,
    /// Show available slash commands as an agent text notification.
    Help,
    /// Inject `invocation` as the user message and start a normal LLM turn.
    RunSkill {
        /// Pre-rendered `<skill>…</skill>` block, with any user args appended.
        invocation: String,
    },
    /// Command not recognised; the bridge sends an error notification.
    Unknown {
        /// Raw command name after the `/`, without leading slash.
        cmd: String,
    },
}

/// Parses `blocks` as a slash command if the prompt qualifies.
///
/// Returns `Some(action)` when the **first** [`ContentBlock`] is a
/// [`ContentBlock::Text`] whose content starts with `/` (after trimming leading
/// whitespace). Additional blocks after the slash text are ignored for command
/// routing — ACP clients may attach workspace context or file attachments
/// alongside the slash text, and those extra blocks must not suppress detection.
/// Returns `None` when the first block is not a text block or its text does not
/// start with `/`, indicating a normal LLM turn.
///
/// `catalogue` is the set of skills available for slash dispatch. Pass an empty
/// slice when the `skills` feature is disabled or no skills are installed.
///
/// Skill matching: the command name is tried first as a `skill:` prefix form
/// (`/skill:<name>`), then as a bare name match against the catalogue. Built-in
/// commands take precedence over skills with the same name.
///
/// # Examples
///
/// ```
/// use agent_client_protocol::schema::{ContentBlock, TextContent};
/// use zhive_bridge_acp::slash::{Skill, SlashAction, parse_prompt};
///
/// // `/compact` → compact
/// let blocks = vec![ContentBlock::Text(TextContent::new("/compact"))];
/// assert_eq!(parse_prompt(&blocks, &[]), Some(SlashAction::Compact));
///
/// // `/new` → clear
/// let blocks = vec![ContentBlock::Text(TextContent::new("/new"))];
/// assert_eq!(parse_prompt(&blocks, &[]), Some(SlashAction::Clear));
///
/// // Plain text → None (normal LLM turn, not intercepted)
/// let plain = vec![ContentBlock::Text(TextContent::new("hello"))];
/// assert_eq!(parse_prompt(&plain, &[]), None);
///
/// // Unknown command
/// let unknown = vec![ContentBlock::Text(TextContent::new("/noop"))];
/// assert_eq!(
///     parse_prompt(&unknown, &[]),
///     Some(SlashAction::Unknown { cmd: "noop".to_owned() }),
/// );
///
/// // Skill by bare name
/// use std::sync::Arc;
/// let skill = Skill { name: Arc::from("commit"), invocation: Arc::from("<skill>body</skill>") };
/// let blocks = vec![ContentBlock::Text(TextContent::new("/commit"))];
/// assert!(matches!(parse_prompt(&blocks, &[skill]), Some(SlashAction::RunSkill { .. })));
///
/// // Extra blocks alongside a slash command are allowed (e.g. file attachments).
/// let with_extra = vec![
///     ContentBlock::Text(TextContent::new("/compact")),
///     ContentBlock::Text(TextContent::new("some context")),
/// ];
/// assert_eq!(parse_prompt(&with_extra, &[]), Some(SlashAction::Compact));
/// ```
#[must_use]
pub fn parse_prompt(blocks: &[ContentBlock], catalogue: &[Skill]) -> Option<SlashAction> {
    // Intercept when the first block is a text block starting with `/`.
    // Extra trailing blocks are permitted so clients that attach workspace
    // context or file contents alongside a slash command still get routed.
    let Some(ContentBlock::Text(text)) = blocks.first() else {
        return None;
    };
    let raw = text.text.trim_start();
    let cmd_str = raw.strip_prefix('/')?;

    // Split into `name` (up to first whitespace) and optional trailing `args`.
    let (name, args) = cmd_str
        .split_once(char::is_whitespace)
        .map_or((cmd_str, ""), |(n, a)| (n, a.trim()));

    Some(match name {
        "compact" => SlashAction::Compact,
        "new" | "clear" => SlashAction::Clear,
        "skills" => SlashAction::ListSkills,
        "help" | "?" => SlashAction::Help,
        other => {
            // Strip the explicit `skill:` prefix if present.
            let skill_name = other.strip_prefix("skill:").unwrap_or(other);
            if let Some(skill) = catalogue.iter().find(|s| s.name.as_ref() == skill_name) {
                let invocation = if args.is_empty() {
                    skill.invocation.to_string()
                } else {
                    format!("{}\n\n{args}", skill.invocation)
                };
                SlashAction::RunSkill { invocation }
            } else {
                SlashAction::Unknown {
                    cmd: other.to_owned(),
                }
            }
        }
    })
}

// Rust guideline compliant 2026-02-21
