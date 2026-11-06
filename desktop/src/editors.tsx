import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useRef,
  type ReactNode,
} from "react";
interface EditorSessions {
  get: (key: string) => any;
  set: (key: string, value: any) => void;
  clear: (key: string) => void;
  forget: (namespace: string) => void;
}
const Context = createContext<EditorSessions>(null!);
/** Drafts remain in memory across navigation, never in localStorage (configuration can
 * contain credentials). Closing the application asks before discarding them. */
export function EditorSessionsProvider({ children }: { children: ReactNode }) {
  const drafts = useRef(new Map<string, any>());
  const sessions = useMemo<EditorSessions>(
    () => ({
      get: (key) => drafts.current.get(key),
      set: (key, value) => {
        drafts.current.set(key, value);
      },
      clear: (key) => {
        drafts.current.delete(key);
      },
      forget: (namespace) => {
        for (const key of drafts.current.keys())
          if (key.startsWith(namespace + ":")) drafts.current.delete(key);
      },
    }),
    [],
  );
  useEffect(() => {
    const beforeUnload = (event: BeforeUnloadEvent) => {
      if (drafts.current.size) {
        event.preventDefault();
        event.returnValue = "";
      }
    };
    window.addEventListener("beforeunload", beforeUnload);
    return () => window.removeEventListener("beforeunload", beforeUnload);
  }, []);
  return <Context.Provider value={sessions}>{children}</Context.Provider>;
}
export function useEditorSessions() {
  const sessions = useContext(Context);
  const namespace = document.documentElement.dataset.hub || "desktop";
  return useMemo<EditorSessions>(
    () => ({
      get: (key) => sessions.get(`${namespace}:${key}`),
      set: (key, value) => sessions.set(`${namespace}:${key}`, value),
      clear: (key) => sessions.clear(`${namespace}:${key}`),
      forget: sessions.forget,
    }),
    [sessions, namespace],
  );
}
