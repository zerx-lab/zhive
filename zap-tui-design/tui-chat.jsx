/* global React, Panel, TopBar, BottomBar, Msg, ToolBlock, CodeLine, Diff, WD, Bar, Spinner, Cursor, Overlay, DEFAULT_HINTS */
/* zap-tui · interactive main chat (typing, slash palette, diff accept/reject) */

const { useEffect, useRef, useState, useMemo, useCallback } = React;

// ---- seeded conversation -----------------------------------

const SEED = [
  { id: "u1", type: "user", body: (
    <>refactor the JWT middleware to support <code>RS256</code> alongside HS256 and emit structured audit events.</>
  )},
  { id: "a1", type: "assistant", body: (
    <>I'll read the current middleware, scan call sites, then propose a patch.</>
  )},
  { id: "t1", type: "tool", tool: {
    kind: "read_file",
    args: "src/middleware/auth.ts",
    meta: "142 lines · 4.1 KB · ok",
    status: "ok",
    body: [
      ["1",  "import jwt from \"jsonwebtoken\";"],
      ["2",  "import { Request, Response, NextFunction } from \"express\";"],
      ["3",  ""],
      ["4",  "const SECRET = process.env.JWT_SECRET!;"],
      ["5",  ""],
      ["6",  "export function requireAuth(req, res, next) {"],
      ["7",  "  const tok = req.header(\"authorization\")?.split(\" \")[1];"],
      ["…",  ""],
    ],
  }},
  { id: "t2", type: "tool", tool: {
    kind: "grep",
    args: "\"requireAuth\" → 11 hits across 7 files",
    meta: "1.2s",
    status: "ok",
    body: [
      ["", "routes/users.ts:14    router.get(\"/me\", requireAuth, handler)"],
      ["", "routes/orgs.ts:22     router.post(\"/orgs\", requireAuth, …)"],
      ["", "routes/billing.ts:9   …requireAuth, billingScope"],
      ["", "+ 8 more"],
    ],
  }},
  { id: "a2", type: "assistant", body: (
    <>
      Proposing a patch: introduce <code>verifyToken()</code> that selects HS256 vs RS256 based on the token header, keeps the existing <code>requireAuth</code> signature, and emits <code>audit:auth.verify</code> on success/fail.
    </>
  )},
  { id: "d1", type: "diff", diff: {
    path: "src/middleware/auth.ts",
    addCount: 24,
    delCount: 6,
    rows: [
      { kind: "hunk", ln1: "", ln2: "", code: "@@ -1,6 +1,9 @@" },
      { kind: "ctx",  ln1: "1", ln2: "1", code: "import jwt from \"jsonwebtoken\";" },
      { kind: "del",  ln1: "2", ln2: "",  code: "import { Request, Response, NextFunction } from \"express\";" },
      { kind: "add",  ln1: "",  ln2: "2", code: WD(["import type { Request, Response, NextFunction } from \"express\";"]) },
      { kind: "add",  ln1: "",  ln2: "3", code: WD(["import { audit } from \"@/lib/audit\";"]) },
      { kind: "ctx",  ln1: "3", ln2: "4", code: "" },
      { kind: "del",  ln1: "4", ln2: "",  code: WD(["const SECRET = process.env.JWT_SECRET!;"]) },
      { kind: "add",  ln1: "",  ln2: "5", code: WD(["const HS_SECRET = process.env.JWT_SECRET!;"]) },
      { kind: "add",  ln1: "",  ln2: "6", code: WD(["const RS_PUBLIC = process.env.JWT_PUBLIC_KEY;"]) },
      { kind: "hunk", ln1: "", ln2: "", code: "@@ -6,4 +9,18 @@" },
      { kind: "ctx",  ln1: "6", ln2: "9", code: "export function requireAuth(req, res, next) {" },
      { kind: "del",  ln1: "7", ln2: "",  code: WD(["  const tok = req.header(\"authorization\")?.split(\" \")[1];"]) },
      { kind: "add",  ln1: "",  ln2: "10", code: WD(["  const tok = ", ["add", "extractBearer(req)"], ";"]) },
      { kind: "del",  ln1: "8", ln2: "",  code: WD(["  jwt.verify(tok, SECRET, (err, claims) => {"]) },
      { kind: "add",  ln1: "",  ln2: "11", code: WD(["  ", ["add", "verifyToken"], "(tok, (err, claims) => {"]) },
      { kind: "ctx",  ln1: "9", ln2: "12", code: "    if (err) {" },
      { kind: "add",  ln1: "",  ln2: "13", code: WD(["      audit(\"auth.verify\", { ok: false, reason: err.code });"]) },
      { kind: "ctx",  ln1: "10", ln2: "14", code: "      return res.status(401).json({ error: \"invalid_token\" });" },
      { kind: "ctx",  ln1: "11", ln2: "15", code: "    }" },
      { kind: "add",  ln1: "",  ln2: "16", code: WD(["    audit(\"auth.verify\", { ok: true, sub: claims.sub });"]) },
      { kind: "ctx",  ln1: "12", ln2: "17", code: "    req.user = claims;" },
      { kind: "ctx",  ln1: "13", ln2: "18", code: "    next();" },
      { kind: "ctx",  ln1: "14", ln2: "19", code: "  });" },
      { kind: "ctx",  ln1: "15", ln2: "20", code: "}" },
    ],
  }},
];

const SLASH_CMDS = [
  { k: "/", name: "help",       desc: "show all commands & shortcuts" },
  { k: "/", name: "model",      desc: "switch model — zap-pro · zap-mini · claude · gpt", hint: "⌃M" },
  { k: "/", name: "sessions",   desc: "list, resume, branch saved sessions", hint: "⌃R" },
  { k: "/", name: "new",        desc: "start a fresh session in this cwd" },
  { k: "/", name: "compact",    desc: "summarize and condense conversation" },
  { k: "/", name: "files",      desc: "open the context file tree", hint: "⇥" },
  { k: "/", name: "mcp",        desc: "manage Model Context Protocol servers" },
  { k: "/", name: "approve",    desc: "review pending tool approvals" },
  { k: "/", name: "settings",   desc: "open settings · theme, keys, providers" },
  { k: "/", name: "diff",       desc: "show pending file changes" },
  { k: "/", name: "undo",       desc: "revert the last applied edit" },
  { k: "/", name: "export",     desc: "save transcript as markdown" },
  { k: "/", name: "exit",       desc: "quit zap (state is saved)", hint: "⌃C" },
];

// ---- the component -----------------------------------------

function InteractiveChat({ theme = "dark", accent = "cyan", chrome = "default", density = "default", inputStyle = "rich", autoplay = true }) {
  const [items, setItems] = useState(() => SEED.slice(0, 4));
  const [busy, setBusy]   = useState(true);
  const [draft, setDraft] = useState("");
  const [palette, setPalette] = useState(null); // null | { q: string, idx: number }
  const [acceptState, setAcceptState] = useState({}); // diffId -> bool
  const taRef = useRef(null);
  const scrollRef = useRef(null);

  // play out the rest of the seeded convo
  useEffect(() => {
    if (!autoplay) { setItems(SEED); setBusy(false); return; }
    let cancel = false;
    let i = 4;
    const tick = () => {
      if (cancel) return;
      if (i >= SEED.length) { setBusy(false); return; }
      setItems(prev => [...prev, SEED[i]]);
      i++;
      setTimeout(tick, 850);
    };
    setTimeout(tick, 900);
    return () => { cancel = true; };
  }, [autoplay]);

  useEffect(() => {
    if (scrollRef.current) {
      scrollRef.current.scrollTop = scrollRef.current.scrollHeight;
    }
  }, [items.length]);

  // ---- key handling ----
  const onKey = (e) => {
    // palette open: ↑/↓/Enter/Esc
    if (palette) {
      const filtered = filterCmds(palette.q);
      if (e.key === "Escape")  { e.preventDefault(); setPalette(null); return; }
      if (e.key === "ArrowDown") { e.preventDefault(); setPalette(p => ({...p, idx: Math.min(filtered.length - 1, p.idx + 1)})); return; }
      if (e.key === "ArrowUp")   { e.preventDefault(); setPalette(p => ({...p, idx: Math.max(0, p.idx - 1)})); return; }
      if (e.key === "Enter")     { e.preventDefault(); runCommand(filtered[palette.idx]?.name); return; }
      if (e.key === "Backspace" && palette.q === "") { e.preventDefault(); setPalette(null); setDraft(""); return; }
      // typing into palette query — fall through to textarea, but track q
      return;
    }
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      sendDraft();
    }
  };

  const onInput = (e) => {
    const v = e.target.value;
    if (palette) {
      // strip leading "/"
      const q = v.startsWith("/") ? v.slice(1) : v;
      setDraft(v);
      setPalette(p => ({ q, idx: 0 }));
      return;
    }
    if (v === "/" && draft === "") {
      setDraft("/");
      setPalette({ q: "", idx: 0 });
      return;
    }
    setDraft(v);
  };

  const sendDraft = () => {
    const text = draft.trim();
    if (!text) return;
    const id = "u_" + Date.now();
    setItems(prev => [...prev, { id, type: "user", body: text }]);
    setDraft("");
    // fake assistant reply
    setBusy(true);
    setTimeout(() => {
      const rid = "a_" + Date.now();
      setItems(prev => [...prev, {
        id: rid, type: "assistant",
        body: <>Got it — I'll work on that next. (demo response)</>,
      }]);
      setBusy(false);
    }, 700);
  };

  const runCommand = (name) => {
    setPalette(null);
    setDraft("");
    const rid = "sys_" + Date.now();
    const cmd = SLASH_CMDS.find(c => c.name === name);
    setItems(prev => [...prev, {
      id: rid, type: "system",
      body: <><span className="acc">/{name}</span> <span className="dim">→ {cmd?.desc || "command executed"}</span></>,
    }]);
  };

  // ---- render ----
  const showPalette = palette !== null;
  const filtered = palette ? filterCmds(palette.q) : [];

  return (
    <div className="tui-root" data-theme={theme} data-accent={accent} data-chrome={chrome} data-density={density}>
      <div className="tui-app">
        <TopBar />

        <Panel
          title="conversation"
          icon="⌬"
          status={<>{busy ? <><Spinner/> <span className="dim">thinking</span></> : <><span className="ok">●</span> <span className="dim">ready</span></>} <span className="dim">·</span> <span className="dim">12 turns · 18.4k / 200k</span></>}
          focus
          className="panel--fill"
        >
          <div className="chat-scroll" ref={scrollRef}>
            {items.map(it => renderItem(it, acceptState, setAcceptState))}
            {busy && (
              <Msg who="zap" label="zap">
                <span className="dim"><Spinner /> reading code…</span>
              </Msg>
            )}
          </div>
        </Panel>

        {inputStyle === "rich" ? (
          <Panel
            title={busy ? "busy" : "ready"}
            icon={busy ? "◐" : "●"}
            status={<><span className="dim">⇧↵ newline</span> <span className="dim">·</span> <span className="dim">⌃J cite file</span></>}
          >
            <InputArea
              draft={draft}
              onInput={onInput}
              onKey={onKey}
              busy={busy}
              ref={taRef}
            />
          </Panel>
        ) : (
          <div className="row-flex" style={{padding:"2px 12px"}}>
            <span className="prompt">{busy ? "◐" : "▸"}</span>
            <SimpleInput draft={draft} onInput={onInput} onKey={onKey} ref={taRef} />
          </div>
        )}

        <BottomBar
          hints={DEFAULT_HINTS}
          right={<span className="dim">{busy ? "running" : "idle"} · {items.length} msgs · ⌨ focus chat</span>}
        />
      </div>

      {showPalette && (
        <Overlay onClose={() => { setPalette(null); setDraft(""); }} width="64ch">
          <SlashPalette query={palette.q} idx={palette.idx} items={filtered} onPick={runCommand} />
        </Overlay>
      )}
    </div>
  );
}

function filterCmds(q) {
  const s = q.toLowerCase();
  return SLASH_CMDS.filter(c => c.name.includes(s) || c.desc.toLowerCase().includes(s));
}

function renderItem(it, acceptState, setAcceptState) {
  if (it.type === "user")      return <Msg key={it.id} who="you">{it.body}</Msg>;
  if (it.type === "assistant") return <Msg key={it.id} who="zap">{it.body}</Msg>;
  if (it.type === "system")    return <Msg key={it.id} who="sys" label="·sys">{it.body}</Msg>;
  if (it.type === "tool") {
    const t = it.tool;
    return (
      <Msg key={it.id} who="zap" label="zap">
        <span className="dim">→ tool call</span>
        <ToolBlock kind={t.kind} args={t.args} meta={t.meta} status={t.status}>
          {t.body.map((row, i) => (
            <div key={i}>
              {row[0] && <span className="ln">{row[0]}</span>}
              <span className="fg">{row[1]}</span>
            </div>
          ))}
        </ToolBlock>
      </Msg>
    );
  }
  if (it.type === "diff") {
    const d = it.diff;
    const state = acceptState[it.id];
    return (
      <Msg key={it.id} who="zap" label="zap">
        <span className="dim">→ proposed edit · review below</span>
        <Diff
          {...d}
          accepted={state}
          onAccept={() => setAcceptState(s => ({...s, [it.id]: true}))}
          onReject={() => setAcceptState(s => ({...s, [it.id]: false}))}
        />
        <div style={{marginTop:6, fontSize:13.5}}>
          <span className="dim">
            press <span className="acc">y</span> to apply, <span className="acc">n</span> to discard, <span className="acc">e</span> to edit
          </span>
        </div>
        <DiffControls
          state={state}
          onY={() => setAcceptState(s => ({...s, [it.id]: true}))}
          onN={() => setAcceptState(s => ({...s, [it.id]: false}))}
        />
      </Msg>
    );
  }
  return null;
}

// trap y/n keys when diff is unresolved — implemented via a tiny invisible focusable
function DiffControls({ state, onY, onN }) {
  const ref = useRef(null);
  useEffect(() => {
    if (state !== undefined) return;
    const h = (e) => {
      if (document.activeElement && document.activeElement.tagName === "TEXTAREA") {
        // don't hijack while typing
        return;
      }
      if (e.key === "y") { onY(); }
      else if (e.key === "n") { onN(); }
    };
    window.addEventListener("keydown", h);
    return () => window.removeEventListener("keydown", h);
  }, [state, onY, onN]);
  return null;
}

// ----------------------------------------------------------
// Slash palette (used inside overlay)
// ----------------------------------------------------------
function SlashPalette({ query, idx, items, onPick }) {
  return (
    <Panel title="command palette" icon="⌘" focus chrome="thick" style={{padding:0, background:"var(--bg)"}}>
      <div className="overlay-search">
        <span className="lbl">/</span>
        <span className="q">{query}<Cursor/></span>
        <span className="grow"></span>
        <span className="dim" style={{fontSize:13}}>{items.length} match{items.length === 1 ? "" : "es"}</span>
      </div>
      <div className="overlay-list">
        {items.length === 0 && (
          <div className="overlay-row"><span></span><span className="dim">no matches</span><span></span><span></span></div>
        )}
        {items.map((c, i) => (
          <div key={c.name} className={"overlay-row " + (i === idx ? "active" : "")} onMouseDown={(e)=>{e.preventDefault(); onPick(c.name);}}>
            <span className="key">{c.k}</span>
            <span className="name">{c.name}</span>
            <span className="desc">{c.desc}</span>
            <span className="hint">{c.hint || ""}</span>
          </div>
        ))}
      </div>
      <div className="overlay-foot">
        <span><span className="kbd">↑↓</span> navigate</span>
        <span><span className="kbd">↵</span> run</span>
        <span><span className="kbd">esc</span> close</span>
        <span className="grow"></span>
        <span className="dim">type to filter · tab to autocomplete</span>
      </div>
    </Panel>
  );
}

// ----------------------------------------------------------
// Input areas
// ----------------------------------------------------------
const InputArea = React.forwardRef(function InputArea({ draft, onInput, onKey, busy }, ref) {
  return (
    <div className="input-wrap">
      <div className="input-pre">
        <span className={"input-led " + (busy ? "led-busy" : "led-ready")}>{busy ? "◐" : "●"}</span>
        <span className="prompt">▸</span>
        <textarea
          ref={ref}
          className="tinput"
          rows={2}
          value={draft}
          onChange={onInput}
          onKeyDown={onKey}
          placeholder={busy ? "zap is working… (Esc to interrupt)" : "ask zap or / for commands"}
          spellCheck={false}
        />
      </div>
      <div className="input-meta">
        <span>{draft.length} chars</span>
        <span>·</span>
        <span>cwd · atlas-api</span>
      </div>
    </div>
  );
});

const SimpleInput = React.forwardRef(function SimpleInput({ draft, onInput, onKey }, ref) {
  return (
    <textarea
      ref={ref}
      className="tinput"
      rows={1}
      value={draft}
      onChange={onInput}
      onKeyDown={onKey}
      placeholder="ask zap or / for commands"
      spellCheck={false}
    />
  );
});

Object.assign(window, { InteractiveChat, SlashPalette, SEED, SLASH_CMDS, filterCmds });
