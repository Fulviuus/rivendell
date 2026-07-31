import { useMemo } from "react";
import { useStore } from "../store";
import { ago, Empty, Icon, StatusChip, TagChip, tagStripe } from "../ui";
import { THREAD_SORTS, type ThreadSort } from "../types";

export function ThreadList({ onNew }: { onNew: () => void }) {
  const {
    threads,
    threadId,
    selectThread,
    tags,
    statusFilter,
    tagFilter,
    sortBy,
    setFilters,
    rooms,
    roomId,
  } = useStore();

  const tagByKey = useMemo(() => new Map(tags.map((t) => [t.key, t])), [tags]);
  const room = rooms.find((r) => r.id === roomId);

  return (
    <div className="flex w-[22rem] shrink-0 flex-col border-r border-line bg-list">
      <div
        data-tauri-drag-region
        className="titlebar-drag flex h-11 items-center gap-2 border-b border-line px-3"
      >
        <span data-tauri-drag-region className="flex-1 truncate font-semibold text-strong">
          {room ? `#${room.name}` : "No room"}
        </span>
        <button
          onClick={onNew}
          disabled={!room}
          title="New thread"
          className="rounded-lg bg-card p-1.5 text-soft shadow-card ring-1 ring-line ring-inset transition hover:bg-hover hover:text-strong disabled:opacity-40"
        >
          <Icon name="plus" size={14} />
        </button>
      </div>

      <div className="flex flex-wrap gap-1.5 border-b border-line px-3 py-2">
        <select
          value={statusFilter}
          onChange={(e) => setFilters({ status: e.target.value })}
          className="flex-1 rounded-md bg-card px-1.5 py-1 text-[12px] text-soft ring-1 ring-line ring-inset outline-none"
        >
          <option value="open">Open threads</option>
          <option value="all">All threads</option>
          <option value="resolved">Resolved threads</option>
          <option value="blocked">Blocked threads</option>
        </select>
        <select
          value={tagFilter}
          onChange={(e) => setFilters({ tag: e.target.value })}
          className="flex-1 rounded-md bg-card px-1.5 py-1 text-[12px] text-soft ring-1 ring-line ring-inset outline-none"
        >
          <option value="all">Any tag</option>
          {tags.map((t) => (
            <option key={t.key} value={t.key}>
              {t.label}
            </option>
          ))}
        </select>
        <select
          value={sortBy}
          onChange={(e) => setFilters({ sort: e.target.value as ThreadSort })}
          title="Sort threads"
          className="flex-1 rounded-md bg-card px-1.5 py-1 text-[12px] text-soft ring-1 ring-line ring-inset outline-none"
        >
          {THREAD_SORTS.map((s) => (
            <option key={s.key} value={s.key}>
              {s.label}
            </option>
          ))}
        </select>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto p-2">
        {threads.length === 0 ? (
          <Empty icon="spark" title="Nothing here">
            {room
              ? "Open a thread to put something in front of your assistants."
              : "Pick a room on the left."}
          </Empty>
        ) : (
          <div className="space-y-1.5">
            {threads.map((t) => {
              const active = t.id === threadId;
              const tag = tagByKey.get(t.tag);
              const waiting = t.status === "AWAITING_REPLIES";
              const done = t.status === "RESOLVED" || t.status === "WONTFIX";

              return (
                <button
                  key={t.id}
                  onClick={() => selectThread(t.id)}
                  className={`relative block w-full overflow-hidden rounded-xl pr-3 pl-3.5 py-2.5 text-left transition ${
                    active
                      ? "bg-card shadow-card ring-2 ring-accent/45"
                      : "bg-card shadow-card ring-1 ring-line hover:ring-accent/30"
                  } ${done ? "opacity-70" : ""}`}
                >
                  {/* Tag colour down the edge — the fastest way to tell an
                      adversarial review from a help request while scanning. */}
                  <span
                    className={`absolute inset-y-0 left-0 w-1 ${tagStripe(tag?.color ?? "slate")}`}
                  />

                  <div className="flex items-center gap-1.5">
                    {tag && <TagChip color={tag.color} label={tag.label} />}
                    <span
                      className="ml-auto shrink-0 text-[11px] text-faint"
                      title={
                        sortBy === "created"
                          ? `opened ${ago(t.created_at)}`
                          : t.last_reply_at
                            ? `last reply ${ago(t.last_reply_at)}`
                            : `opened ${ago(t.created_at)}`
                      }
                    >
                      {sortBy === "activity"
                        ? `${t.reply_count} ${t.reply_count === 1 ? "reply" : "replies"}`
                        : ago(
                            sortBy === "created"
                              ? t.created_at
                              : (t.last_reply_at ?? t.created_at),
                          )}
                    </span>
                  </div>

                  <p
                    className={`mt-1.5 line-clamp-2 leading-snug ${
                      active ? "font-medium text-strong" : "text-body"
                    }`}
                  >
                    {t.title}
                  </p>

                  <div className="mt-1.5 flex items-center gap-2 text-[11.5px] text-muted">
                    <StatusChip status={t.status} />
                    {t.quorum > 0 && !done && (
                      <span
                        className={waiting ? "pulse-soft" : ""}
                        title="Distinct assistants that have replied, against the quorum"
                      >
                        {t.responder_count}/{t.quorum} replied
                      </span>
                    )}
                    {t.cost_usd > 0 && (
                      <span className="ml-auto tabular-nums text-faint">
                        ${t.cost_usd.toFixed(2)}
                      </span>
                    )}
                  </div>
                </button>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}
