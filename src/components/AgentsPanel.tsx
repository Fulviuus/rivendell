import { useState } from "react";
import { api, errText } from "../api";
import { useStore } from "../store";
import {
  Avatar,
  Button,
  ColorPicker,
  CopyButton,
  Field,
  Icon,
  Input,
  Modal,
  Select,
  Textarea,
} from "../ui";
import type { Agent, NewAgentKey } from "../types";

export function AgentsPanel({ onClose }: { onClose: () => void }) {
  const { agents, rooms, roomId, refreshAgents, notify } = useStore();
  const room = rooms.find((r) => r.id === roomId);
  const [creating, setCreating] = useState(false);
  const [editing, setEditing] = useState<Agent | null>(null);
  const [revealed, setRevealed] = useState<{ key: NewAgentKey; name: string } | null>(null);

  async function rotate(a: Agent) {
    try {
      const key = await api.rotateAgentKey(a.id);
      await refreshAgents();
      setRevealed({ key, name: a.name });
    } catch (e) {
      notify("error", errText(e));
    }
  }

  async function remove(a: Agent) {
    try {
      await api.deleteAgent(a.id);
      await refreshAgents();
    } catch (e) {
      notify("error", errText(e));
    }
  }

  if (revealed) {
    return (
      <ConnectionModal
        bundle={revealed.key}
        name={revealed.name}
        onClose={() => setRevealed(null)}
      />
    );
  }

  if (creating) {
    return (
      <NewAgentModal
        onClose={() => setCreating(false)}
        onCreated={(key, name) => {
          setCreating(false);
          setRevealed({ key, name });
        }}
      />
    );
  }

  if (editing) {
    return <EditAgentModal agent={editing} onClose={() => setEditing(null)} />;
  }

  return (
    <Modal
      wide
      title="Agents"
      subtitle={room ? `#${room.name} · ${room.project_name}` : "No room selected"}
      onClose={onClose}
    >
      <div className="space-y-2">
        {agents.length === 0 && (
          <p className="py-4 text-center text-[12.5px] text-faint">
            No agents yet. Add a coder for your own session, then the assistants you want it to
            consult.
          </p>
        )}

        {agents.map((a) => (
          <div
            key={a.id}
            className="flex items-center gap-3 rounded-xl bg-card px-3 py-2.5 shadow-card ring-1 ring-line"
          >
            <Avatar name={a.name} icon={a.icon} color={a.color} size={28} />
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-2">
                <span
                  className={`font-semibold ${a.revoked_at ? "text-faint line-through" : "text-strong"}`}
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
              <div className="mt-0.5 flex items-center gap-2 text-[11.5px] text-faint">
                {a.key_preview && <code className="font-mono">{a.key_preview}</code>}
                {a.role === "ASSISTANT" && (
                  <span>{a.auto_dispatch ? "auto-dispatch on" : "manual only"}</span>
                )}
              </div>
              {a.system_note && (
                <p className="mt-1 text-[12px] leading-snug text-muted">{a.system_note}</p>
              )}
            </div>

            <div className="flex shrink-0 gap-1">
              {a.role === "ASSISTANT" && (
                <Button
                  size="sm"
                  variant="subtle"
                  title={a.auto_dispatch ? "Disable auto-dispatch" : "Enable auto-dispatch"}
                  onClick={async () => {
                    await api.setAgentAutoDispatch(a.id, !a.auto_dispatch);
                    await refreshAgents();
                  }}
                >
                  <Icon name={a.auto_dispatch ? "pause" : "play"} size={12} />
                </Button>
              )}
              <Button size="sm" variant="subtle" title="Edit agent" onClick={() => setEditing(a)}>
                <Icon name="gear" size={12} />
              </Button>
              <Button size="sm" variant="subtle" title="Issue a new key" onClick={() => rotate(a)}>
                <Icon name="key" size={12} />
              </Button>
              <Button size="sm" variant="subtle" title="Delete agent" onClick={() => remove(a)}>
                <Icon name="trash" size={12} />
              </Button>
            </div>
          </div>
        ))}

        <div className="flex justify-between pt-2">
          <p className="max-w-md text-[11.5px] leading-relaxed text-faint">
            Assistants Rivendell launches itself get a one-time token per run, so their long-lived
            key stays unused. Use the key below for a session you attach yourself — typically your
            coder.
          </p>
          <Button variant="primary" onClick={() => setCreating(true)} disabled={!room}>
            <Icon name="plus" size={13} />
            New agent
          </Button>
        </div>
      </div>
    </Modal>
  );
}

function NewAgentModal({
  onClose,
  onCreated,
}: {
  onClose: () => void;
  onCreated: (key: NewAgentKey, name: string) => void;
}) {
  const { roomId, profiles, refreshAgents, notify } = useStore();
  const [name, setName] = useState("");
  const [role, setRole] = useState<"CODER" | "ASSISTANT">("ASSISTANT");
  const [profileId, setProfileId] = useState<string>("");
  const [note, setNote] = useState("");
  const [color, setColor] = useState("");
  const [autoDispatch, setAutoDispatch] = useState(true);
  const [busy, setBusy] = useState(false);

  const profile = profiles.find((p) => String(p.id) === profileId);
  const external = profile?.key === "external";

  async function submit() {
    if (roomId === null || !name.trim()) return;
    setBusy(true);
    try {
      const key = await api.createAgent({
        roomId,
        name: name.trim(),
        role,
        profileId: profileId ? Number(profileId) : null,
        systemNote: note,
        color,
        autoDispatch: role === "ASSISTANT" && autoDispatch && !external,
      });
      await refreshAgents();
      onCreated(key, name.trim());
    } catch (e) {
      notify("error", errText(e));
      setBusy(false);
    }
  }

  return (
    <Modal title="New agent" onClose={onClose}>
      <div className="space-y-3.5">
        <div className="grid grid-cols-2 gap-3">
          <Field label="Name">
            <Input
              autoFocus
              value={name}
              placeholder="skeptic"
              onChange={(e) => setName(e.target.value)}
            />
          </Field>
          <Field label="Role">
            <Select value={role} onChange={(e) => setRole(e.target.value as "CODER" | "ASSISTANT")}>
              <option value="ASSISTANT">Assistant — replies to threads</option>
              <option value="CODER">Coder — opens and resolves threads</option>
            </Select>
          </Field>
        </div>

        <Field
          label="Kind"
          hint={role === "CODER" ? "a coder is usually your own attached session" : ""}
        >
          <Select value={profileId} onChange={(e) => setProfileId(e.target.value)}>
            <option value="">Choose…</option>
            {profiles.map((p) => (
              <option key={p.id} value={p.id}>
                {p.label}
              </option>
            ))}
          </Select>
        </Field>

        {profile && (
          <div className="rounded-lg bg-code p-2.5 text-[12px] leading-relaxed text-muted ring-1 ring-line">
            {profile.notes}
            {profile.launch_cmd && (
              <div className="mt-1.5 font-mono text-[11px] text-faint">
                launches: {profile.launch_cmd} {JSON.parse(profile.launch_args).join(" ")}
              </div>
            )}
          </div>
        )}

        <Field label="Colour" hint="its avatar and everything it says">
          <div className="flex items-center gap-3">
            <ColorPicker value={color} onChange={setColor} />
            <Avatar name={name || "??"} color={color} size={28} />
          </div>
        </Field>

        <Field label="Note" hint="optional — shown to other agents in the room">
          <Textarea
            rows={2}
            value={note}
            placeholder="Focus on concurrency and error paths."
            onChange={(e) => setNote(e.target.value)}
          />
        </Field>

        {role === "ASSISTANT" && !external && (
          <label className="flex items-center gap-2 text-[12.5px] text-soft">
            <input
              type="checkbox"
              checked={autoDispatch}
              onChange={(e) => setAutoDispatch(e.target.checked)}
              className="accent-accent"
            />
            Launch automatically when a thread needs replies
          </label>
        )}

        <div className="flex justify-end gap-2 pt-1">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} disabled={busy || !name.trim()}>
            Create & generate key
          </Button>
        </div>
      </div>
    </Modal>
  );
}

function EditAgentModal({ agent, onClose }: { agent: Agent; onClose: () => void }) {
  const { profiles, refreshAgents, refreshThread, notify } = useStore();
  const [name, setName] = useState(agent.name);
  const [note, setNote] = useState(agent.system_note);
  const [color, setColor] = useState(agent.color);
  const [profileId, setProfileId] = useState(agent.profile_id ? String(agent.profile_id) : "");
  const [autoDispatch, setAutoDispatch] = useState(agent.auto_dispatch);
  const [busy, setBusy] = useState(false);

  async function save() {
    setBusy(true);
    try {
      await api.updateAgent(agent.id, {
        name: name.trim(),
        system_note: note,
        color,
        auto_dispatch: autoDispatch,
        profile_id: profileId ? Number(profileId) : null,
      });
      await refreshAgents();
      // Messages carry the agent's colour, so an open thread has to re-read.
      await refreshThread();
      onClose();
    } catch (e) {
      notify("error", errText(e));
      setBusy(false);
    }
  }

  return (
    <Modal title={`Edit ${agent.name}`} subtitle={agent.role} onClose={onClose}>
      <div className="space-y-3.5">
        <Field label="Name">
          <Input autoFocus value={name} onChange={(e) => setName(e.target.value)} />
        </Field>

        <Field label="Colour" hint="its avatar and everything it says">
          <div className="flex items-center gap-3">
            <ColorPicker value={color} onChange={setColor} />
            <Avatar name={name || agent.name} icon={agent.icon} color={color} size={28} />
          </div>
        </Field>

        <Field label="Kind">
          <Select value={profileId} onChange={(e) => setProfileId(e.target.value)}>
            <option value="">None</option>
            {profiles.map((p) => (
              <option key={p.id} value={p.id}>
                {p.label}
              </option>
            ))}
          </Select>
        </Field>

        <Field label="Note" hint="shown to other agents in the room">
          <Textarea rows={2} value={note} onChange={(e) => setNote(e.target.value)} />
        </Field>

        {agent.role === "ASSISTANT" && (
          <label className="flex items-center gap-2 text-[12.5px] text-soft">
            <input
              type="checkbox"
              checked={autoDispatch}
              onChange={(e) => setAutoDispatch(e.target.checked)}
              className="accent-accent"
            />
            Launch automatically when a thread needs replies
          </label>
        )}

        <div className="flex justify-end gap-2 pt-1">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={save} disabled={busy || !name.trim()}>
            Save
          </Button>
        </div>
      </div>
    </Modal>
  );
}

function ConnectionModal({
  bundle,
  name,
  onClose,
}: {
  bundle: NewAgentKey;
  name: string;
  onClose: () => void;
}) {
  const [tab, setTab] = useState<"cli" | "json" | "shim">("cli");
  const value = tab === "cli" ? bundle.claude_cli : tab === "json" ? bundle.mcp_json : bundle.shim_json;

  return (
    <Modal
      wide
      title={`${name}'s API key`}
      subtitle="Shown once. Only a hash is stored — if you lose it, issue a new one."
      onClose={onClose}
    >
      <div className="space-y-4">
        <div>
          <div className="mb-1 flex items-center justify-between">
            <span className="text-[11.5px] font-medium tracking-wide text-soft uppercase">
              Key
            </span>
            <CopyButton text={bundle.api_key} />
          </div>
          <code className="block overflow-x-auto rounded-lg bg-code px-3 py-2 font-mono text-[12px] font-medium break-all text-accent-text ring-1 ring-line">
            {bundle.api_key}
          </code>
        </div>

        <div>
          <div className="mb-2 flex items-center justify-between">
            <div className="flex gap-1">
              {(
                [
                  ["cli", "Claude Code"],
                  ["json", "MCP config"],
                  ["shim", "stdio shim"],
                ] as const
              ).map(([k, label]) => (
                <button
                  key={k}
                  onClick={() => setTab(k)}
                  className={`rounded-lg px-2.5 py-1 text-[12px] transition ${
                    tab === k ? "bg-accent-soft text-accent-text" : "text-muted hover:text-strong"
                  }`}
                >
                  {label}
                </button>
              ))}
            </div>
            <CopyButton text={value} />
          </div>
          <pre className="max-h-64 overflow-auto rounded-lg bg-code px-3 py-2.5 font-mono text-[11.5px] leading-relaxed whitespace-pre-wrap text-soft ring-1 ring-line">
            {value}
          </pre>
          <p className="mt-2 text-[11.5px] leading-relaxed text-faint">
            {tab === "cli" &&
              "Run this in the project folder. Your session then sees the rivendell tools and can open threads."}
            {tab === "json" &&
              "Drop into any client that speaks streamable-HTTP MCP with custom headers."}
            {tab === "shim" &&
              "For clients that only support stdio. Build the shim with `cargo build --release -p rivendell-mcp` and put it on PATH."}
          </p>
        </div>

        <div className="flex justify-end">
          <Button variant="primary" onClick={onClose}>
            I've saved it
          </Button>
        </div>
      </div>
    </Modal>
  );
}
