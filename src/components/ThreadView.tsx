import { useEffect, useMemo, useRef, useState } from "react";
import { openPath } from "@tauri-apps/plugin-opener";
import { api, errText } from "../api";
import { renderMarkdown } from "../markdown";
import { useStore } from "../store";
import {
  ago,
  Avatar,
  Button,
  CopyButton,
  Empty,
  Icon,
  Modal,
  StatusChip,
  TagChip,
  Textarea,
  VerdictChip,
  agentTone,
} from "../ui";
import type { AgentRun, Message, ThreadContextItem } from "../types";

export function ThreadView() {
  const { thread, tags, agents, notify, refreshThread } = useStore();
  const [composing, setComposing] = useState("");
  const [resolving, setResolving] = useState(false);
  const [busy, setBusy] = useState(false);
  const bottom = useRef<HTMLDivElement>(null);

  const tag = useMemo(() => tags.find((t) => t.key === thread?.tag), [tags, thread?.tag]);
  const messageCount = thread?.messages.length ?? 0;

  useEffect(() => {
    bottom.current?.scrollIntoView({ behavior: "smooth", block: "end" });
  }, [messageCount, thread?.id]);

  if (!thread) {
    return (
      <Empty icon="hash" title="No thread selected">
        Pick a thread, or open a new one to put something in front of your assistants.
      </Empty>
    );
  }

  const done = thread.status === "RESOLVED" || thread.status === "WONTFIX";
  const running = thread.runs.filter((r) => r.status === "RUNNING");

  async function post() {
    if (!composing.trim() || !thread) return;
    setBusy(true);
    try {
      await api.reply({ thread_id: thread.id, body: composing });
      setComposing("");
      await refreshThread();
    } catch (e) {
      notify("error", errText(e));
    } finally {
      setBusy(false);
    }
  }

  async function redispatch() {
    if (!thread) return;
    setBusy(true);
    try {
      const n = await api.dispatchThread(thread.id);
      notify("info", n > 0 ? `Dispatched ${n} assistant(s).` : "Nothing to dispatch.");
    } catch (e) {
      notify("error", errText(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col bg-canvas">
      <header className="shrink-0 border-b border-line bg-card px-5 py-3 shadow-card">
        <div className="flex items-start gap-3">
          <div className="min-w-0 flex-1">
            <div className="flex flex-wrap items-center gap-2">
              {tag && <TagChip color={tag.color} label={tag.label} />}
              <StatusChip status={thread.status} />
              <span className="text-[11.5px] text-muted">
                opened by {thread.author_name} · {ago(thread.created_at)}
              </span>
            </div>
            <h1 className="mt-1.5 text-[15px] leading-snug font-semibold text-strong">
              {thread.title}
            </h1>
            <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11.5px] text-muted">
              {thread.git_ref && (
                <span className="inline-flex items-center gap-1" title={thread.git_ref}>
                  <Icon name="git" size={11} />
                  pinned at {thread.git_ref.slice(0, 8)}
                  {thread.git_dirty && " (tree was dirty)"}
                </span>
              )}
              {thread.quorum > 0 && (
                <span>
                  {thread.responder_count}/{thread.quorum} assistants replied
                </span>
              )}
              {thread.cost_usd > 0 && (
                <span className="tabular-nums">${thread.cost_usd.toFixed(3)} spent</span>
              )}
            </div>
          </div>

          <div className="flex shrink-0 gap-1.5">
            {!done && (
              <>
                <Button size="sm" onClick={redispatch} disabled={busy} title="Run the assistants again">
                  <Icon name="play" size={12} />
                  Dispatch
                </Button>
                <Button size="sm" variant="primary" onClick={() => setResolving(true)}>
                  <Icon name="check" size={13} />
                  Resolve
                </Button>
              </>
            )}
            {thread.export_path && (
              <Button
                size="sm"
                title={thread.export_path}
                onClick={() => openPath(thread.export_path!).catch((e) => notify("error", errText(e)))}
              >
                <Icon name="file" size={12} />
                Record
              </Button>
            )}
          </div>
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-5 py-4">
        <article className="mb-5">
          <div className="mb-1.5 flex items-center gap-2">
            <Avatar
              name={thread.author_name}
              icon={thread.author_icon}
              color={thread.author_color}
              size={24}
            />
            <span className="font-semibold text-strong">{thread.author_name}</span>
            <span className="rounded bg-chip px-1.5 py-px text-[10.5px] tracking-wide text-muted uppercase">
              the ask
            </span>
          </div>
          <div
            className={`prose-msg rounded-xl border-l-2 bg-card p-3.5 shadow-card ring-1 ring-line ${
              agentTone(thread.author_name, thread.author_color).edge
            }`}
            dangerouslySetInnerHTML={{ __html: renderMarkdown(thread.body) }}
          />
        </article>

        {thread.context.length > 0 && (
          <section className="mb-5">
            <h2 className="mb-2 flex items-center gap-1.5 text-[11.5px] font-semibold tracking-wide text-muted uppercase">
              <Icon name="file" size={12} />
              Pinned context
              <span className="font-normal normal-case text-faint">
                — as it was when the thread was opened
              </span>
            </h2>
            <div className="space-y-2">
              {thread.context.map((c) => (
                <ContextBlock key={c.id} item={c} />
              ))}
            </div>
          </section>
        )}

        <section className="space-y-4">
          {thread.messages.map((m) => (
            <MessageCard key={m.id} message={m} />
          ))}
        </section>

        {running.length > 0 && (
          <div className="mt-4 flex items-center gap-2 text-[12.5px] text-muted">
            <span className="h-1.5 w-1.5 animate-pulse rounded-full bg-accent" />
            {running.map((r) => r.agent_name).join(", ")} {running.length === 1 ? "is" : "are"}{" "}
            working…
          </div>
        )}

        {thread.runs.length > 0 && <RunLogs runs={thread.runs} />}

        {thread.resolution_summary && (
          <div className="mt-5 rounded-xl border-l-2 border-l-emerald-400 bg-emerald-50 p-3.5 ring-1 ring-emerald-200 dark:border-l-emerald-500 dark:bg-emerald-500/8 dark:ring-emerald-500/20">
            <div className="mb-1.5 flex items-center gap-1.5 text-[11.5px] font-semibold tracking-wide text-emerald-700 uppercase dark:text-emerald-300">
              <Icon name="check" size={12} />
              Resolution
            </div>
            <div
              className="prose-msg"
              dangerouslySetInnerHTML={{ __html: renderMarkdown(thread.resolution_summary) }}
            />
            {thread.export_path && (
              <p className="mt-2 font-mono text-[11px] text-muted">{thread.export_path}</p>
            )}
          </div>
        )}

        <div ref={bottom} />
      </div>

      {!done && (
        <div className="shrink-0 border-t border-line bg-card px-5 py-3">
          <Textarea
            rows={2}
            value={composing}
            placeholder="Add what you have learned — assistants see it on their next pass. ⌘↵ to post."
            onChange={(e) => setComposing(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                e.preventDefault();
                post();
              }
            }}
          />
          <div className="mt-2 flex items-center justify-between">
            <span className="text-[11.5px] text-muted">
              posting as {agents.find((a) => a.role === "HUMAN")?.name ?? "you"}
            </span>
            <Button variant="primary" size="sm" onClick={post} disabled={busy || !composing.trim()}>
              Post update
            </Button>
          </div>
        </div>
      )}

      {resolving && (
        <ResolveModal
          onClose={() => setResolving(false)}
          onDone={async () => {
            setResolving(false);
            await refreshThread();
          }}
        />
      )}
    </div>
  );
}

function ContextBlock({ item }: { item: ThreadContextItem }) {
  const [open, setOpen] = useState(item.content.split("\n").length <= 40);
  const header =
    item.path && item.start_line && item.end_line
      ? `${item.path}:${item.start_line}-${item.end_line}`
      : (item.path ?? (item.kind === "diff" ? "working-tree diff" : item.kind));

  return (
    <div className="overflow-hidden rounded-xl bg-card shadow-card ring-1 ring-line">
      <button
        onClick={() => setOpen(!open)}
        className="flex w-full items-center gap-2 px-3 py-2 text-left transition hover:bg-hover"
      >
        <Icon name={item.kind === "diff" ? "git" : "file"} size={12} className="text-muted" />
        <span className="flex-1 truncate font-mono text-[11.5px] text-soft">{header}</span>
        <span className="text-[11px] text-faint">{item.content.split("\n").length} lines</span>
        <Icon
          name="chevron"
          size={11}
          className={`text-faint transition-transform ${open ? "rotate-90" : ""}`}
        />
      </button>
      {open && (
        <pre className="max-h-96 overflow-auto border-t border-line bg-code px-3 py-2.5 font-mono text-[11.5px] leading-relaxed">
          {item.kind === "diff"
            ? item.content.split("\n").map((line, i) => (
                <div
                  key={i}
                  className={
                    line.startsWith("+") && !line.startsWith("+++")
                      ? "text-emerald-700 dark:text-emerald-300"
                      : line.startsWith("-") && !line.startsWith("---")
                        ? "text-rose-700 dark:text-rose-300"
                        : line.startsWith("@@")
                          ? "text-accent-text"
                          : "text-soft"
                  }
                >
                  {line || " "}
                </div>
              ))
            : <span className="text-soft">{item.content}</span>}
        </pre>
      )}
    </div>
  );
}

function MessageCard({ message: m }: { message: Message }) {
  const isAssistant = m.agent_role === "ASSISTANT";
  const tone = agentTone(m.agent_name, m.color);
  return (
    <article>
      <div className="mb-1.5 flex flex-wrap items-center gap-2">
        <Avatar name={m.agent_name} icon={m.icon} color={m.color} size={24} />
        <span className="font-semibold text-strong">{m.agent_name}</span>
        {!isAssistant && (
          <span className="rounded bg-chip px-1.5 py-px text-[10.5px] tracking-wide text-muted uppercase">
            {m.agent_role}
          </span>
        )}
        {m.verdict && <VerdictChip verdict={m.verdict} severity={m.severity} />}
        <span className="text-[11.5px] text-faint">{ago(m.created_at)}</span>
        {m.tokens_out > 0 && (
          <span className="text-[11px] tabular-nums text-faint">
            {(m.tokens_in + m.tokens_out).toLocaleString()} tok
          </span>
        )}
      </div>
      <div
        className={`prose-msg rounded-xl border-l-2 p-3.5 shadow-card ring-1 ring-line ${tone.edge} ${
          isAssistant ? `bg-card ${tone.tint}` : "bg-chip/50"
        }`}
        dangerouslySetInnerHTML={{ __html: renderMarkdown(m.body) }}
      />
      {m.refs.length > 0 && (
        <ul className="mt-1.5 space-y-0.5 pl-1">
          {m.refs.map((r, i) => (
            <li key={i} className="flex gap-1.5 text-[11.5px] text-muted">
              <Icon name="file" size={11} className="mt-0.5 text-faint" />
              <span className="font-mono text-soft">
                {r.path}
                {r.line ? `:${r.line}` : ""}
              </span>
              {r.note && <span className="text-faint">— {r.note}</span>}
            </li>
          ))}
        </ul>
      )}
    </article>
  );
}

function RunLogs({ runs }: { runs: AgentRun[] }) {
  const [open, setOpen] = useState(false);
  const failed = runs.filter((r) => r.status === "FAILED" || r.status === "KILLED");

  return (
    <div className="mt-5">
      <button
        onClick={() => setOpen(!open)}
        className="flex items-center gap-1.5 text-[11.5px] text-muted transition hover:text-strong"
      >
        <Icon
          name="chevron"
          size={11}
          className={`transition-transform ${open ? "rotate-90" : ""}`}
        />
        {runs.length} run{runs.length === 1 ? "" : "s"}
        {failed.length > 0 && (
          <span className="text-rose-600 dark:text-rose-400">· {failed.length} failed</span>
        )}
      </button>
      {open && (
        <div className="mt-2 space-y-2">
          {runs.map((r) => (
            <div key={r.id} className="overflow-hidden rounded-lg bg-card shadow-card ring-1 ring-line">
              <div className="flex items-center gap-2 px-3 py-1.5 text-[11.5px]">
                <span className="font-medium text-soft">{r.agent_name}</span>
                <span
                  className={
                    r.status === "EXITED"
                      ? "text-emerald-600 dark:text-emerald-400"
                      : r.status === "RUNNING"
                        ? "text-accent-text"
                        : "text-rose-600 dark:text-rose-400"
                  }
                >
                  {r.status}
                  {r.exit_code !== null && r.exit_code !== 0 && ` (${r.exit_code})`}
                </span>
                <span className="ml-auto text-faint">{ago(r.started_at)}</span>
              </div>
              {r.log && (
                <pre className="max-h-64 overflow-auto border-t border-line bg-code px-3 py-2 font-mono text-[11px] leading-relaxed whitespace-pre-wrap text-muted">
                  {r.log}
                </pre>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function ResolveModal({ onClose, onDone }: { onClose: () => void; onDone: () => void }) {
  const { thread, notify } = useStore();
  const [summary, setSummary] = useState("");
  const [status, setStatus] = useState("RESOLVED");
  const [busy, setBusy] = useState(false);
  const [written, setWritten] = useState<string | null>(null);

  async function submit() {
    if (!thread || !summary.trim()) return;
    setBusy(true);
    try {
      const path = await api.resolveThread(thread.id, summary, status);
      if (path) {
        setWritten(path);
      } else {
        onDone();
      }
    } catch (e) {
      notify("error", errText(e));
      setBusy(false);
    }
  }

  if (written) {
    return (
      <Modal title="Resolved" subtitle="Written into the repo" onClose={onDone}>
        <p className="text-[13px] text-soft">
          The decision record is committed to your working tree at:
        </p>
        <div className="mt-2 flex items-center gap-2">
          <code className="flex-1 truncate rounded-lg bg-code px-2.5 py-1.5 font-mono text-[11.5px] text-soft ring-1 ring-line">
            {written}
          </code>
          <CopyButton text={written} />
        </div>
        <div className="mt-4 flex justify-end">
          <Button variant="primary" onClick={onDone}>
            Done
          </Button>
        </div>
      </Modal>
    );
  }

  return (
    <Modal
      title="Resolve thread"
      subtitle="This summary is written into the repo as a durable decision record."
      onClose={onClose}
    >
      <div className="space-y-3.5">
        <Textarea
          autoFocus
          rows={6}
          value={summary}
          placeholder="What was decided, and why. Write it for whoever reads it in six months — 'fixed' helps nobody."
          onChange={(e) => setSummary(e.target.value)}
        />
        <div className="flex gap-1.5">
          {[
            ["RESOLVED", "Resolved"],
            ["WONTFIX", "Won't fix"],
            ["BLOCKED", "Blocked"],
          ].map(([value, label]) => (
            <button
              key={value}
              onClick={() => setStatus(value)}
              className={`rounded-lg px-2.5 py-1 text-[12px] ring-1 ring-inset transition ${
                status === value
                  ? "bg-accent-soft text-accent-text ring-accent/40"
                  : "bg-field text-muted ring-line hover:text-strong"
              }`}
            >
              {label}
            </button>
          ))}
        </div>
        <div className="flex justify-end gap-2">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} disabled={busy || !summary.trim()}>
            {status === "BLOCKED" ? "Mark blocked" : "Resolve & write record"}
          </Button>
        </div>
      </div>
    </Modal>
  );
}
