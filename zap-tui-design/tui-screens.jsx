/* global React, Panel, TopBar, BottomBar, Msg, ToolBlock, CodeLine, Diff, WD, Bar, Spinner, Cursor, Overlay */
/* zap-tui · snapshot screens (welcome, model, sessions, mcp, settings, approval, files, diff-only, tool-stream) */

const { useState, useEffect, useMemo } = React;

// ============================================================
// shared shell
// ============================================================
function Shell({ theme = "dark", accent = "cyan", chrome = "default", density = "default", children, hints, headerProps }) {
  return (
    <div className="tui-root" data-theme={theme} data-accent={accent} data-chrome={chrome} data-density={density}>
      <div className="tui-app">
        <TopBar {...(headerProps || {})} />
        {children}
        <BottomBar hints={hints || undefined} />
      </div>
    </div>
  );
}

// ============================================================
// 1 · WELCOME / SPLASH
// ============================================================
function ScreenWelcome(props) {
  const hints = [
    { k: "↵",  d: "start" },
    { k: "/",  d: "cmds" },
    { k: "⌃R", d: "resume" },
    { k: "⌃M", d: "model" },
    { k: "?",  d: "help" },
  ];
  return (
    <Shell {...props} hints={hints}>
      <Panel title="zap · v0.4.2" icon="⚡" status={<><span className="dim">14 sessions · last 3m ago</span></>} focus>
        <div style={{display:"grid", gridTemplateColumns:"1fr 1fr", gap:24, padding:"8px 4px 6px", height:"100%"}}>
          {/* left: brand */}
          <div style={{display:"flex", flexDirection:"column", justifyContent:"space-between"}}>
            <div>
              <div style={{fontSize:22, color:"var(--fg-bright)", lineHeight:"28px"}}>
                <span className="acc" style={{fontWeight:700}}>⚡ zap</span>
              </div>
              <div className="dim" style={{marginTop:4, fontSize:14}}>terminal copilot for codebases that breathe.</div>

              <div style={{marginTop:18, display:"grid", gridTemplateColumns:"auto 1fr", columnGap:14, rowGap:4, fontSize:13.5}}>
                <span className="dim">cwd</span>      <span>~/work/atlas-api</span>
                <span className="dim">branch</span>   <span>main <span className="mute">· clean · ahead 2</span></span>
                <span className="dim">model</span>    <span className="acc">zap-pro · 200k ctx</span>
                <span className="dim">mcp</span>      <span><span className="suc">●</span> postgres <span className="suc">●</span> github <span className="warn">◐</span> linear <span className="mute">○</span> playwright</span>
                <span className="dim">approval</span> <span>review · destructive ops only</span>
              </div>
            </div>

            <div className="col-flex">
              <div className="dim" style={{fontSize:12.5, letterSpacing:".08em"}}>RECENT SESSIONS</div>
              <div className="list" style={{fontSize:13.5}}>
                <div className="row sel" style={{gridTemplateColumns:"auto 1fr auto"}}>
                  <span>1.</span><span>auth refactor · RS256 + audit</span><span className="dim">3m ago</span>
                </div>
                <div className="row" style={{gridTemplateColumns:"auto 1fr auto"}}>
                  <span className="dim">2.</span><span>migrate users.id to ulid</span><span className="dim">yesterday</span>
                </div>
                <div className="row" style={{gridTemplateColumns:"auto 1fr auto"}}>
                  <span className="dim">3.</span><span>flaky billing test triage</span><span className="dim">2d ago</span>
                </div>
                <div className="row" style={{gridTemplateColumns:"auto 1fr auto"}}>
                  <span className="dim">4.</span><span>readme polish</span><span className="dim">last week</span>
                </div>
              </div>
            </div>
          </div>

          {/* right: getting started */}
          <div className="col-flex">
            <div className="dim" style={{fontSize:12.5, letterSpacing:".08em"}}>GET STARTED</div>
            <Panel title="ask anything" icon="▸" chrome="rounded">
              <div style={{padding:"4px 0", fontSize:13.5}}>
                <div><span className="acc">▸</span> <span className="bright">explain</span> <span className="dim">the request lifecycle in src/server.ts</span></div>
                <div><span className="acc">▸</span> <span className="bright">refactor</span> <span className="dim">the JWT middleware to support RS256</span></div>
                <div><span className="acc">▸</span> <span className="bright">why</span> <span className="dim">is the orgs/billing test flaky on CI?</span></div>
                <div><span className="acc">▸</span> <span className="bright">migrate</span> <span className="dim">users.id from int → ulid (incl. fk rewires)</span></div>
              </div>
            </Panel>

            <Panel title="zap learned about this repo" icon="✦" status={<span className="dim">indexed 7m ago</span>} chrome="rounded">
              <div style={{padding:"4px 0", fontSize:13.5}}>
                <div><span className="dim">·</span> express · drizzle · postgres 16</div>
                <div><span className="dim">·</span> 184 files · 31k loc · vitest + playwright</div>
                <div><span className="dim">·</span> conventional commits · prettier · eslint</div>
                <div><span className="dim">·</span> tasks: <span className="acc">pnpm dev</span>, <span className="acc">pnpm test</span>, <span className="acc">pnpm db:push</span></div>
              </div>
            </Panel>

            <div style={{marginTop:"auto"}}>
              <Panel title="loading" icon="◐">
                <div style={{padding:"2px 0", fontSize:13.5, display:"flex", gap:12, alignItems:"center"}}>
                  <Bar value={84} total={100} width={36} />
                  <span className="dim">indexing changed files · 84%</span>
                </div>
              </Panel>
            </div>
          </div>
        </div>
      </Panel>
    </Shell>
  );
}

// ============================================================
// 2 · MODEL SELECTOR (overlay over a dimmed chat)
// ============================================================
function ScreenModelPicker(props) {
  const items = [
    { provider: "zap",      name: "zap-pro",       ctx: "200k", price: "$3 / $15", tags: ["fast","tool-use","default"], current: true },
    { provider: "zap",      name: "zap-mini",      ctx: "128k", price: "$0.20 / $0.80", tags: ["cheap","quick"], },
    { provider: "anthropic",name: "claude-sonnet-4.5", ctx: "200k", price: "$3 / $15", tags: ["reasoning","long-ctx"], },
    { provider: "openai",   name: "gpt-5",          ctx: "256k", price: "$3 / $12", tags: ["coding"], },
    { provider: "openai",   name: "gpt-5-mini",     ctx: "128k", price: "$0.30 / $1.20", tags: ["cheap"], },
    { provider: "local",    name: "qwen3-coder-32b",ctx: "32k",  price: "free · ollama", tags: ["offline"], current: false, dim: true },
    { provider: "local",    name: "deepseek-coder-7b", ctx: "16k", price: "free · ollama", tags: ["tiny"], dim: true },
  ];
  return (
    <Shell {...props}>
      <Panel title="conversation" icon="⌬" status={<span className="dim">12 turns · 18.4k / 200k</span>}>
        <div style={{opacity:0.25, pointerEvents:"none"}}>
          <Msg who="you">refactor the JWT middleware to support RS256…</Msg>
          <Msg who="zap">I'll read the current middleware, scan call sites…</Msg>
        </div>
      </Panel>
      <Overlay width="80ch" onClose={()=>{}}>
        <Panel title="select model" icon="⌘" focus chrome="thick" style={{padding:0, background:"var(--bg)"}}>
          <div className="overlay-search">
            <span className="lbl">⌕</span>
            <span className="q">claude<Cursor/></span>
            <span className="grow"></span>
            <span className="dim" style={{fontSize:13}}>{items.length} models</span>
          </div>
          <div className="overlay-list" style={{maxHeight:"22em"}}>
            {items.map((m, i) => (
              <div key={m.name} className={"overlay-row " + (i === 2 ? "active" : "")} style={{gridTemplateColumns:"3ch 12ch 22ch 1fr auto"}}>
                <span className="key">{m.current ? "●" : " "}</span>
                <span className={m.dim ? "dim" : "name"}>{m.provider}</span>
                <span className={m.dim ? "dim" : "name"}>{m.name}</span>
                <span className="desc">
                  <span className="dim">ctx </span>{m.ctx}
                  <span className="dim">  ·  in/out </span>{m.price}
                  <span className="dim">  ·  </span>
                  {m.tags.map((t, j) => <span key={j} style={{color:"var(--fg-dim)", marginRight:8}}>#{t}</span>)}
                </span>
                <span className="hint">{m.current ? "current" : ""}</span>
              </div>
            ))}
          </div>
          <div className="overlay-foot">
            <span><span className="kbd">↑↓</span> nav</span>
            <span><span className="kbd">↵</span> select</span>
            <span><span className="kbd">⇧↵</span> select & retry last</span>
            <span><span className="kbd">esc</span> cancel</span>
            <span className="grow"></span>
            <span className="dim">type to filter providers</span>
          </div>
        </Panel>
      </Overlay>
    </Shell>
  );
}

// ============================================================
// 3 · SESSIONS LIST
// ============================================================
function ScreenSessions(props) {
  const rows = [
    { sel: true,  title: "auth refactor · RS256 + audit",       branch: "feature/auth-rs256", model: "zap-pro",  msgs: 18, when: "3m ago",  diffs: 2, status: "active" },
    { title: "migrate users.id to ulid",                         branch: "migrations/users-ulid", model: "claude-4.5", msgs: 42, when: "yesterday", diffs: 7, status: "paused" },
    { title: "flaky billing test triage",                        branch: "tests/billing-flaky", model: "zap-pro",  msgs: 27, when: "2d ago", diffs: 0, status: "done" },
    { title: "readme polish",                                    branch: "docs/readme", model: "zap-mini", msgs: 8,  when: "5d ago", diffs: 1, status: "done" },
    { title: "spike: ratatui prototype",                         branch: "spike/tui", model: "zap-pro",  msgs: 33, when: "1w ago", diffs: 5, status: "archived" },
    { title: "infra · upgrade pg 15 → 16",                       branch: "infra/pg16", model: "claude-4.5", msgs: 21, when: "2w ago", diffs: 3, status: "archived" },
    { title: "feature flag plumbing",                            branch: "feature/flags", model: "zap-pro",  msgs: 14, when: "1mo ago", diffs: 2, status: "archived" },
  ];
  const statusColor = (s) => s === "active" ? "suc" : s === "paused" ? "warn" : s === "done" ? "info" : "dim";
  return (
    <Shell {...props}>
      <Panel title="sessions · ~/work/atlas-api" icon="⌥" status={<><span className="dim">14 total · 3 active · 11 archived</span></>} focus>
        <div className="overlay-search" style={{margin:"-4px -12px 4px", borderBottom:"1px solid var(--border)"}}>
          <span className="lbl">⌕</span>
          <span className="q">auth<Cursor/></span>
          <span className="grow"></span>
          <span className="dim">filter</span>
          <span className="acc">·active</span>
          <span className="dim">·paused</span>
          <span className="dim">·done</span>
          <span className="dim">·archived</span>
        </div>
        <div className="list" style={{padding:"2px 0"}}>
          <div className="row dim" style={{gridTemplateColumns:"3ch 36ch 24ch 14ch 6ch 10ch 10ch", fontSize:12.5, letterSpacing:".06em"}}>
            <span></span>
            <span>SESSION</span>
            <span>BRANCH</span>
            <span>MODEL</span>
            <span>MSGS</span>
            <span>WHEN</span>
            <span>STATUS</span>
          </div>
          {rows.map((r, i) => (
            <div key={i} className={"row " + (r.sel ? "sel" : "")} style={{gridTemplateColumns:"3ch 36ch 24ch 14ch 6ch 10ch 10ch"}}>
              <span>{i + 1}.</span>
              <span className={r.sel ? "acc" : "bright"}>{r.title}</span>
              <span className="dim">{r.branch}</span>
              <span className="dim">{r.model}</span>
              <span className="dim">{r.msgs}</span>
              <span className="dim">{r.when}</span>
              <span className={statusColor(r.status)}>{r.status}</span>
            </div>
          ))}
        </div>
      </Panel>

      <Panel title="preview · auth refactor" icon="◫" chrome="rounded">
        <div style={{display:"grid", gridTemplateColumns:"1fr 36ch", gap:14, padding:"2px 0", fontSize:13.5}}>
          <div className="col-flex" style={{gap:2}}>
            <div className="dim">▸ refactor the JWT middleware to support RS256 alongside HS256…</div>
            <div className="dim">◇ read src/middleware/auth.ts · 142 lines</div>
            <div className="dim">◇ grep "requireAuth" → 11 hits</div>
            <div className="dim">◇ propose patch · +24 −6 in src/middleware/auth.ts</div>
            <div className="dim">▸ also add unit test for RS256 path</div>
          </div>
          <div className="col-flex" style={{gap:2, fontSize:13}}>
            <span><span className="dim">started </span>2024-05-25 14:02</span>
            <span><span className="dim">last act </span>3m ago</span>
            <span><span className="dim">tokens </span>18.4k in / 6.1k out</span>
            <span><span className="dim">cost </span>$0.12</span>
            <span><span className="dim">tools </span>read · grep · edit · run · git</span>
          </div>
        </div>
      </Panel>
    </Shell>
  );
}

// ============================================================
// 4 · MCP PLUGIN MANAGER
// ============================================================
function ScreenMCP(props) {
  const servers = [
    { name: "postgres",    url: "stdio · npx @mcp/postgres",      tools: 6, status: "ok",     ms: 12, since: "2h",   sel: false },
    { name: "github",      url: "http · api.github.com (oauth)",  tools: 14, status: "ok",    ms: 84, since: "1d",   sel: true },
    { name: "linear",      url: "http · mcp.linear.app",          tools: 9,  status: "warn",  ms: 312, since: "12m", sel: false, note: "rate limited" },
    { name: "playwright",  url: "stdio · npx @mcp/playwright",    tools: 11, status: "off",   ms: 0,   since: "—",    sel: false },
    { name: "filesystem",  url: "builtin · sandboxed cwd",        tools: 8,  status: "ok",    ms: 4,   since: "—",    sel: false, builtin: true },
    { name: "shell",       url: "builtin · sandbox · approval",   tools: 3,  status: "ok",    ms: 6,   since: "—",    sel: false, builtin: true },
    { name: "memory",      url: "local · ~/.zap/memory.db",       tools: 4,  status: "ok",    ms: 2,   since: "—",    sel: false, builtin: true },
  ];
  const dot = (s) => s === "ok" ? <span className="suc">●</span> : s === "warn" ? <span className="warn">◐</span> : <span className="mute">○</span>;
  return (
    <Shell {...props}>
      <Panel title="mcp servers" icon="⊕" status={<><span className="suc">●</span> <span className="dim">5 connected</span> <span className="warn">◐</span> <span className="dim">1 degraded</span> <span className="mute">○</span> <span className="dim">1 off</span></>}>
        <div className="tabs">
          <div className="tab active">servers</div>
          <div className="tab">tools</div>
          <div className="tab">resources</div>
          <div className="tab">prompts</div>
          <div className="tab">audit log</div>
        </div>
        <div className="list">
          <div className="row dim" style={{gridTemplateColumns:"3ch 16ch 32ch 6ch 8ch 8ch 1fr", fontSize:12.5}}>
            <span></span>
            <span>NAME</span>
            <span>TRANSPORT</span>
            <span>TOOLS</span>
            <span>p50</span>
            <span>UP</span>
            <span>STATUS</span>
          </div>
          {servers.map((s, i) => (
            <div key={s.name} className={"row " + (s.sel ? "sel" : "")} style={{gridTemplateColumns:"3ch 16ch 32ch 6ch 8ch 8ch 1fr"}}>
              <span>{dot(s.status)}</span>
              <span className={s.sel ? "acc" : "bright"}>{s.name}{s.builtin && <span className="mute"> ★</span>}</span>
              <span className="dim">{s.url}</span>
              <span className="dim">{s.tools}</span>
              <span className="dim">{s.ms}ms</span>
              <span className="dim">{s.since}</span>
              <span className={s.status === "ok" ? "suc" : s.status === "warn" ? "warn" : "mute"}>
                {s.status === "ok" ? "connected" : s.status === "warn" ? `degraded · ${s.note || ""}` : "disabled"}
              </span>
            </div>
          ))}
        </div>
      </Panel>

      <Panel title="github · 14 tools" icon="◇" chrome="rounded">
        <div style={{display:"grid", gridTemplateColumns:"1fr 1fr", padding:"2px 0", fontSize:13.5, columnGap:24, rowGap:2}}>
          <div><span className="acc">▸</span> create_issue       <span className="dim">— file an issue on a repo</span></div>
          <div><span className="acc">▸</span> list_prs           <span className="dim">— open / closed / merged</span></div>
          <div><span className="acc">▸</span> review_pr          <span className="dim">— approve, request changes</span></div>
          <div><span className="acc">▸</span> search_code        <span className="dim">— ranked code search</span></div>
          <div><span className="acc">▸</span> get_workflow_run   <span className="dim">— logs + status</span></div>
          <div><span className="acc">▸</span> + 9 more …</div>
        </div>
        <div className="overlay-foot" style={{padding:"4px 0 0", borderTop:"1px dashed var(--border)", marginTop:4}}>
          <span><span className="kbd">↵</span> open</span>
          <span><span className="kbd">d</span> disable</span>
          <span><span className="kbd">a</span> approval mode</span>
          <span><span className="kbd">r</span> reconnect</span>
          <span className="grow"></span>
          <span className="dim">edit at ~/.zap/mcp.json</span>
        </div>
      </Panel>
    </Shell>
  );
}

// ============================================================
// 5 · SETTINGS / CONFIG
// ============================================================
function ScreenSettings(props) {
  return (
    <Shell {...props}>
      <Panel title="settings" icon="⚙" status={<span className="dim">~/.zap/config.toml</span>} focus>
        <div className="tabs">
          <div className="tab">profile</div>
          <div className="tab active">model & providers</div>
          <div className="tab">tools & approval</div>
          <div className="tab">appearance</div>
          <div className="tab">keymap</div>
          <div className="tab">advanced</div>
        </div>

        <div style={{display:"grid", gridTemplateColumns:"34ch 1fr", gap:16, padding:"2px 0"}}>
          <div className="list" style={{fontSize:13.5}}>
            <div className="row dim" style={{gridTemplateColumns:"1fr"}}>PROVIDERS</div>
            <div className="row sel" style={{gridTemplateColumns:"3ch 1fr auto"}}><span></span><span>anthropic</span><span className="suc">●</span></div>
            <div className="row" style={{gridTemplateColumns:"3ch 1fr auto"}}><span className="dim">·</span><span>openai</span><span className="suc">●</span></div>
            <div className="row" style={{gridTemplateColumns:"3ch 1fr auto"}}><span className="dim">·</span><span>zap cloud</span><span className="suc">●</span></div>
            <div className="row" style={{gridTemplateColumns:"3ch 1fr auto"}}><span className="dim">·</span><span>ollama (local)</span><span className="warn">◐</span></div>
            <div className="row" style={{gridTemplateColumns:"3ch 1fr auto"}}><span className="dim">·</span><span>azure</span><span className="mute">○</span></div>
            <div className="row" style={{gridTemplateColumns:"3ch 1fr auto"}}><span className="dim">·</span><span>+ add provider</span><span className="dim">⏎</span></div>
          </div>

          <div className="col-flex" style={{fontSize:13.5, gap:8}}>
            <Panel title="anthropic" icon="◇" chrome="rounded">
              <div style={{display:"grid", gridTemplateColumns:"18ch 1fr", rowGap:2, padding:"2px 0"}}>
                <span className="dim">api key</span>     <span><span className="bright">sk-ant-***</span><span className="dim"> · 4 keys · rotated 12d ago</span></span>
                <span className="dim">base url</span>    <span>https://api.anthropic.com</span>
                <span className="dim">default model</span><span className="acc">claude-sonnet-4.5</span>
                <span className="dim">fallback</span>    <span>zap-pro <span className="dim">on 429 / 5xx</span></span>
                <span className="dim">max retries</span> <span>3 · exp backoff</span>
                <span className="dim">timeout</span>     <span>120s</span>
                <span className="dim">stream</span>      <span>on</span>
                <span className="dim">cache prompts</span><span>on · prefix caching <span className="dim">(saves ~38%)</span></span>
                <span className="dim">spend cap</span>   <span>$50 / day <span className="dim">· $12.40 used today</span></span>
              </div>
            </Panel>
            <Panel title="defaults" icon="◇" chrome="rounded">
              <div style={{display:"grid", gridTemplateColumns:"18ch 1fr", rowGap:2, padding:"2px 0"}}>
                <span className="dim">routing</span>      <span>capability-based <span className="dim">· cheap → reasoning on demand</span></span>
                <span className="dim">temperature</span>  <span>0.2</span>
                <span className="dim">budget</span>       <span><Bar value={62} total={100} width={20}/> <span className="dim">$31 / $50 today</span></span>
              </div>
            </Panel>
          </div>
        </div>
      </Panel>

      <div className="row-flex" style={{padding:"0 12px", fontSize:13, color:"var(--fg-dim)"}}>
        <span><span className="kbd" style={{background:"var(--bg-elev)", border:"1px solid var(--border)", padding:"0 6px"}}>⇥</span> next field</span>
        <span><span className="kbd" style={{background:"var(--bg-elev)", border:"1px solid var(--border)", padding:"0 6px"}}>↵</span> edit</span>
        <span><span className="kbd" style={{background:"var(--bg-elev)", border:"1px solid var(--border)", padding:"0 6px"}}>⌃S</span> save</span>
        <span><span className="kbd" style={{background:"var(--bg-elev)", border:"1px solid var(--border)", padding:"0 6px"}}>⌃Z</span> revert</span>
        <span className="grow"></span>
        <span className="warn">● unsaved changes (2)</span>
      </div>
    </Shell>
  );
}

// ============================================================
// 6 · APPROVAL DIALOG (danger op)
// ============================================================
function ScreenApproval(props) {
  return (
    <Shell {...props}>
      <Panel title="conversation" icon="⌬" status={<span className="dim">awaiting approval</span>}>
        <div style={{opacity:0.3, pointerEvents:"none"}}>
          <Msg who="you">drop the unused `legacy_sessions` table</Msg>
          <Msg who="zap">checking dependencies first…</Msg>
          <Msg who="zap">no foreign keys reference it. proposing destructive sql…</Msg>
        </div>
      </Panel>
      <Overlay width="80ch" onClose={()=>{}}>
        <Panel title="approval required" icon="⚠" focus chrome="thick" style={{padding:0, background:"var(--bg)", borderColor:"var(--warn)", boxShadow:"inset 0 0 0 1px var(--warn)"}}>
          <div style={{padding:"6px 14px 4px", borderBottom:"1px solid var(--border)"}}>
            <div className="warn pulse" style={{fontSize:14, fontWeight:500}}>⚠ destructive shell command · sandbox: cwd · cannot be undone</div>
            <div className="dim" style={{fontSize:13.5, marginTop:2}}>requested by <span className="acc">zap-pro</span> · tool <span className="acc">shell.exec</span> · 3rd approval this session</div>
          </div>
          <div style={{padding:"6px 14px", fontSize:13.5}}>
            <div className="dim">command</div>
            <div className="tool" style={{marginTop:4}}>
              <div className="tool__body" style={{padding:"6px 12px", color:"var(--fg-bright)"}}>
                <div><span className="acc">$</span> psql atlas_dev -c <span className="err">"DROP TABLE legacy_sessions CASCADE;"</span></div>
              </div>
            </div>

            <div className="dim" style={{marginTop:8}}>side effects (analyzed)</div>
            <div style={{marginTop:2}}>
              <div><span className="err">·</span> drops table <span className="bright">legacy_sessions</span> <span className="dim">(0 fk inbound, 4,210 rows)</span></div>
              <div><span className="warn">·</span> revokes table grants from role <span className="bright">app_rw</span></div>
              <div><span className="suc">·</span> no migrations reference this table</div>
              <div><span className="suc">·</span> backup snapshot exists · 2h ago · 14 MB</div>
            </div>

            <div className="dim" style={{marginTop:8}}>scope this approval applies to</div>
            <div className="list" style={{marginTop:2}}>
              <div className="row sel" style={{gridTemplateColumns:"3ch 1fr"}}><span></span><span>just this once</span></div>
              <div className="row" style={{gridTemplateColumns:"3ch 1fr"}}><span className="dim">·</span><span>this session</span></div>
              <div className="row" style={{gridTemplateColumns:"3ch 1fr"}}><span className="dim">·</span><span>session + cwd <span className="dim">~/work/atlas-api</span></span></div>
              <div className="row" style={{gridTemplateColumns:"3ch 1fr"}}><span className="dim">·</span><span>always <span className="warn">(not recommended for destructive)</span></span></div>
            </div>
          </div>
          <div className="overlay-foot">
            <span><span className="kbd" style={{background:"var(--success)", color:"#04140e", borderColor:"var(--success)"}}>y</span> approve</span>
            <span><span className="kbd" style={{background:"var(--error)", color:"#1a0708", borderColor:"var(--error)"}}>n</span> deny</span>
            <span><span className="kbd">e</span> edit command</span>
            <span><span className="kbd">d</span> dry run</span>
            <span className="grow"></span>
            <span className="dim">defaults to <span className="err">deny</span> in 28s</span>
          </div>
        </Panel>
      </Overlay>
    </Shell>
  );
}

// ============================================================
// 7 · FILE TREE & CONTEXT MANAGER
// ============================================================
function ScreenFiles(props) {
  const tree = [
    { d: 0, n: "atlas-api", kind: "dir", open: true },
    { d: 1, n: ".github",   kind: "dir" },
    { d: 1, n: "src",       kind: "dir", open: true },
    { d: 2, n: "middleware",kind: "dir", open: true },
    { d: 3, n: "auth.ts",   kind: "file", lang: "ts", ctx: "pinned", sel: true },
    { d: 3, n: "logging.ts",kind: "file", lang: "ts" },
    { d: 3, n: "rate.ts",   kind: "file", lang: "ts", ctx: "auto" },
    { d: 2, n: "routes",    kind: "dir", open: true },
    { d: 3, n: "users.ts",  kind: "file", lang: "ts", ctx: "auto" },
    { d: 3, n: "orgs.ts",   kind: "file", lang: "ts", ctx: "auto" },
    { d: 3, n: "billing.ts",kind: "file", lang: "ts" },
    { d: 2, n: "lib",       kind: "dir" },
    { d: 2, n: "server.ts", kind: "file", lang: "ts" },
    { d: 1, n: "test",      kind: "dir" },
    { d: 1, n: "drizzle",   kind: "dir" },
    { d: 1, n: ".env",      kind: "file", lang: "env", ctx: "blocked" },
    { d: 1, n: "package.json", kind: "file", lang: "json", ctx: "auto" },
    { d: 1, n: "README.md", kind: "file", lang: "md" },
  ];
  const indent = (d) => "  ".repeat(d);
  return (
    <Shell {...props}>
      <div style={{display:"grid", gridTemplateColumns:"40ch 1fr", gap:12, height:"100%"}}>
        <Panel title="files" icon="◫" status={<span className="dim">⇥ focus · ⌘P fuzzy</span>} focus>
          <div className="overlay-search" style={{margin:"-4px -12px 4px", borderBottom:"1px solid var(--border)"}}>
            <span className="lbl">⌕</span>
            <span className="q">auth<Cursor/></span>
            <span className="grow"></span>
            <span className="dim">3 matches</span>
          </div>
          <div style={{fontSize:13.5, lineHeight:"20px"}}>
            {tree.map((n, i) => (
              <div key={i} style={{
                display:"grid",
                gridTemplateColumns:"1fr auto",
                padding:"0 6px",
                background: n.sel ? "var(--accent-tint)" : "transparent",
                color: n.sel ? "var(--accent)" : "var(--fg)",
              }}>
                <span>
                  <span style={{color:"var(--fg-mute)"}}>{indent(n.d)}</span>
                  {n.kind === "dir" ? <span className="dim">{n.open ? "▾ " : "▸ "}</span> : <span className="dim">  </span>}
                  {n.kind === "dir"
                    ? <span className="bright">{n.n}/</span>
                    : <span>{n.n}</span>}
                </span>
                <span style={{fontSize:12.5}}>
                  {n.ctx === "pinned"  && <span className="acc">◆ pinned</span>}
                  {n.ctx === "auto"    && <span className="dim">◇ auto</span>}
                  {n.ctx === "blocked" && <span className="err">✕ blocked</span>}
                </span>
              </div>
            ))}
          </div>
        </Panel>

        <div className="col-flex">
          <Panel title="src/middleware/auth.ts" icon="◇" status={<span className="dim">142 lines · ts · pinned · 4.1 KB</span>}>
            <div style={{fontSize:13.5, lineHeight:"20px", padding:"2px 0"}}>
              {[
                ["1","import jwt from \"jsonwebtoken\";"],
                ["2","import { Request, Response, NextFunction } from \"express\";"],
                ["3",""],
                ["4","const SECRET = process.env.JWT_SECRET!;"],
                ["5",""],
                ["6","export function requireAuth(req, res, next) {"],
                ["7","  const tok = req.header(\"authorization\")?.split(\" \")[1];"],
                ["8","  jwt.verify(tok, SECRET, (err, claims) => {"],
                ["9","    if (err) {"],
                ["10","      return res.status(401).json({ error: \"invalid_token\" });"],
                ["11","    }"],
                ["12","    req.user = claims;"],
                ["13","    next();"],
                ["14","  });"],
                ["15","}"],
              ].map(([ln, code], i) => (
                <div key={i} style={{display:"grid", gridTemplateColumns:"4ch 1fr", color:"var(--fg)"}}>
                  <span className="mute" style={{textAlign:"right", paddingRight:6}}>{ln}</span>
                  <span>{code}</span>
                </div>
              ))}
            </div>
          </Panel>

          <Panel title="context budget" icon="▦" chrome="rounded">
            <div style={{padding:"2px 0", fontSize:13.5}}>
              <div className="row-flex"><Bar value={64} total={100} width={36}/><span className="dim">128k / 200k used · 36% headroom</span></div>
              <div className="row-flex" style={{marginTop:4, gap:18, flexWrap:"wrap"}}>
                <span><span className="acc">◆</span> pinned · 3 files · 18k</span>
                <span><span className="dim">◇ auto</span> · 11 files · 84k</span>
                <span><span className="dim">▸ chat</span> · 18k</span>
                <span><span className="dim">▸ tools</span> · 8k</span>
              </div>
            </div>
          </Panel>
        </div>
      </div>
    </Shell>
  );
}

// ============================================================
// 8 · DIFF FOCUS (full-screen review)
// ============================================================
function ScreenDiffReview(props) {
  const rows = [
    { kind: "hunk", code: "@@ -1,6 +1,9 @@ src/middleware/auth.ts" },
    { kind: "ctx",  ln1:"1",  ln2:"1",  code: "import jwt from \"jsonwebtoken\";" },
    { kind: "del",  ln1:"2",  ln2:"",   code: "import { Request, Response, NextFunction } from \"express\";" },
    { kind: "add",  ln1:"",   ln2:"2",  code: WD(["import ", ["add","type "], "{ Request, Response, NextFunction } from \"express\";"]) },
    { kind: "add",  ln1:"",   ln2:"3",  code: "import { audit } from \"@/lib/audit\";" },
    { kind: "ctx",  ln1:"3",  ln2:"4",  code: "" },
    { kind: "del",  ln1:"4",  ln2:"",   code: WD(["const ", ["del","SECRET"], " = process.env.JWT_SECRET!;"]) },
    { kind: "add",  ln1:"",   ln2:"5",  code: WD(["const ", ["add","HS_SECRET"], " = process.env.JWT_SECRET!;"]) },
    { kind: "add",  ln1:"",   ln2:"6",  code: "const RS_PUBLIC = process.env.JWT_PUBLIC_KEY;" },
    { kind: "hunk", code: "@@ -6,10 +9,14 @@ src/middleware/auth.ts" },
    { kind: "ctx",  ln1:"6",  ln2:"9",  code: "export function requireAuth(req, res, next) {" },
    { kind: "del",  ln1:"7",  ln2:"",   code: WD(["  const tok = req.header(\"authorization\")?.split(\" \")[1];"]) },
    { kind: "add",  ln1:"",   ln2:"10", code: WD(["  const tok = ", ["add","extractBearer(req)"], ";"]) },
    { kind: "del",  ln1:"8",  ln2:"",   code: WD(["  jwt.verify(tok, ", ["del","SECRET"], ", (err, claims) => {"]) },
    { kind: "add",  ln1:"",   ln2:"11", code: WD(["  ", ["add","verifyToken"], "(tok, (err, claims) => {"]) },
    { kind: "ctx",  ln1:"9",  ln2:"12", code: "    if (err) {" },
    { kind: "add",  ln1:"",   ln2:"13", code: WD(["      ", ["add","audit(\"auth.verify\", { ok: false, reason: err.code });"]]) },
    { kind: "ctx",  ln1:"10", ln2:"14", code: "      return res.status(401).json({ error: \"invalid_token\" });" },
    { kind: "ctx",  ln1:"11", ln2:"15", code: "    }" },
    { kind: "add",  ln1:"",   ln2:"16", code: WD(["    ", ["add","audit(\"auth.verify\", { ok: true, sub: claims.sub });"]]) },
    { kind: "ctx",  ln1:"12", ln2:"17", code: "    req.user = claims;" },
    { kind: "ctx",  ln1:"13", ln2:"18", code: "    next();" },
    { kind: "ctx",  ln1:"14", ln2:"19", code: "  });" },
    { kind: "ctx",  ln1:"15", ln2:"20", code: "}" },
  ];
  const hints = [
    { k: "y",  d: "apply" },
    { k: "n",  d: "reject" },
    { k: "e",  d: "edit" },
    { k: "↵",  d: "next hunk" },
    { k: "⇧↵", d: "prev hunk" },
    { k: "a",  d: "accept all" },
    { k: "⌃c", d: "cancel" },
  ];
  return (
    <Shell {...props} hints={hints}>
      <div style={{display:"grid", gridTemplateColumns:"32ch 1fr", gap:12, height:"100%"}}>
        <Panel title="changeset · 3 files" icon="◫" status={<span className="dim">+ 38 − 14</span>}>
          <div className="list">
            <div className="row sel" style={{gridTemplateColumns:"3ch 1fr auto auto"}}>
              <span></span>
              <span className="acc">middleware/auth.ts</span>
              <span className="suc">+24</span>
              <span className="err">−6</span>
            </div>
            <div className="row" style={{gridTemplateColumns:"3ch 1fr auto auto"}}>
              <span className="dim">·</span>
              <span>lib/audit.ts <span className="dim">(new)</span></span>
              <span className="suc">+12</span>
              <span className="err">−0</span>
            </div>
            <div className="row" style={{gridTemplateColumns:"3ch 1fr auto auto"}}>
              <span className="dim">·</span>
              <span>test/auth.test.ts</span>
              <span className="suc">+2</span>
              <span className="err">−8</span>
            </div>
          </div>

          <div style={{marginTop:8, padding:"4px 6px", borderTop:"1px dashed var(--border)", fontSize:13}}>
            <div className="dim">summary</div>
            <div style={{marginTop:2}}>
              · adds RS256 verification via <span className="acc">verifyToken()</span>
            </div>
            <div>· emits <span className="acc">audit:auth.verify</span> with subject + reason</div>
            <div>· keeps <span className="acc">requireAuth</span> signature</div>
          </div>
        </Panel>

        <Panel title="src/middleware/auth.ts · review" icon="◇" status={<><span className="suc">+24</span> <span className="err">−6</span> <span className="dim">· 2 hunks · 1/2</span></>} focus>
          <div className="diff" style={{border:"none", margin:0, background:"transparent"}}>
            <div className="diff__rows">
              {rows.map((r, i) => (
                <div key={i} className={`diff__row diff__row--${r.kind}`}>
                  <span className="ln">{r.ln1 ?? ""}</span>
                  <span className="ln">{r.ln2 ?? ""}</span>
                  <span className="sign">{r.kind === "add" ? "+" : r.kind === "del" ? "-" : r.kind === "hunk" ? "@" : " "}</span>
                  <span className={"code " + (r.kind === "hunk" ? "hunk" : "")}>{r.code}</span>
                </div>
              ))}
            </div>
          </div>
        </Panel>
      </div>
    </Shell>
  );
}

// ============================================================
// 9 · TOOL STREAM (long-running run command)
// ============================================================
function ScreenToolStream(props) {
  return (
    <Shell {...props}>
      <Panel title="conversation" icon="⌬" status={<><Spinner/> <span className="dim">running pnpm test</span></>} focus className="panel--fill">
        <Msg who="you">run the auth tests and tell me which ones break</Msg>
        <Msg who="zap">
          on it. running <code>pnpm test test/auth</code>.
          <ToolBlock
            kind="shell.exec"
            args="pnpm test test/auth"
            meta="14.2s · running"
            status="warn"
          >
            <div><span className="suc">▸</span> RUN <span className="bright">v1.6.0</span> /work/atlas-api</div>
            <div></div>
            <div><span className="suc">✓</span> test/auth/hs256.spec.ts <span className="dim">(4)</span></div>
            <div><span className="suc">✓</span> test/auth/extract.spec.ts <span className="dim">(3)</span></div>
            <div><span className="err">✗</span> test/auth/rs256.spec.ts <span className="dim">(5 | 2 failed)</span></div>
            <div className="dim">    <span className="err">×</span> verifies a token signed with RS256</div>
            <div className="dim">    <span className="err">×</span> rejects HS256 token when only RS256 configured</div>
            <div><span className="dim">↻</span> test/auth/audit.spec.ts <span className="dim">(2 | running)</span></div>
            <div></div>
            <div><span className="dim">stderr ─</span></div>
            <div className="err">  AssertionError: expected 'invalid_token' to deeply equal 'algo_not_allowed'</div>
            <div className="err">    at test/auth/rs256.spec.ts:42:18</div>
          </ToolBlock>
        </Msg>
        <Msg who="zap" label="zap">
          <span className="dim"><Spinner/> two failures so far · waiting for audit suite…</span>
        </Msg>
      </Panel>
      <Panel title="input" icon="●">
        <div style={{padding:"2px 0", fontSize:14}}>
          <span className="led-busy">◐</span>
          <span className="prompt" style={{marginLeft:8}}>▸</span>
          <span className="dim"> zap is running · Esc to interrupt</span>
        </div>
      </Panel>
    </Shell>
  );
}

// ----------------------------------------------------------
// export
Object.assign(window, {
  ScreenWelcome, ScreenModelPicker, ScreenSessions, ScreenMCP,
  ScreenSettings, ScreenApproval, ScreenFiles, ScreenDiffReview, ScreenToolStream, Shell,
});
