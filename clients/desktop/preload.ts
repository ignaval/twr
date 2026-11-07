import { contextBridge, ipcRenderer } from "electron";
import type { DesktopAPI, DesktopEvent } from "../shared/types";
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
  voiceArm: (on) => ipcRenderer.invoke("twr:voice-arm", on),
  voiceTranscribe: (recording, hint) =>
    ipcRenderer.invoke("twr:voice-transcribe", recording, hint),
  voiceSpeaker: (op) => ipcRenderer.invoke("twr:voice-speaker", op),
  quit: () => ipcRenderer.invoke("twr:quit"),
  copyText: (text) => ipcRenderer.invoke("twr:copy-text", text),
  bootstrap: () => ipcRenderer.invoke("twr:bootstrap"),
  connect: (workspace) => ipcRenderer.invoke("twr:connect", workspace),
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
  addListener: async (_event, callback) => {
    const listener = (_: unknown, value: any) => callback(value);
    ipcRenderer.on("twr:remote-message", listener);
    return {
      remove: async () => {
        ipcRenderer.removeListener("twr:remote-message", listener);
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
