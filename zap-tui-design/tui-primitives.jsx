/* global React */
/* zap-tui · shared primitives (Panel, Header, Footer, KeyHint, Diff, Bar) */

const { useEffect, useRef, useState, useMemo } = React;

// ----------------------------------------------------------
// Panel — ratatui-style bordered block w/ title overlay
// ----------------------------------------------------------
function Panel({ title, icon, status, focus, children, style, className = "", chrome }) {
  const mods = [
    "panel",
    focus ? "panel--focus" : "",
    chrome === "rounded" ? "panel--rounded" : "",
    chrome === "thick"   ? "panel--thick"   : "",
    className,
  ].filter(Boolean).join(" ");
  return (
    <div className={mods} style={style}>
      {title && (
        <div className="panel__title">
          {icon ? <span className="dot">{icon}</span> : <span className="dot">─</span>}
          <span>{title}</span>
        </div>
      )}
      {status && <div className="panel__status">{status}</div>}
      {children}
    </div>
  );
}

// ----------------------------------------------------------
// Top bar — ratatui top-line breadcrumbs
// ----------------------------------------------------------
function TopBar({ session = "auth-refactor", branch = "main", model = "zap-pro · 200k", cwd = "~/work/atlas-api" }) {
  return (
    <div className="tui-header">
      <span className="brand">⚡ zap</span>
      <span className="sep">·</span>
      <span className="crumb">{cwd}</span>
      <span className="sep">·</span>
      <span className="crumb dim">{branch}</span>
      <span className="sep">·</span>
      <span className="crumb dim">{session}</span>
      <span className="spacer"></span>
      <span className="pill acc">{model}</span>
    </div>
  );
}

// ----------------------------------------------------------
// Bottom bar — keyboard hints
// ----------------------------------------------------------
function BottomBar({ hints = DEFAULT_HINTS, right }) {
  return (
    <div className="tui-footer">
      {hints.map((h, i) => (
        <span className="item" key={i}>
          <span className="kbd">{h.k}</span>
          <span className="desc">{h.d}</span>
        </span>
      ))}
      <span className="spacer"></span>
      {right}
    </div>
  );
}
const DEFAULT_HINTS = [
  { k: "↵",   d: "send" },
  { k: "/",   d: "cmds" },
  { k: "⇥",   d: "files" },
  { k: "⌃R",  d: "history" },
  { k: "⌃M",  d: "model" },
  { k: "⌃C",  d: "quit" },
];

// ----------------------------------------------------------
// Message bubble (user / assistant / system / tool result)
// ----------------------------------------------------------
function Msg({ who = "you", label, children, extra }) {
  const cls = who === "you" ? "user" : who === "zap" ? "assistant" : "system";
  const lbl = label || (who === "you" ? "you" : who === "zap" ? "zap" : "·");
  return (
    <div className={`msg ${cls}`}>
      <div className="msg__label">{lbl}</div>
      <div className="msg__body">
        {children}
        {extra}
      </div>
    </div>
  );
}

// ----------------------------------------------------------
// Embedded tool-call block (read_file, run, grep, edit, …)
// ----------------------------------------------------------
function ToolBlock({ kind, args, status = "ok", meta, children }) {
  const cls = status === "ok" ? "ok" : status === "warn" ? "warn" : "";
  return (
    <div className="tool">
      <div className="tool__head">
        <span className="kind">▸ {kind}</span>
        <span className="arg">{args}</span>
        <span className={`meta ${cls}`}>{meta}</span>
      </div>
      {children && <div className="tool__body">{children}</div>}
    </div>
  );
}

// helper for line-number prefixed body
function CodeLine({ n, children, color = "fg" }) {
  return (
    <div>
      <span className="ln">{n}</span>
      <span className={color}>{children}</span>
    </div>
  );
}

// ----------------------------------------------------------
// Diff block w/ inline word-level highlight
//   rows: { kind: 'ctx'|'add'|'del'|'hunk', ln1, ln2, code: ReactNode | string }
// ----------------------------------------------------------
function Diff({ path, addCount, delCount, rows, accepted, onAccept, onReject, focused }) {
  return (
    <div className="diff">
      <div className="diff__head">
        <span className="path">{path}</span>
        <span className="stat"><span className="add">+{addCount}</span> <span className="del">-{delCount}</span></span>
        <span className="ctrl">
          {accepted === undefined ? (
            <>
              <span><b>y</b> accept</span>
              <span><b>n</b> reject</span>
              <span><b>e</b> edit</span>
              <span><b>?</b> explain</span>
            </>
          ) : accepted ? (
            <span className="suc">✓ applied</span>
          ) : (
            <span className="err">✕ rejected</span>
          )}
        </span>
      </div>
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
  );
}

// ----------------------------------------------------------
// Word-diff helper — splits a string into context + add/del runs
// usage: <WordRow>{["const ", ["del","oldFn"], ["add","newFn"], "(x)"]}</WordRow>
// ----------------------------------------------------------
function WD(parts) {
  return parts.map((p, i) => {
    if (Array.isArray(p)) {
      const [k, t] = p;
      return <span key={i} className={k === "add" ? "word-add" : "word-del"}>{t}</span>;
    }
    return <span key={i}>{p}</span>;
  });
}

// ----------------------------------------------------------
// Progress bar (block characters)
// ----------------------------------------------------------
function Bar({ value = 0, total = 1, width = 24 }) {
  const f = Math.max(0, Math.min(1, value / total));
  const filled = Math.round(f * width);
  return (
    <span className="bar">
      {"█".repeat(filled)}
      <span className="dim">{"░".repeat(width - filled)}</span>
    </span>
  );
}

// ----------------------------------------------------------
// Spinner — animated braille
// ----------------------------------------------------------
const SPIN = ["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"];
function Spinner({ color }) {
  const [i, setI] = useState(0);
  useEffect(() => {
    const t = setInterval(() => setI(v => (v + 1) % SPIN.length), 90);
    return () => clearInterval(t);
  }, []);
  return <span style={{ color: color || "var(--accent)" }}>{SPIN[i]}</span>;
}

// ----------------------------------------------------------
// Cursor (blinking block)
// ----------------------------------------------------------
function Cursor() { return <span className="cursor" />; }

// ----------------------------------------------------------
// Overlay frame for modals (palette, model picker, approval)
// ----------------------------------------------------------
function Overlay({ children, width = "60ch", onClose }) {
  return (
    <div className="overlay-back" onMouseDown={(e) => { if (e.target === e.currentTarget) onClose && onClose(); }}>
      <div className="overlay-panel" style={{ width }}>{children}</div>
    </div>
  );
}

// export to window for cross-script use
Object.assign(window, {
  Panel, TopBar, BottomBar, Msg, ToolBlock, CodeLine,
  Diff, WD, Bar, Spinner, Cursor, Overlay,
  DEFAULT_HINTS,
});
