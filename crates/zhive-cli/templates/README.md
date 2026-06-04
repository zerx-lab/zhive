# zhive prompt templates

zhive renders its system / compaction prompts from [Jinja2](https://jinja.palletsprojects.com/)
(`.j2`) templates via [minijinja](https://docs.rs/minijinja). The files in this
directory are the **embedded defaults**, compiled into the `zhive` binary, so a
default prompt is always present.

## Override layers

A template named `<name>` (e.g. `system/base`) resolves through these layers,
where a later layer fully replaces an earlier one:

1. **Embedded default** — the files here, baked into the binary.
2. **User override** — `$XDG_CONFIG_HOME/zhive/templates/<name>.j2`
   (falls back to `~/.config/zhive/templates/<name>.j2`).
3. **Project override** — `./.zhive/templates/<name>.j2`, relative to the
   directory `zhive` is launched from. **Highest precedence.**

Overrides are loaded once at startup. A missing override is silently ignored; a
read error or a broken template is logged and zhive falls back to the embedded
default, so a bad override never breaks a session. Edit a `.j2` file and restart
to apply changes.

## Templates

| Name | File | Purpose |
| --- | --- | --- |
| `system/base` | `system/base.j2` | The system prompt skeleton: persona + environment + project instructions. |
| `system/persona` | `system/persona.j2` | The default assistant persona. |
| `system/persona.<kind>` / `system/persona.<name>` | *(add your own)* | Per-provider persona override (see below). |

## Per-provider personas

`system/base` selects the persona partial for the active provider with this
precedence:

1. `system/persona.<provider_name>` — the entry name from `[provider.<name>]`
   in `config.toml` (e.g. `system/persona.my-proxy`).
2. `system/persona.<provider_kind>` — the backend type (e.g.
   `system/persona.openai`, `system/persona.anthropic`). More stable than the
   name, which a user can rename.
3. `system/persona` — the default, always present.

To ship a different persona for OpenAI-backed providers, create
`~/.config/zhive/templates/system/persona.openai.j2`. No embedded per-provider
personas are shipped, so the default behavior is identical for every provider
until you add one.

## Available variables

These are exposed to `system/*` templates:

| Variable | Type | Notes |
| --- | --- | --- |
| `cwd` | string | Working directory. |
| `os` | string | Host OS (`linux`, `macos`, `windows`, …). |
| `provider_name` | string | Active `[provider.<name>]` entry name. |
| `provider_kind` | string | Active provider backend kind. |
| `model` | string \| none | Active model id, when known. |
| `project_instructions` | object \| none | `{ source, body, truncated }` for the nearest `AGENTS.md` / `CLAUDE.md`, or `none`. |
| `persona_template` | string | Resolved persona partial name (used by `system/base`). |

Example snippet using a variable and a conditional:

```jinja
{% include persona_template %}

# Environment
- Working directory: {{ cwd }}
- Operating system: {{ os }}
{% if project_instructions %}
# Project instructions
Source: {{ project_instructions.source }}

{{ project_instructions.body }}{% endif %}
```
