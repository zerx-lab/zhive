/* global React, ReactDOM, DesignCanvas, DCSection, DCArtboard,
   InteractiveChat, ScreenWelcome, ScreenModelPicker, ScreenSessions, ScreenMCP,
   ScreenSettings, ScreenApproval, ScreenFiles, ScreenDiffReview, ScreenToolStream,
   TweaksPanel, useTweaks, TweakSection, TweakRadio, TweakColor, TweakToggle */

const { useState, useEffect } = React;

// Tweakable defaults — host can rewrite this
const TWEAK_DEFAULTS = /*EDITMODE-BEGIN*/{
  "theme": "dark",
  "accent": "cyan",
  "chrome": "default",
  "density": "default",
  "inputStyle": "rich"
}/*EDITMODE-END*/;

function App() {
  const [t, setTweak] = useTweaks(TWEAK_DEFAULTS);

  // shared props for every screen so Tweaks propagates
  const tp = {
    theme: t.theme,
    accent: t.accent,
    chrome: t.chrome,
    density: t.density,
  };

  // artboard dim — tight to terminal: 110ch * 9 + 24 = 1014, 32 rows * 22 + 16 = 720
  const TW = 1024;
  const TH = 728;

  return (
    <>
      <DesignCanvas minScale={0.2} maxScale={2}>

        <DCSection id="hero" title="Interactive · type, slash, accept diffs"
          subtitle="The live prototype — focus the input and type, press / for the command palette, press y/n on the proposed diff. Switch themes via the Tweaks panel.">
          <DCArtboard id="chat-live" label="Main chat · live" width={TW} height={TH}>
            <InteractiveChat {...tp} inputStyle={t.inputStyle} />
          </DCArtboard>
        </DCSection>

        <DCSection id="key-screens" title="Key screens"
          subtitle="Snapshot states across the product. Every screen uses the same chrome system so themes apply uniformly.">
          <DCArtboard id="welcome" label="01 · welcome" width={TW} height={TH}>
            <ScreenWelcome {...tp} />
          </DCArtboard>
          <DCArtboard id="sessions" label="02 · sessions" width={TW} height={TH}>
            <ScreenSessions {...tp} />
          </DCArtboard>
          <DCArtboard id="files" label="03 · file tree + context" width={TW} height={TH}>
            <ScreenFiles {...tp} />
          </DCArtboard>
          <DCArtboard id="tool-stream" label="04 · tool stream · running" width={TW} height={TH}>
            <ScreenToolStream {...tp} />
          </DCArtboard>
          <DCArtboard id="diff" label="05 · diff review · full" width={TW} height={TH}>
            <ScreenDiffReview {...tp} />
          </DCArtboard>
        </DCSection>

        <DCSection id="overlays" title="Overlays & modals"
          subtitle="Slash command palette, model picker, destructive-op approval — all share an overlay shell.">
          <DCArtboard id="model" label="06 · model picker" width={TW} height={TH}>
            <ScreenModelPicker {...tp} />
          </DCArtboard>
          <DCArtboard id="approval" label="07 · approval · destructive" width={TW} height={TH}>
            <ScreenApproval {...tp} />
          </DCArtboard>
          <DCArtboard id="palette" label="08 · slash palette" width={TW} height={TH}>
            <StaticPalette {...tp} />
          </DCArtboard>
        </DCSection>

        <DCSection id="config" title="Configuration"
          subtitle="Anything users care about should be reachable here without a config file edit.">
          <DCArtboard id="settings" label="09 · settings" width={TW} height={TH}>
            <ScreenSettings {...tp} />
          </DCArtboard>
          <DCArtboard id="mcp" label="10 · MCP servers" width={TW} height={TH}>
            <ScreenMCP {...tp} />
          </DCArtboard>
        </DCSection>

        <DCSection id="variations" title="Variations · theme · chrome · accent"
          subtitle="The same chat at different visual settings. The Tweaks panel flips the live prototype globally — these are pinned reference points.">
          <DCArtboard id="v-dark-cyan" label="dark · cyan · default" width={TW} height={TH}>
            <InteractiveChat theme="dark" accent="cyan" chrome="default" density="default" autoplay={false} />
          </DCArtboard>
          <DCArtboard id="v-dark-amber" label="dark · amber · rounded" width={TW} height={TH}>
            <InteractiveChat theme="dark" accent="amber" chrome="default" density="default" autoplay={false} />
          </DCArtboard>
          <DCArtboard id="v-mono" label="mono · lime · minimal chrome" width={TW} height={TH}>
            <InteractiveChat theme="mono" accent="lime" chrome="minimal" density="default" autoplay={false} />
          </DCArtboard>
          <DCArtboard id="v-light" label="light · cyan · airy" width={TW} height={TH}>
            <InteractiveChat theme="light" accent="cyan" chrome="default" density="airy" autoplay={false} />
          </DCArtboard>
          <DCArtboard id="v-magenta" label="dark · magenta · lean" width={TW} height={TH}>
            <InteractiveChat theme="dark" accent="magenta" chrome="default" density="lean" autoplay={false} />
          </DCArtboard>
        </DCSection>

      </DesignCanvas>

      <TweaksPanel title="Tweaks · zap-tui">
        <TweakSection label="Theme" />
        <TweakRadio label="Mode" value={t.theme}
          options={["dark", "light", "mono"]}
          onChange={(v) => setTweak("theme", v)} />
        <TweakColor label="Accent" value={t.accent}
          options={["cyan", "amber", "lime", "magenta"]}
          onChange={(v) => setTweak("accent", v)} />

        <TweakSection label="Chrome & density" />
        <TweakRadio label="Chrome" value={t.chrome}
          options={["default", "minimal"]}
          onChange={(v) => setTweak("chrome", v)} />
        <TweakRadio label="Density" value={t.density}
          options={["lean", "default", "airy"]}
          onChange={(v) => setTweak("density", v)} />

        <TweakSection label="Input" />
        <TweakRadio label="Input style" value={t.inputStyle}
          options={["rich", "simple"]}
          onChange={(v) => setTweak("inputStyle", v)} />
      </TweaksPanel>
    </>
  );
}

// Slash palette as a standalone snapshot artboard
function StaticPalette(props) {
  const items = window.SLASH_CMDS;
  return (
    <Shell {...props}>
      <Panel title="conversation" icon="⌬" status={<span className="dim">12 turns</span>}>
        <div style={{opacity:0.25, pointerEvents:"none"}}>
          <Msg who="you">refactor the JWT middleware…</Msg>
          <Msg who="zap">I'll read the current middleware…</Msg>
        </div>
      </Panel>
      <Overlay width="68ch" onClose={()=>{}}>
        <Panel title="command palette" icon="⌘" focus chrome="thick" style={{padding:0, background:"var(--bg)"}}>
          <div className="overlay-search">
            <span className="lbl">/</span>
            <span className="q">mo<Cursor/></span>
            <span className="grow"></span>
            <span className="dim" style={{fontSize:13}}>3 matches</span>
          </div>
          <div className="overlay-list">
            {[
              items.find(c => c.name === "model"),
              items.find(c => c.name === "compact"),
              items.find(c => c.name === "mcp"),
            ].map((c, i) => (
              <div key={c.name} className={"overlay-row " + (i === 0 ? "active" : "")}>
                <span className="key">/</span>
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
      </Overlay>
    </Shell>
  );
}

ReactDOM.createRoot(document.getElementById("root")).render(<App />);
