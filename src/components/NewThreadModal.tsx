import { useEffect, useMemo, useState } from "react";
import { api, errText } from "../api";
import { useStore } from "../store";
import { Button, Field, Icon, Input, Modal } from "../ui";
import { MentionTextarea } from "./MentionTextarea";
import type { ContextInput } from "../types";

interface Attachment {
  path: string;
  start: string;
  end: string;
}

export function NewThreadModal({ onClose }: { onClose: () => void }) {
  const { roomId, rooms, tags, agents, notify, selectThread, refreshThreads } = useStore();
  const room = rooms.find((r) => r.id === roomId);

  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [tagKey, setTagKey] = useState("");
  const [mentions, setMentions] = useState<number[]>([]);
  const [includeDiff, setIncludeDiff] = useState(true);
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [files, setFiles] = useState<string[]>([]);
  const [git, setGit] = useState<{ is_repo: boolean; branch: string | null; dirty: boolean } | null>(
    null,
  );
  const [busy, setBusy] = useState(false);

  const tag = useMemo(() => tags.find((t) => t.key === tagKey), [tags, tagKey]);
  const assistants = agents.filter((a) => a.role === "ASSISTANT" && !a.revoked_at);
  // Who @-completion can actually offer: anyone here but you.
  const mentionable = agents.filter((a) => a.role !== "HUMAN" && !a.revoked_at);

  useEffect(() => {
    if (!tagKey && tags.length) setTagKey(tags[0].key);
  }, [tags, tagKey]);

  useEffect(() => {
    if (roomId === null) return;
    api.listProjectFiles(roomId).then(setFiles).catch(() => setFiles([]));
    api
      .gitStatus(roomId)
      .then((g) => {
        setGit(g);
        setIncludeDiff(g.is_repo && g.dirty);
      })
      .catch(() => setGit(null));
  }, [roomId]);

  async function submit() {
    if (roomId === null || !title.trim() || !tagKey) return;
    setBusy(true);
    try {
      const context: ContextInput[] = attachments
        .filter((a) => a.path.trim())
        .map((a) => ({
          kind: "file",
          path: a.path.trim(),
          start_line: a.start ? Number(a.start) : null,
          end_line: a.end ? Number(a.end) : null,
        }));

      const id = await api.createThread({
        room_id: roomId,
        title,
        body,
        tag: tagKey,
        mentions,
        context,
        include_diff: includeDiff,
      });
      await refreshThreads();
      await selectThread(id);
      onClose();
    } catch (e) {
      notify("error", errText(e));
      setBusy(false);
    }
  }

  return (
    <Modal
      wide
      title="New thread"
      subtitle={room ? `#${room.name} · ${room.project_name}` : undefined}
      onClose={onClose}
    >
      <div className="space-y-3.5">
        <Field label="Title">
          <Input
            autoFocus
            value={title}
            placeholder="Token refresh races when two requests 401 at once"
            onChange={(e) => setTitle(e.target.value)}
          />
        </Field>

        <Field label="Tag" hint="decides who is pulled in and what they are told to do">
          <div className="flex flex-wrap gap-1.5">
            {tags.map((t) => (
              <button
                key={t.key}
                onClick={() => setTagKey(t.key)}
                className={`rounded-lg px-2.5 py-1 text-[12px] ring-1 ring-inset transition ${
                  tagKey === t.key
                    ? "bg-accent-soft font-medium text-accent-text ring-accent/40"
                    : "bg-field text-muted ring-line hover:text-strong"
                }`}
              >
                {t.label}
              </button>
            ))}
          </div>
        </Field>

        {tag && (
          <div className="rounded-lg bg-code p-2.5 text-[12px] leading-relaxed text-muted ring-1 ring-line">
            <span className="text-soft">Assistants will be told:</span> {tag.instruction}
            {tag.verdict_options.length > 0 && (
              <div className="mt-1.5 text-faint">
                Replies must carry a verdict: {tag.verdict_options.join(" · ")}
              </div>
            )}
          </div>
        )}

        <Field
          label="The ask"
          hint={mentionable.length ? "markdown · type @ to call an agent in" : "markdown"}
        >
          <MentionTextarea
            rows={7}
            agents={agents}
            value={body}
            placeholder={
              "What you tried, what you expected, what happened instead.\n\nBe specific — vague asks get vague reviews."
            }
            onChange={setBody}
          />
        </Field>

        <Field
          label="Code to review"
          hint="copied now, so it still matches the question after you keep working"
        >
          <div className="space-y-2">
            {git?.is_repo && (
              <label className="flex items-center gap-2 text-[12.5px] text-soft">
                <input
                  type="checkbox"
                  checked={includeDiff}
                  onChange={(e) => setIncludeDiff(e.target.checked)}
                  className="accent-accent"
                />
                Attach the working-tree diff
                <span className="text-faint">
                  {git.branch ? `(${git.branch}${git.dirty ? ", dirty" : ", clean"})` : ""}
                </span>
              </label>
            )}

            {attachments.length > 0 && (
              <div className="flex gap-1.5 px-0.5 text-[11px] text-faint">
                <span className="flex-1">file — start typing to search the project</span>
                <span className="w-20">first line</span>
                <span className="w-20">last line</span>
                <span className="w-7" />
              </div>
            )}
            {attachments.map((a, i) => (
              <div key={i} className="flex gap-1.5">
                <Input
                  list="project-files"
                  value={a.path}
                  autoFocus={!a.path}
                  placeholder="src/auth/token.ts"
                  className="min-w-0 flex-1 font-mono !text-[12px]"
                  onChange={(e) =>
                    setAttachments(
                      attachments.map((x, j) => (j === i ? { ...x, path: e.target.value } : x)),
                    )
                  }
                />
                <Input
                  value={a.start}
                  placeholder="whole file"
                  className="w-20 !text-[12px]"
                  onChange={(e) =>
                    setAttachments(
                      attachments.map((x, j) => (j === i ? { ...x, start: e.target.value } : x)),
                    )
                  }
                />
                <Input
                  value={a.end}
                  placeholder="whole file"
                  className="w-20 !text-[12px]"
                  onChange={(e) =>
                    setAttachments(
                      attachments.map((x, j) => (j === i ? { ...x, end: e.target.value } : x)),
                    )
                  }
                />
                <Button
                  size="sm"
                  variant="subtle"
                  onClick={() => setAttachments(attachments.filter((_, j) => j !== i))}
                >
                  <Icon name="x" size={13} />
                </Button>
              </div>
            ))}
            <datalist id="project-files">
              {files.filter((f) => !f.endsWith("/")).slice(0, 800).map((f) => (
                <option key={f} value={f} />
              ))}
            </datalist>

            <Button
              size="sm"
              variant="subtle"
              onClick={() => setAttachments([...attachments, { path: "", start: "", end: "" }])}
            >
              <Icon name="plus" size={12} />
              Attach a file excerpt
            </Button>
          </div>
        </Field>

        <div className="grid grid-cols-1 gap-3">
          <Field
            label="Address to"
            hint={
              assistants.length === 0
                ? "no assistants here yet — add one in project settings"
                : mentions.length
                  ? "only these agents will see it"
                  : "left empty, every assistant in the room sees it"
            }
          >
            <div className="max-h-28 space-y-1 overflow-y-auto rounded-lg bg-field p-2 ring-1 ring-inset ring-line">
              {assistants.length === 0 && (
                <p className="px-1 py-0.5 text-[12px] text-faint">No assistants in this room.</p>
              )}
              {assistants.map((a) => (
                <label key={a.id} className="flex items-center gap-2 text-[12.5px] text-soft">
                  <input
                    type="checkbox"
                    checked={mentions.includes(a.id)}
                    className="accent-accent"
                    onChange={(e) =>
                      setMentions(
                        e.target.checked
                          ? [...mentions, a.id]
                          : mentions.filter((m) => m !== a.id),
                      )
                    }
                  />
                  <Icon name={a.icon} size={13} className="text-faint" />
                  {a.name}
                </label>
              ))}
            </div>
          </Field>

        </div>

        <div className="flex justify-end gap-2 pt-1">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} disabled={busy || !title.trim() || !tagKey}>
            Open thread & dispatch
          </Button>
        </div>
      </div>
    </Modal>
  );
}
