import { contextBridge, ipcRenderer } from "electron";
import type { DesktopAPI, DesktopEvent } from "../shared/types";
const api: DesktopAPI = {
  setAppearance: (appearance) =>
    ipcRenderer.invoke("twr:set-appearance", appearance),
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
contextBridge.exposeInMainWorld("twr", api);
