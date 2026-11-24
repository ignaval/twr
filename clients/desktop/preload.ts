import { contextBridge, ipcRenderer } from "electron";
import type {
  DesktopAPI,
  DesktopAppAPI,
  DesktopEvent,
  UpdateStatus,
} from "../shared/types";
import type { DesktopConnections } from "../shared/connections";
const api: DesktopAPI = {
  inboxSources: () => ipcRenderer.invoke("twr:inbox-sources"),
  sendMessage: (message) => ipcRenderer.invoke("twr:send-message", message),
  secureRead: (key) => ipcRenderer.invoke("twr:secure-read", key),
  secureWrite: (key, value) =>
    ipcRenderer.invoke("twr:secure-write", key, value),
  secureDelete: (key) => ipcRenderer.invoke("twr:secure-delete", key),
  setAppearance: (appearance) =>
    ipcRenderer.invoke("twr:set-appearance", appearance),
  voiceStatus: () => ipcRenderer.invoke("twr:voice-status"),
  voiceSetup: () => ipcRenderer.invoke("twr:voice-setup"),
  voiceArm: (on) => ipcRenderer.invoke("twr:voice-arm", on),
  voiceTranscribe: (recording, hint) =>
    ipcRenderer.invoke("twr:voice-transcribe", recording, hint),
  voiceSpeaker: (op) => ipcRenderer.invoke("twr:voice-speaker", op),
  readAloudVoices: {
    list: () => ipcRenderer.invoke("twr:voices-list"),
    select: (lang, voice) =>
      ipcRenderer.invoke("twr:voices-select", lang, voice),
    install: (model) => ipcRenderer.invoke("twr:voices-install", model),
    preview: (voice, lang) =>
      ipcRenderer.invoke("twr:voices-preview", voice, lang),
    stopPreview: () => ipcRenderer.invoke("twr:voices-preview-stop"),
    openSystemVoices: () => ipcRenderer.invoke("twr:voices-system"),
    onEvent: (callback) => {
      const listener = (_: unknown, event: DesktopEvent) => {
        if (event.type === "voice-install" && event.install)
          callback({ type: "install", install: event.install });
        else if (event.type === "voice-preview")
          callback({ type: "preview", voice: event.preview?.voice });
      };
      ipcRenderer.on("twr:event", listener);
      return () => ipcRenderer.removeListener("twr:event", listener);
    },
  },
  quit: () => ipcRenderer.invoke("twr:quit"),
  copyText: (text) => ipcRenderer.invoke("twr:copy-text", text),
  bootstrap: () => ipcRenderer.invoke("twr:bootstrap"),
  connect: (workspace, options) =>
    ipcRenderer.invoke("twr:connect", workspace, options),
  rpc: (method, params = {}) => ipcRenderer.invoke("twr:rpc", method, params),
  cli: (args) => ipcRenderer.invoke("twr:cli", args),
  catalog: () => ipcRenderer.invoke("twr:catalog"),
  terminalOpen: (request) => ipcRenderer.invoke("twr:terminal-open", request),
  terminalFocus: (focused) => ipcRenderer.send("twr:terminal-focus", focused),
  terminalAck: (id, count) => ipcRenderer.send("twr:terminal-ack", id, count),
  terminalWrite: (id, data) => ipcRenderer.send("twr:terminal-write", id, data),
  terminalResize: (id, cols, rows) =>
    ipcRenderer.send("twr:terminal-resize", id, cols, rows),
  terminalClose: (id) => ipcRenderer.invoke("twr:terminal-close", id),
  terminalRead: (id) => ipcRenderer.invoke("twr:terminal-read", id),
  pickDirectory: () => ipcRenderer.invoke("twr:pick-directory"),
  pickFile: (request) => ipcRenderer.invoke("twr:pick-file", request),
  openExternal: (url) => ipcRenderer.invoke("twr:open-external", url),
  reveal: (path) => ipcRenderer.invoke("twr:reveal", path),
  configRead: () => ipcRenderer.invoke("twr:config-read"),
  configSave: (content, hash) =>
    ipcRenderer.invoke("twr:config-save", content, hash),
  configSet: (table, key, value) =>
    ipcRenderer.invoke("twr:config-set", table, key, value),
  exportText: (name, content) =>
    ipcRenderer.invoke("twr:export", name, content),
  onEvent: (callback) => {
    const listener = (_: unknown, event: DesktopEvent) => callback(event);
    ipcRenderer.on("twr:event", listener);
    return () => ipcRenderer.removeListener("twr:event", listener);
  },
};
contextBridge.exposeInMainWorld("twrLocal", api);
const connections: DesktopConnections = {
  listHubs: () => ipcRenderer.invoke("twr:remote-list"),
  pair: (request) => ipcRenderer.invoke("twr:remote-pair", request),
  connect: (request) => ipcRenderer.invoke("twr:remote-connect", request),
  send: (request) => ipcRenderer.invoke("twr:remote-send", request),
  disconnect: () => ipcRenderer.invoke("twr:remote-disconnect"),
  removeHub: (request) => ipcRenderer.invoke("twr:remote-remove", request),
  resetSavedHubs: () => ipcRenderer.invoke("twr:remote-reset"),
  gatewayStatus: () => ipcRenderer.invoke("twr:gateway-status"),
  cancelInvitation: () => ipcRenderer.invoke("twr:gateway-cancel-invitation"),
  stopGateway: () => ipcRenderer.invoke("twr:gateway-stop"),
  settings: () => ipcRenderer.invoke("twr:gateway-settings"),
  check: (request) => ipcRenderer.invoke("twr:gateway-check", request),
  setup: (request) => ipcRenderer.invoke("twr:gateway-setup", request),
  restart: (request) => ipcRenderer.invoke("twr:gateway-restart", request),
  invite: (request) => ipcRenderer.invoke("twr:gateway-invite", request),
  deactivateLocal: () => ipcRenderer.invoke("twr:deactivate-local"),
  updateRoutes: (request) =>
    ipcRenderer.invoke("twr:remote-update-routes", request),
  addListener: async (event, callback) => {
    const channel =
      event === "network" ? "twr:remote-network" : "twr:remote-message";
    const listener = (_: unknown, value: any) => callback(value);
    ipcRenderer.on(channel, listener);
    return {
      remove: async () => {
        ipcRenderer.removeListener(channel, listener);
      },
    };
  },
};
contextBridge.exposeInMainWorld("twrRemote", connections);

contextBridge.exposeInMainWorld(
  "twrUploadRequest",
  (workspace: string, method: string, params: unknown) =>
    ipcRenderer.invoke("twr:upload-request", workspace, method, params),
);

const application: DesktopAppAPI = {
  settings: () => ipcRenderer.invoke("twr:app-settings"),
  setLaunchAtLogin: (enabled) => ipcRenderer.invoke("twr:app-login", enabled),
  setTray: (enabled) => ipcRenderer.invoke("twr:app-tray", enabled),
  setNotifications: (enabled) =>
    ipcRenderer.invoke("twr:app-notifications", enabled),
  setBackground: (enabled) => ipcRenderer.invoke("twr:app-background", enabled),
  setAutoUpdate: (enabled) =>
    ipcRenderer.invoke("twr:app-auto-update", enabled),
  checkForUpdates: () => ipcRenderer.invoke("twr:app-update-check"),
  downloadUpdate: () => ipcRenderer.invoke("twr:app-update-download"),
  installUpdate: () => ipcRenderer.invoke("twr:app-update-install"),
  setAttention: (summary) => ipcRenderer.send("twr:app-attention", summary),
  reportError: (report) => ipcRenderer.send("twr:app-error", report),
  diagnostics: () => ipcRenderer.invoke("twr:app-diagnostics"),
  openLogs: () => ipcRenderer.invoke("twr:app-open-logs"),
  onUpdate: (callback) => {
    const listener = (_: unknown, status: UpdateStatus) => callback(status);
    ipcRenderer.on("twr:app-update", listener);
    return () => ipcRenderer.removeListener("twr:app-update", listener);
  },
  onSaveNow: (save) => {
    const listener = (_: unknown, id: number) =>
      void Promise.resolve()
        .then(save)
        .catch(() => {})
        .finally(() => ipcRenderer.send("twr:app-saved", id));
    ipcRenderer.on("twr:app-save-now", listener);
    return () => ipcRenderer.removeListener("twr:app-save-now", listener);
  },
};
contextBridge.exposeInMainWorld("twrApp", application);
