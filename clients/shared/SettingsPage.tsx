import { useCallback, useEffect, useId, useRef, useState } from "react";
import { FolderOpen, Save } from "lucide-react";
import {
  rovingIndex,
  setTerminalScreenReader,
  terminalScreenReader,
} from "./a11y";
import { AppearanceOptions } from "./AppearanceOptions";
import { AppOptions } from "./AppOptions";
import { useEditorSessions } from "./editors";
import { useHostAPI } from "./host";
import { HubStoragePanel, SpeakersPanel } from "./HubPanels";
import { LayoutOptions } from "./LayoutOptions";
import { LoopSettingsPanel } from "./LoopViews";
import { OfflineOptions } from "./OfflineOptions";
import { OrchestratorSettings } from "./OrchestratorSettings";
import { ReadAloudVoiceOptions } from "./ReadAloudVoiceOptions";
import {
  initialSettingsTab,
  saveSettingsTab,
  savedSettingsTab,
  settingsTabs,
  tabForSection,
  type Section,
  type SettingsTab,
} from "./settingsModel";
import { ThreadDefaults, type SaveContext } from "./ThreadDefaults";
import { Toggle } from "./Toggle";
import type { Json } from "./types";
import { DataView, Spinner } from "./ui";
import "./settings.css";

type Rpc = (method: string, params?: Json) => Promise<Json>;

interface ConfigFile {
  content: string;
  hash: string;
  path: string;
  settings?: Json;
}

/** Settings, in tabs: General (this device), Agents (the orchestrator and new threads'
 * defaults), Loops, Voice and Advanced. Read aloud (`speech`) opens on Voice and Hub &
 * storage (`maintenance`) on Advanced; otherwise the tab used last on this device. */
export function Settings({
  section,
  fail,
  rpc,
  action,
  cliAction,
  canAdmin,
  connected,
  refreshKey,
  orchestratorAccess,
  infoChanged,
}: {
  section: Section;
  canAdmin: boolean;
  connected: boolean;
  refreshKey?: number;
  orchestratorAccess?: string;
  infoChanged?: () => void;
  fail: (error: unknown) => void;
  rpc: Rpc;
  action: (id: string, initial?: Json) => void;
  cliAction: (path: string, initial?: Json) => void;
}) {
  const hostApi = useHostAPI();
  const editors = useEditorSessions();
  const ids = useId();
  const voicePrefs = !!hostApi.readAloudVoices;
  const tabs = settingsTabs(canAdmin, voicePrefs);
  const [tab, setTab] = useState<SettingsTab>(() =>
    initialSettingsTab(section, savedSettingsTab(), tabs),
  );
  // Read aloud and Hub & storage open their tab each time they are chosen.
  useEffect(() => {
    const wanted = tabForSection(section);
    if (wanted && tabs.some((t) => t.id === wanted)) setTab(wanted);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [section]);
  const shown = tabs.some((t) => t.id === tab) ? tab : "general";
  const choose = (next: SettingsTab) => {
    setTab(next);
    saveSettingsTab(next);
  };
  const tabButtons = useRef<Partial<Record<SettingsTab, HTMLButtonElement>>>(
    {},
  );

  // The hub's settings (admin only): the config file, the effective configuration, the
  // orchestrator and what the hub found installed.
  const [config, setConfig] = useState<ConfigFile>();
  const [text, setText] = useState("");
  const [loaded, setLoaded] = useState<Json>();
  const [show, setShow] = useState<Json>();
  const [info, setInfo] = useState<Json>();
  const readShow = useCallback(() => {
    // An older hub has no model list: the pickers stay free text.
    void rpc("orchestrator.show")
      .then(setShow)
      .catch(() => {});
  }, [rpc]);
  useEffect(() => {
    if (!canAdmin) return;
    void hostApi
      .configRead()
      .then((c) => {
        setConfig(c);
        setText(editors.get("config")?.text ?? c.content);
      })
      .catch(fail);
    void rpc("config.show").then(setLoaded).catch(fail);
    readShow();
    void rpc("hub.info")
      .then(setInfo)
      .catch(() => {});
  }, [rpc, fail, canAdmin]);
  useEffect(() => {
    if (!config || text === config.content) return;
    const listener = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", listener);
    return () => window.removeEventListener("beforeunload", listener);
  }, [text, config]);
  const reread = () =>
    void hostApi
      .configRead()
      .then((c) => {
        setConfig(c);
        if (!editors.get("config")) setText(c.content);
      })
      .catch(fail);
  const context: SaveContext = { fail, saved: reread };
  const settings = config?.settings || loaded;

  let panel;
  if (shown === "general")
    panel = (
      <>
        <section className="settings-section">
          <h2 id={`${ids}-appearance`}>Appearance</h2>
          <p>
            {document.documentElement.dataset.mobile === "true"
              ? "System follows your device."
              : "System follows your desktop. On Omarchy, its current theme is used when available."}
          </p>
          <AppearanceOptions labelledBy={`${ids}-appearance`} />
        </section>
        <LayoutOptions />
        <AccessibilityOptions />
        <AppOptions parts={["application", "updates"]} />
      </>
    );
  else if (shown === "agents")
    panel = (
      <>
        <OrchestratorSettings
          rpc={rpc}
          action={action}
          show={show}
          reload={readShow}
          loaded={settings}
          context={context}
          orchestratorAccess={orchestratorAccess}
          refreshKey={refreshKey}
          infoChanged={infoChanged}
        />
        {settings ? (
          <ThreadDefaults
            loaded={settings}
            info={info}
            show={show}
            context={context}
          />
        ) : (
          <p className="transcript-loading">
            <Spinner /> Loading settings…
          </p>
        )}
      </>
    );
  else if (shown === "loops")
    panel = <LoopSettingsPanel connected={connected} />;
  else if (shown === "voice")
    panel = (
      <>
        {hostApi.readAloudVoices && (
          <ReadAloudVoiceOptions voices={hostApi.readAloudVoices} />
        )}
        {canAdmin && (
          <SpeakersPanel
            rpc={rpc}
            action={action}
            cliAction={cliAction}
            fail={fail}
            refreshKey={refreshKey}
          />
        )}
      </>
    );
  else
    panel = (
      <>
        <OfflineOptions />
        <AppOptions parts={["diagnostics"]} />
        {canAdmin && (
          <>
            <HubStoragePanel
              rpc={rpc}
              action={action}
              cliAction={cliAction}
              fail={fail}
              refreshKey={refreshKey}
            />
            {config && (
              <ConfigFileEditor
                config={config}
                text={text}
                setText={setText}
                setConfig={setConfig}
                fail={fail}
              />
            )}
            {loaded && (
              <section className="settings-section">
                <h2>Effective configuration</h2>
                <details>
                  <summary>Show what the running hub uses</summary>
                  <p className="muted">Credentials are left out.</p>
                  <DataView value={loaded} />
                </details>
              </section>
            )}
          </>
        )}
      </>
    );

  return (
    <div className="manager settings-page">
      <header className="section-heading">
        <div>
          <h1>Settings</h1>
          <p>
            {canAdmin
              ? "Make twr feel at home."
              : "Preferences for this device."}
          </p>
        </div>
      </header>
      <div
        className="settings-tabs"
        role="tablist"
        aria-label="Settings sections"
        onKeyDown={(event) => {
          const index = tabs.findIndex((t) => t.id === shown);
          const next = rovingIndex(event.key, index, tabs.length, "horizontal");
          if (next === undefined) return;
          event.preventDefault();
          choose(tabs[next].id);
          tabButtons.current[tabs[next].id]?.focus();
        }}
      >
        {tabs.map((t) => (
          <button
            key={t.id}
            ref={(node) => {
              if (node) tabButtons.current[t.id] = node;
            }}
            type="button"
            role="tab"
            id={`${ids}-tab-${t.id}`}
            aria-selected={shown === t.id}
            aria-controls={`${ids}-panel`}
            tabIndex={shown === t.id ? 0 : -1}
            className={shown === t.id ? "active" : ""}
            onClick={() => choose(t.id)}
          >
            {t.label}
          </button>
        ))}
      </div>
      <div
        id={`${ids}-panel`}
        role="tabpanel"
        aria-labelledby={`${ids}-tab-${shown}`}
        className="settings-panel"
      >
        {panel}
      </div>
    </div>
  );
}

function AccessibilityOptions() {
  const [reader, setReader] = useState(terminalScreenReader);
  return (
    <section className="settings-section">
      <h2>Accessibility</h2>
      <Toggle
        label="Terminal screen reader support"
        detail="Lets VoiceOver, Orca and TalkBack read terminal output. It can slow very busy terminals, so it is off by default."
        checked={reader}
        onChange={(on) => {
          setReader(on);
          setTerminalScreenReader(on);
        }}
      />
    </section>
  );
}

/** Settings › Advanced › Config file: the whole file, behind "Edit config file…" (open
 * while there are unsaved edits). Comments and formatting are kept; a save over a file
 * changed since it was read is refused. */
function ConfigFileEditor({
  config,
  text,
  setText,
  setConfig,
  fail,
}: {
  config: ConfigFile;
  text: string;
  setText: (text: string) => void;
  setConfig: (config: ConfigFile) => void;
  fail: (error: unknown) => void;
}) {
  const hostApi = useHostAPI();
  const editors = useEditorSessions();
  const ids = useId();
  const [open, setOpen] = useState(() => text !== config.content);
  const [busy, setBusy] = useState(false);
  const [saved, setSaved] = useState(false);
  return (
    <section className="settings-section">
      <h2>Config file</h2>
      <p>Every setting, including the ones not shown here.</p>
      <button
        type="button"
        aria-expanded={open}
        aria-controls={`${ids}-editor`}
        onClick={() => setOpen(!open)}
      >
        {open ? "Hide config file" : "Edit config file…"}
      </button>
      <div id={`${ids}-editor`} hidden={!open} className="config-file">
        {open && (
          <>
            <div className="config-path">
              <code>{config.path}</code>
              <button
                className="icon-button"
                aria-label="Show configuration file"
                title="Show configuration file"
                onClick={() => void hostApi.reveal(config.path)}
              >
                <FolderOpen size={16} />
              </button>
            </div>
            <textarea
              className="config-editor"
              aria-label="Configuration TOML"
              value={text}
              onChange={(e) => {
                setText(e.target.value);
                setSaved(false);
                editors.set("config", {
                  text: e.target.value,
                  hash: editors.get("config")?.hash ?? config.hash,
                });
              }}
              spellCheck={false}
              rows={18}
            />
            <div className="manager-actions">
              <button
                className="primary"
                disabled={busy || text === config.content}
                onClick={() => {
                  setBusy(true);
                  void hostApi
                    .configSave(
                      text,
                      editors.get("config")?.hash ?? config.hash,
                    )
                    .then(() => hostApi.configRead())
                    .then((c) => {
                      editors.clear("config");
                      setConfig(c);
                      setText(c.content);
                      setSaved(true);
                    })
                    .catch(fail)
                    .finally(() => setBusy(false));
                }}
              >
                {busy ? <Spinner /> : <Save size={15} />}Save configuration
              </button>
              <button
                onClick={() => {
                  if (
                    text !== config.content &&
                    !window.confirm("Discard your unsaved configuration edits?")
                  )
                    return;
                  void hostApi
                    .configRead()
                    .then((c) => {
                      editors.clear("config");
                      setConfig(c);
                      setText(c.content);
                    })
                    .catch(fail);
                }}
              >
                Reload from disk
              </button>
            </div>
          </>
        )}
      </div>
      <span className="success-text" role="status">
        {saved ? "Saved. Hub settings apply after the hub restarts." : ""}
      </span>
    </section>
  );
}
