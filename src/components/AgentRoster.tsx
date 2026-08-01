import { Avatar, Button, Icon } from "../ui";
import type { Agent } from "../types";

/**
 * The agents of one room.
 *
 * Shared by room settings and project settings rather than duplicated: an agent
 * belongs to a room, so both places are looking at the same list, just from
 * different heights. The parent owns the dialogs, since project settings swaps
 * its whole body for them while room settings does not.
 */
export function AgentRoster({
  agents,
  compact,
  inRoom,
  onAdd,
  onEdit,
  onRotate,
  onDelete,
  onRemove,
}: {
  agents: Agent[];
  /** Tighter rows, for the per-room blocks inside project settings. */
  compact?: boolean;
  /** Viewing one room: offer "remove from room" as well as delete. */
  inRoom?: boolean;
  onAdd: () => void;
  onEdit: (a: Agent) => void;
  onRotate: (a: Agent) => void;
  onDelete: (a: Agent) => void;
  onRemove?: (a: Agent) => void;
}) {
  return (
    <div className="space-y-1">
      {agents.length === 0 && (
        <p className="px-1 py-1 text-[12px] text-faint">
          No agents in this room yet.
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
            <Button
              size="sm"
              variant="subtle"
              title="Issue a new key"
              onClick={() => onRotate(a)}
            >
              <Icon name="key" size={11} />
            </Button>
            {inRoom && onRemove && (
              <Button
                size="sm"
                variant="subtle"
                title="Remove from this room — the agent stays in the project"
                onClick={() => onRemove(a)}
              >
                <Icon name="x" size={11} />
              </Button>
            )}
            <Button
              size="sm"
              variant="subtle"
              title="Delete this agent from the project entirely"
              onClick={() => onDelete(a)}
            >
              <Icon name="trash" size={11} />
            </Button>
          </div>
        </div>
      ))}

      <Button size="sm" variant="subtle" onClick={onAdd}>
        <Icon name="plus" size={12} />
        {inRoom ? "Create a new agent" : "Add an agent"}
      </Button>
    </div>
  );
}
