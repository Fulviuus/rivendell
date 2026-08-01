import { Avatar, Button, Icon } from "../ui";
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
}) {
  return (
    <div className="space-y-1">
      {agents.length === 0 && (
        <p className="px-1 py-1 text-[12px] text-faint">
          {mode === "room" ? "Nobody is in this room yet." : "No agents in this project yet."}
        </p>
      )}

      {agents.map((a) => (
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
              <span className="rounded bg-chip px-1.5 py-px text-[10.5px] tracking-wide text-muted uppercase">
                {a.role}
              </span>
              {a.profile_label && (
                <span className="text-[11.5px] text-faint">{a.profile_label}</span>
              )}
            </div>
            {a.key_preview && (
              <code className="font-mono text-[11px] text-faint">{a.key_preview}</code>
            )}
            {!compact && a.system_note && (
              <p className="mt-0.5 text-[12px] leading-snug text-muted">{a.system_note}</p>
            )}
          </div>

          <div className="flex shrink-0 gap-0.5">
            <Button size="sm" variant="subtle" title="Edit" onClick={() => onEdit(a)}>
              <Icon name="pencil" size={11} />
            </Button>
            <Button size="sm" variant="subtle" title="Issue a new key" onClick={() => onRotate(a)}>
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
      ))}

      {onAdd && (
        <Button size="sm" variant="subtle" onClick={onAdd}>
          <Icon name="plus" size={12} />
          {addLabel ?? "Add an agent"}
        </Button>
      )}
    </div>
  );
}
