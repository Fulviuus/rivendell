// The agent dialogs. Agents belong to a room, so they are reached through
// that room's project settings rather than a global list.
import { useState } from "react";
import { api, errText } from "../api";
import { useStore } from "../store";
import {
  Avatar,
  Button,
  ColorPicker,
  CopyButton,
  Field,
  Input,
  Modal,
  Select,
  Textarea,
} from "../ui";
import type { Agent, NewAgentKey } from "../types";

export function NewAgentModal({
  projectId,
  roomId,
  roomName,
  onClose,
  onCreated,
}: {
  projectId: number;
  /** Optional room to drop it into straight away. */
  roomId?: number;
  roomName?: string;
  onClose: () => void;
  onCreated: (key: NewAgentKey, name: string) => void;
}) {
  const { profiles, refreshAgents, notify } = useStore();
  const [name, setName] = useState("");
  const [profileId, setProfileId] = useState<string>("");
  const [note, setNote] = useState("");
  const [color, setColor] = useState("");
  const [busy, setBusy] = useState(false);

  const profile = profiles.find((p) => String(p.id) === profileId);

  async function submit() {
    if (!name.trim()) return;
    setBusy(true);
    try {
      const key = await api.createAgent({
        projectId,
        name: name.trim(),
        // Everyone in a council is the same kind of thing; the value is
        // stored only so the database's own constraint stays satisfied.
        role: "ASSISTANT",
        profileId: profileId ? Number(profileId) : null,
        systemNote: note,
        color,
      });
      if (roomId !== undefined) await api.joinRoom(roomId, key.agent_id);
      await refreshAgents();
      onCreated(key, name.trim());
    } catch (e) {
      notify("error", errText(e));
      setBusy(false);
    }
  }

  return (
    <Modal title="New agent" subtitle={roomName ? `for this project, joining #${roomName}` : "for this project"} onClose={onClose}>
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
        </div>

        <Field
          label="Kind"
          hint="which tool this is — sets its icon"
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

export function EditAgentModal({ agent, onClose }: { agent: Agent; onClose: () => void }) {
  const { profiles, refreshAgents, refreshThread, notify } = useStore();
  const [name, setName] = useState(agent.name);
  const [note, setNote] = useState(agent.system_note);
  const [color, setColor] = useState(agent.color);
  const [profileId, setProfileId] = useState(agent.profile_id ? String(agent.profile_id) : "");
  const [busy, setBusy] = useState(false);

  async function save() {
    setBusy(true);
    try {
      await api.updateAgent(agent.id, {
        name: name.trim(),
        system_note: note,
        color,
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

export function ConnectionModal({
  bundle,
  name,
  rotated,
  onClose,
}: {
  bundle: NewAgentKey;
  name: string;
  /** True when this replaced an existing key rather than being the first. */
  rotated?: boolean;
  onClose: () => void;
}) {
  const [tab, setTab] = useState<"cli" | "json" | "shim">("cli");
  const value = tab === "cli" ? bundle.claude_cli : tab === "json" ? bundle.mcp_json : bundle.shim_json;

  return (
    <Modal
      wide
      title={`${name}'s API key`}
      subtitle={
        rotated
          ? "A new key. The previous one stopped working just now."
          : "Copy it now — this is the only time it is shown."
      }
      onClose={onClose}
    >
      <div className="space-y-4">
        {rotated && (
          <p className="rounded-lg bg-amber-50 px-3 py-2 text-[12.5px] leading-relaxed text-amber-900 ring-1 ring-amber-200 dark:bg-amber-500/10 dark:text-amber-200 dark:ring-amber-500/25">
            Anything still running with {name}'s old key is now getting 401s. Update it with the
            key below, or restart it.
          </p>
        )}
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
