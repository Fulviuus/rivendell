import { useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, errText } from "../api";
import { useStore } from "../store";
import {
  Button,
  ColorPicker,
  Field,
  Icon,
  Input,
  Modal,
  swatchFor,
  type AgentColor,
} from "../ui";
import { AgentRoster } from "./AgentRoster";
import { ConnectionModal, EditAgentModal, NewAgentModal } from "./AgentModals";
import type { Agent, NewAgentKey, Project } from "../types";

interface Stats {
  rooms: number;
  threads: number;
  messages: number;
  agents: number;
  exported_records: number;
}

export function ProjectSettings({
  project,
  onClose,
}: {
  project: Project;
  onClose: () => void;
}) {
  const { rooms, refreshRooms, refreshAgents, selectRoom, notify } = useStore();

  const [name, setName] = useState(project.name);
  const [folder, setFolder] = useState(project.folder_path);
  const [color, setColor] = useState(project.color);
  const [busy, setBusy] = useState(false);

  // Agents belong to rooms, not projects, so this view lists them per room
  // rather than flattening them — the flat "Agents" button was exactly the
  // ambiguity that made the scoping unclear.
  const [agents, setAgents] = useState<Agent[]>([]);
  const [adding, setAdding] = useState<{ id: number; name: string } | null>(null);
  const [editing, setEditing] = useState<Agent | null>(null);
  const [revealed, setRevealed] = useState<{
    key: NewAgentKey;
    name: string;
    rotated: boolean;
  } | null>(null);
  // Rotating is destructive — it breaks whatever is using the current key —
  // so it asks first rather than firing on a single click of a small icon.
  const [rotating, setRotating] = useState<Agent | null>(null);

  const [stats, setStats] = useState<Stats | null>(null);
  const [confirmText, setConfirmText] = useState("");
  const [confirming, setConfirming] = useState(false);

  const projectRooms = useMemo(
    () => rooms.filter((r) => r.project_id === project.id),
    [rooms, project.id],
  );

  const loadAgents = async () => {
    try {
      const all = await api.listAgents();
      const ids = new Set(projectRooms.map((r) => r.id));
      setAgents(all.filter((a) => ids.has(a.room_id)));
    } catch (e) {
      notify("error", errText(e));
    }
  };

  useEffect(() => {
    loadAgents();
    api.projectStats(project.id).then(setStats).catch(() => setStats(null));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [project.id, rooms.length]);

  if (revealed) {
    return (
      <ConnectionModal
        bundle={revealed.key}
        name={revealed.name}
        rotated={revealed.rotated}
        onClose={() => setRevealed(null)}
      />
    );
  }
  if (adding) {
    return (
      <NewAgentModal
        roomId={adding.id}
        roomName={adding.name}
        onClose={() => setAdding(null)}
        onCreated={async (key, agentName) => {
          setAdding(null);
          await loadAgents();
          await refreshAgents();
          setRevealed({ key, name: agentName, rotated: false });
        }}
      />
    );
  }
  if (rotating) {
    return (
      <Modal
        title={`Issue a new key for ${rotating.name}?`}
        subtitle="The current one stops working immediately."
        onClose={() => setRotating(null)}
      >
        <div className="space-y-3.5">
          <p className="text-[13px] leading-relaxed text-base">
            Rivendell stores only a hash of a key, so the current one cannot be shown again —
            issuing a new one is the only way to get a working key. Any session already connected
            as <b>{rotating.name}</b> will start getting 401s until you give it the new key.
          </p>
          {rotating.key_preview && (
            <p className="text-[12px] text-muted">
              Current key <code className="font-mono text-soft">{rotating.key_preview}</code> will
              be revoked.
            </p>
          )}
          <div className="flex justify-end gap-2">
            <Button onClick={() => setRotating(null)}>Cancel</Button>
            <Button
              variant="danger"
              disabled={busy}
              onClick={async () => {
                setBusy(true);
                try {
                  const key = await api.rotateAgentKey(rotating.id);
                  const name = rotating.name;
                  setRotating(null);
                  await loadAgents();
                  setRevealed({ key, name, rotated: true });
                } catch (e) {
                  notify("error", errText(e));
                } finally {
                  setBusy(false);
                }
              }}
            >
              <Icon name="key" size={12} />
              Issue new key
            </Button>
          </div>
        </div>
      </Modal>
    );
  }

  if (editing) {
    return (
      <EditAgentModal
        agent={editing}
        onClose={async () => {
          setEditing(null);
          await loadAgents();
        }}
      />
    );
  }

  async function pickFolder() {
    const chosen = await open({
      directory: true,
      multiple: false,
      title: "Choose a project folder",
      defaultPath: folder,
    });
    if (typeof chosen === "string") setFolder(chosen);
  }

  async function save() {
    setBusy(true);
    try {
      const patch: Record<string, unknown> = { name, color };
      if (folder !== project.folder_path) patch.folder_path = folder;
      await api.updateProject(project.id, patch);
      await refreshRooms();
      onClose();
    } catch (e) {
      notify("error", errText(e));
      setBusy(false);
    }
  }

  async function destroy() {
    setBusy(true);
    try {
      await api.deleteProject(project.id);
      await refreshRooms();
      await selectRoom(null);
      onClose();
    } catch (e) {
      notify("error", errText(e));
      setBusy(false);
    }
  }

  return (
    <Modal wide title={project.name} subtitle="Project settings" onClose={onClose}>
      <div className="space-y-5">
        <div className="grid grid-cols-2 gap-3">
          <Field label="Name">
            <Input value={name} onChange={(e) => setName(e.target.value)} />
          </Field>
          <Field label="Colour" hint="marks it in the sidebar">
            <div className="flex items-center gap-2 pt-1">
              <ColorPicker value={color} onChange={setColor} />
            </div>
          </Field>
        </div>

        <Field
          label="Working folder"
          hint="what agents may read; context already pinned to threads is unaffected"
        >
          <div className="flex gap-2">
            <Input
              value={folder}
              onChange={(e) => setFolder(e.target.value)}
              className="flex-1 font-mono !text-[12px]"
            />
            <Button onClick={pickFolder}>
              <Icon name="folder" size={13} />
              Choose…
            </Button>
          </div>
          {project.git_remote && (
            <p className="mt-1 truncate font-mono text-[11px] text-faint">{project.git_remote}</p>
          )}
        </Field>

        {/* ---------------------------------------------------- agents --- */}
        <div>
          <h3 className="mb-2 text-[11.5px] font-semibold tracking-wide text-soft uppercase">
            Agents
          </h3>
          <p className="mb-2 text-[11.5px] leading-relaxed text-muted">
            An agent belongs to one room, and its API key is what puts it there. The same tool in
            two rooms is two agents with two keys. You can also manage these from the gear
            beside a room in the sidebar.
          </p>

          <div className="space-y-3">
            {projectRooms.length === 0 && (
              <p className="text-[12.5px] text-faint">This project has no rooms yet.</p>
            )}
            {projectRooms.map((room) => {
              const mine = agents.filter((a) => a.room_id === room.id);
              return (
                <div key={room.id} className="rounded-xl bg-card p-2.5 shadow-card ring-1 ring-line">
                  <div className="mb-1.5 flex items-center gap-1.5">
                    <Icon name="hash" size={12} className="text-faint" />
                    <span className="flex-1 font-medium text-strong">{room.name}</span>
                  </div>

                  <AgentRoster
                    compact
                    agents={mine}
                    onAdd={() => setAdding({ id: room.id, name: room.name })}
                    onEdit={setEditing}
                    onRotate={setRotating}
                    onDelete={async (a) => {
                      try {
                        await api.deleteAgent(a.id);
                        await loadAgents();
                        await refreshAgents();
                      } catch (e) {
                        notify("error", errText(e));
                      }
                    }}
                  />
                </div>
              );
            })}
          </div>
        </div>

        {/* ----------------------------------------------------- danger --- */}
        <div className="rounded-xl bg-rose-50 p-3 ring-1 ring-rose-200 dark:bg-rose-500/8 dark:ring-rose-500/25">
          <h3 className="text-[11.5px] font-semibold tracking-wide text-rose-700 uppercase dark:text-rose-300">
            Delete project
          </h3>
          {stats && (
            <p className="mt-1 text-[12.5px] leading-relaxed text-base">
              This destroys <b>{stats.rooms}</b> room{stats.rooms === 1 ? "" : "s"},{" "}
              <b>{stats.threads}</b> thread{stats.threads === 1 ? "" : "s"}, <b>{stats.messages}</b>{" "}
              message{stats.messages === 1 ? "" : "s"} and <b>{stats.agents}</b> agent
              {stats.agents === 1 ? "" : "s"}. It cannot be undone.
              {stats.exported_records > 0 && (
                <>
                  {" "}
                  The <b>{stats.exported_records}</b> decision record
                  {stats.exported_records === 1 ? "" : "s"} already written into the repo are files
                  on disk and will survive.
                </>
              )}
            </p>
          )}

          {confirming ? (
            <div className="mt-2 space-y-2">
              <p className="text-[12px] text-muted">
                Type <b className="font-mono text-base">{project.name}</b> to confirm.
              </p>
              <div className="flex gap-2">
                <Input
                  autoFocus
                  value={confirmText}
                  onChange={(e) => setConfirmText(e.target.value)}
                  className="flex-1"
                />
                <Button
                  variant="danger"
                  disabled={busy || confirmText !== project.name}
                  onClick={destroy}
                >
                  Delete for ever
                </Button>
                <Button
                  variant="subtle"
                  onClick={() => {
                    setConfirming(false);
                    setConfirmText("");
                  }}
                >
                  Cancel
                </Button>
              </div>
            </div>
          ) : (
            <Button variant="danger" size="sm" className="mt-2" onClick={() => setConfirming(true)}>
              <Icon name="trash" size={12} />
              Delete this project
            </Button>
          )}
        </div>

        <div className="flex items-center justify-between border-t border-line pt-3">
          <span className="inline-flex items-center gap-1.5 text-[11.5px] text-faint">
            <span className={`h-2.5 w-2.5 rounded ${swatchFor((color || "slate") as AgentColor)}`} />
            created {new Date(project.created_at).toLocaleDateString()}
          </span>
          <div className="flex gap-2">
            <Button onClick={onClose}>Cancel</Button>
            <Button variant="primary" onClick={save} disabled={busy || !name.trim()}>
              Save
            </Button>
          </div>
        </div>
      </div>
    </Modal>
  );
}
