import { useState } from "react";
import { api, errText } from "../api";
import { useStore } from "../store";
import { Avatar, Button, Icon, Modal, Toggle } from "../ui";
import type { Agent } from "../types";

/**
 * The agents in a list, with the actions that make sense where it is shown.
 *
 * Rooms manage membership; the project manages existence. Keeping those apart
 * matters: a delete button inside a room reads as "take it out of this room"
 * and would in fact destroy the agent everywhere, along with its key.
 */
export function AgentRoster({
  agents,
  mode,
  compact,
  onAdd,
  addLabel,
  onEdit,
  onRotate,
  onRemove,
  onDelete,
  onChanged,
}: {
  agents: Agent[];
  /** "room" offers remove; "project" offers delete. Never both. */
  mode: "room" | "project";
  compact?: boolean;
  onAdd?: () => void;
  addLabel?: string;
  onEdit: (a: Agent) => void;
  onRotate: (a: Agent) => void;
  onRemove?: (a: Agent) => void;
  onDelete?: (a: Agent) => void;
  /** Called after the awake switch changes, so the caller can reload its list. */
  onChanged?: () => void | Promise<void>;
}) {
  const { awake, notify, refreshAwake, profiles } = useStore();
  const [confirming, setConfirming] = useState<Agent | null>(null);

  async function setAwake(a: Agent, on: boolean) {
    try {
      await api.setAgentAwake(a.id, on);
      await refreshAwake();
      await onChanged?.();
    } catch (e) {
      notify("error", errText(e));
    }
  }

  return (
    <div className="space-y-1">
      {agents.length === 0 && (
        <p className="px-1 py-1 text-[12px] text-faint">
          {mode === "room" ? "Nobody is in this room yet." : "No agents in this project yet."}
        </p>
      )}

      {agents.map((a) => {
        // Rivendell can only start an agent whose kind carries a command —
        // several kinds are identity only: a label and an icon for something
        // you run yourself.
        const startable = !!profiles.find((p) => p.id === a.profile_id)?.launch_cmd;
        const live = awake[a.id];
        return (
          <div
            key={a.id}
            className={`flex items-center gap-2 rounded-lg transition hover:bg-hover ${
              compact ? "px-1.5 py-1" : "bg-card px-2.5 py-2 shadow-card ring-1 ring-line"
            }`}
          >
            <Avatar name={a.name} icon={a.icon} color={a.color} size={compact ? 22 : 26} />
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2">
                <span
                  className={`font-medium ${a.revoked_at ? "text-faint line-through" : "text-strong"}`}
                >
                  {a.name}
                </span>
                {/* Everyone in a council is the same kind of thing, so there
                    is nothing to label — except which one is you. */}
                {a.role === "HUMAN" && (
                  <span className="rounded bg-chip px-1.5 py-px text-[10.5px] tracking-wide text-muted uppercase">
                    you
                  </span>
                )}
                {a.profile_label && (
                  <span className="text-[11.5px] text-faint">{a.profile_label}</span>
                )}
                {a.awake && <AwakeDot agent={a} />}
              </div>
              {a.key_preview && (
                <code className="font-mono text-[11px] text-faint">{a.key_preview}</code>
              )}
              {!compact && a.system_note && (
                <p className="mt-0.5 text-[12px] leading-snug text-muted">{a.system_note}</p>
              )}
              {!compact && a.awake && live?.trouble && (
                <p className="mt-0.5 text-[12px] leading-snug text-rose-600 dark:text-rose-400">
                  {live.trouble}
                </p>
              )}
            </div>

            <div className="flex shrink-0 items-center gap-1.5">
              <Toggle
                on={a.awake}
                disabled={!startable || !!a.revoked_at}
                label={`Keep ${a.name} awake`}
                title={
                  a.revoked_at
                    ? "This agent's key has been revoked."
                    : !startable
                      ? `Rivendell has no command for ${a.name}. Give it a kind that carries a launch command, or run it yourself.`
                      : a.awake
                        ? `Rivendell starts ${a.name} when its rooms have work. Switch off to stop.`
                        : `Have Rivendell start ${a.name} when its rooms have work.`
                }
                onChange={(on) => (on ? setConfirming(a) : setAwake(a, false))}
              />

              <div className="flex gap-0.5">
                <Button size="sm" variant="subtle" title="Edit" onClick={() => onEdit(a)}>
                  <Icon name="pencil" size={11} />
                </Button>
                <Button
                  size="sm"
                  variant="subtle"
                  title="Issue a new key"
                  onClick={() => onRotate(a)}
                >
                  <Icon name="key" size={11} />
                </Button>

                {mode === "room" && onRemove && (
                  <Button
                    size="sm"
                    variant="subtle"
                    title="Take out of this room — the agent and its key stay in the project"
                    onClick={() => onRemove(a)}
                  >
                    <Icon name="x" size={11} />
                  </Button>
                )}

                {/* Only where it cannot be mistaken for "remove from this room". */}
                {mode === "project" && onDelete && (
                  <Button
                    size="sm"
                    variant="subtle"
                    title="Delete from the project — removes it from every room and kills its key"
                    onClick={() => onDelete(a)}
                  >
                    <Icon name="trash" size={11} />
                  </Button>
                )}
              </div>
            </div>
          </div>
        );
      })}

      {onAdd && (
        <Button size="sm" variant="subtle" onClick={onAdd}>
          <Icon name="plus" size={12} />
          {addLabel ?? "Add an agent"}
        </Button>
      )}

      {confirming && (
        <ConfirmAwake
          agent={confirming}
          onClose={() => setConfirming(null)}
          onConfirm={async () => {
            const a = confirming;
            setConfirming(null);
            await setAwake(a, true);
          }}
        />
      )}
    </div>
  );
}

/** Running now, watching, or something is wrong. */
function AwakeDot({ agent }: { agent: Agent }) {
  const live = useStore((s) => s.awake[agent.id]);
  const running = live?.running ?? false;
  const watching = live?.watching ?? false;
  const trouble = live?.trouble;

  // Awake in the database but with no watcher up means it is on its way, or it
  // failed — and the two look identical for about a second, so neither claims
  // more than it knows.
  const tone = trouble
    ? "bg-rose-500"
    : running
      ? "bg-emerald-500"
      : watching
        ? "bg-emerald-500/50"
        : "bg-amber-500/60";
  const word = trouble ? "stopped" : running ? "running" : watching ? "awake" : "starting";
  const said = trouble
    ? trouble
    : running
      ? live?.threads?.length
        ? `Running now, on ${live.threads.map((t) => `#${t}`).join(", ")}.`
        : "Running now."
      : watching
        ? "Awake, waiting for something to happen."
        : "Starting its watcher.";

  return (
    <span className="flex items-center gap-1 text-[11px] text-muted" title={said}>
      <span className={`h-1.5 w-1.5 rounded-full ${tone} ${running ? "pulse-soft" : ""}`} />
      {word}
    </span>
  );
}

/**
 * Turning this on spends money while nobody is watching. That is a reasonable
 * thing to want and it should not be a surprise, so it gets said out loud once.
 */
function ConfirmAwake({
  agent,
  onClose,
  onConfirm,
}: {
  agent: Agent;
  onClose: () => void;
  onConfirm: () => void;
}) {
  return (
    <Modal title={`Keep ${agent.name} awake?`} subtitle={agent.profile_label ?? ""} onClose={onClose}>
      <div className="space-y-3 text-[13px] leading-relaxed text-body">
        <p>
          Rivendell will start {agent.name} whenever a thread that asked for it moves —
          including while you are away from the machine. Each start is a real session and costs
          real money.
        </p>
        <p className="text-muted">
          It only starts one at a time, only for threads that asked for it, and it stops after{" "}
          <span className="text-body">40 starts in an hour</span> in case something loops.
        </p>
        <div className="flex justify-end gap-2 pt-1">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={onConfirm}>
            Keep it awake
          </Button>
        </div>
      </div>
    </Modal>
  );
}
