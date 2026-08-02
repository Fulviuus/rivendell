import { useEffect, useState } from "react";
import { NewThreadModal } from "./components/NewThreadModal";
import { Sidebar } from "./components/Sidebar";
import { ThreadList } from "./components/ThreadList";
import { ThreadView } from "./components/ThreadView";
import { errText } from "./api";
import { useStore } from "./store";
import { resolve, setTheme, storedTheme, type Theme } from "./theme";
import { Button, Icon } from "./ui";

export default function App() {
  const { boot, ready, roomId, threadId, selectThread, toast, notify } = useStore();
  const [showNewThread, setShowNewThread] = useState(false);
  const [theme, setThemeState] = useState<Theme>(storedTheme);
  const [fatal, setFatal] = useState<string | null>(null);

  useEffect(() => {
    boot().catch((e) => setFatal(errText(e)));
  }, [boot]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "n" && roomId !== null) {
        e.preventDefault();
        setShowNewThread(true);
        return;
      }
      if (e.key !== "Escape") return;

      // Escape is already spoken for in three places, and taking it blindly
      // would be worse than not having it: a modal on top owns it, the mention
      // list uses it to dismiss itself, and a half-written reply or an edit in
      // progress would be thrown away with the view. Only claim it when
      // nothing else is holding it.
      if (document.querySelector("[data-modal]")) return;
      const el = document.activeElement as HTMLElement | null;
      if (el && (el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable)) {
        return;
      }
      if (useStore.getState().threadId !== null) {
        e.preventDefault();
        void useStore.getState().selectThread(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [roomId]);

  function toggleTheme() {
    const next: Theme = resolve(theme) === "dark" ? "light" : "dark";
    setTheme(next);
    setThemeState(next);
  }

  if (fatal) {
    return (
      <div className="flex h-full items-center justify-center p-10">
        <div className="max-w-md text-center">
          <p className="font-medium text-rose-600 dark:text-rose-300">
            Rivendell could not start
          </p>
          <pre className="mt-2 rounded-lg bg-card p-3 text-left font-mono text-[11.5px] whitespace-pre-wrap text-muted ring-1 ring-line">
            {fatal}
          </pre>
        </div>
      </div>
    );
  }

  if (!ready) {
    return (
      <div className="flex h-full items-center justify-center">
        <span className="pulse-soft text-[12.5px] text-muted">starting…</span>
      </div>
    );
  }

  return (
    <div className="flex h-full">
      <Sidebar />
      <ThreadList onNew={() => setShowNewThread(true)} />

      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <div
          data-tauri-drag-region
          className="titlebar-drag flex h-11 shrink-0 items-center justify-end gap-1 border-b border-line bg-card px-3"
        >
          {/* Panel chrome, deliberately a row above the thread's own Close and
              Resolve buttons: those act on the issue, this only stops showing
              it. Same glyph, but nothing here can be confused for the other. */}
          {threadId !== null && (
            <Button
              variant="subtle"
              size="sm"
              title="Stop showing this thread — it stays exactly as it is (Esc)"
              onClick={() => void selectThread(null)}
            >
              <Icon name="x" size={14} />
            </Button>
          )}
          <Button
            variant="subtle"
            size="sm"
            title={resolve(theme) === "dark" ? "Switch to light" : "Switch to dark"}
            onClick={toggleTheme}
          >
            <Icon name={resolve(theme) === "dark" ? "sun" : "moon"} size={14} />
          </Button>
        </div>
        <ThreadView />
      </div>

      {showNewThread && roomId !== null && (
        <NewThreadModal onClose={() => setShowNewThread(false)} />
      )}

      {toast && (
        <div
          className={`fixed bottom-4 left-1/2 z-[60] max-w-lg -translate-x-1/2 rounded-xl px-3.5 py-2.5 text-[12.5px] shadow-pop ring-1 ${
            toast.kind === "error"
              ? "bg-rose-50 text-rose-800 ring-rose-300 dark:bg-rose-500/12 dark:text-rose-200 dark:ring-rose-500/30"
              : "bg-card text-body ring-line"
          }`}
          onClick={() => notify("info", "")}
        >
          {toast.text}
        </div>
      )}
    </div>
  );
}
