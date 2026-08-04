import { useEffect, useState } from "react";
import { useStore } from "../store";
import { ago, Avatar, Icon, swatchFor, type AgentColor } from "../ui";
import type { ConnectedAgent } from "../types";

/**
 * Who is on the wire: every agent holding a connection to the listener, and
 * anyone heard from moments ago. Clicking a row opens what that agent is
 * listening to — its project, the folder, and the rooms it hears.
 *
 * The truth here is deliberately layered. A live hold (the long poll, the
 * wake socket, the notification stream) is *connected*; an agent that let go
 * seconds ago is almost always between polls, reacting to what came back —
 * dropping it from the list at exactly the moment it is working would be the
 * most misleading thing this section could do. So the dot distinguishes the
 * two, and the expanded row says precisely which hold it has.
 */
export function ConnectedAgents() {
  const connections = useStore((s) => s.connections);
  const [openId, setOpenId] = useState<number | null>(null);
  const [, setTick] = useState(0);

  // Relative labels and the online/away split move with the clock, not with
  // events. A display tick keeps them honest, and — only while a row has no
  // live hold, since those are the only rows that can decay off the list by
  // pure time — refetches so the backend can prune. When everyone is either
  // holding a connection or gone, this does nothing.
  useEffect(() => {
    if (connections.length === 0) return;
    const id = setInterval(() => {
      setTick((n) => n + 1);
      const rows = useStore.getState().connections;
      if (rows.some((c) => c.connections.length === 0)) {
        void useStore.getState().refreshConnections();
      }
    }, 30_000);
    return () => clearInterval(id);
  }, [connections.length === 0]);

  const online = connections.filter(isOnline).length;

  return (
    <div className="border-t border-line px-2 pt-2 pb-1">
      <div
        className="flex items-center gap-1.5 px-2 pb-1"
        title="Agents holding a connection to the listener — the long poll, the wake socket or the notification stream — or heard from in the last few minutes."
      >
        <span className="text-[10.5px] font-semibold tracking-wide text-faint uppercase">
          Connected
        </span>
        <span className="rounded-full bg-chip px-1.5 text-[10.5px] text-muted tabular-nums">
          {online}
        </span>
      </div>

      {connections.length === 0 && (
        <p className="px-2 pb-1 text-[11.5px] leading-relaxed text-faint">
          Nobody is on the wire.
        </p>
      )}

      <div className="max-h-44 space-y-px overflow-y-auto">
        {connections.map((a) => (
          <Row
            key={a.agent_id}
            agent={a}
            open={openId === a.agent_id}
            onToggle={() => setOpenId(openId === a.agent_id ? null : a.agent_id)}
          />
        ))}
      </div>
    </div>
  );
}

const KIND_LABEL: Record<string, string> = {
  poll: "holding the long poll",
  socket: "on the wake socket",
  stream: "on the notification stream",
};

/** Seconds since an ISO stamp. Unparseable reads as stale, never as fresh. */
function secondsSince(iso: string): number {
  const t = new Date(iso).getTime();
  return Number.isNaN(t) ? Number.POSITIVE_INFINITY : Math.max(0, (Date.now() - t) / 1000);
}

/** A live hold, or let go so recently it is just the poll loop breathing. */
function isOnline(a: ConnectedAgent): boolean {
  return a.connections.length > 0 || secondsSince(a.last_seen) < 90;
}

function Row({
  agent: a,
  open,
  onToggle,
}: {
  agent: ConnectedAgent;
  open: boolean;
  onToggle: () => void;
}) {
  const listening = a.connections.length > 0;
  const word = listening
    ? "listening"
    : isOnline(a)
      ? "here, between polls"
      : `seen ${ago(a.last_seen)}`;

  return (
    <div>
      <button
        onClick={onToggle}
        aria-expanded={open}
        title={`${a.name} — ${word}. Click for what it is listening to.`}
        className={`flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-left transition hover:bg-hover ${
          open ? "bg-hover" : ""
        }`}
      >
        <Avatar name={a.name} icon={a.icon} color={a.color} size={20} />
        <span
          className={`flex-1 truncate text-[12.5px] ${isOnline(a) ? "text-soft" : "text-faint"}`}
        >
          {a.name}
        </span>
        <span
          className={`h-1.5 w-1.5 shrink-0 rounded-full ${
            listening
              ? "bg-emerald-500"
              : isOnline(a)
                ? "bg-emerald-500/50"
                : "bg-faint"
          }`}
        />
      </button>

      {open && (
        <div className="mx-1 mt-0.5 mb-1.5 space-y-1.5 rounded-lg bg-card p-2.5 text-[11.5px] shadow-card ring-1 ring-line">
          <div className="flex items-center gap-1.5" title={a.folder_path}>
            {a.project_color ? (
              <span
                className={`h-2 w-2 shrink-0 rounded ${swatchFor(a.project_color as AgentColor)}`}
              />
            ) : (
              <Icon name="folder" size={12} className="text-faint" />
            )}
            <span className="truncate font-medium text-strong">{a.project_name}</span>
          </div>
          <p className="truncate font-mono text-[10.5px] text-faint" title={a.folder_path}>
            {a.folder_path}
          </p>
          {a.rooms.length > 0 ? (
            <p className="leading-snug text-muted">
              listening to {a.rooms.map((r) => `#${r}`).join(", ")}
            </p>
          ) : (
            <p className="leading-snug text-muted">in no rooms yet — it hears nothing</p>
          )}
          <div className="space-y-0.5 border-t border-line pt-1.5 text-muted">
            {listening ? (
              a.connections.map((c, i) => (
                <p key={i} className="leading-snug">
                  {KIND_LABEL[c.kind] ?? c.kind} · {ago(c.since)}
                </p>
              ))
            ) : (
              <p className="leading-snug">last heard from {ago(a.last_seen)}</p>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
