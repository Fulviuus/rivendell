import { useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import logoDark from "../assets/logo-dark.png";
import logoLight from "../assets/logo.png";
import { api, errText } from "../api";
import { useStore } from "../store";
import { Button, Field, Icon, Input, Modal, swatchFor, Textarea, type AgentColor } from "../ui";
import { ProjectSettings } from "./ProjectSettings";
import { RoomSettings } from "./RoomSettings";
import type { Project } from "../types";

export function Sidebar() {
  const { projects, rooms, roomId, selectRoom, refreshRooms, notify, serverUrl } = useStore();
  const [newRoomFor, setNewRoomFor] = useState<Project | null>(null);
  const [settingsFor, setSettingsFor] = useState<Project | null>(null);
  const [settingsForRoom, setSettingsForRoom] = useState(false);
  const [busy, setBusy] = useState(false);

  const grouped = useMemo(
    () =>
      projects.map((p) => ({
        project: p,
        rooms: rooms.filter((r) => r.project_id === p.id),
      })),
    [projects, rooms],
  );

  async function addProject() {
    try {
      const folder = await open({ directory: true, multiple: false, title: "Choose a project folder" });
      if (typeof folder !== "string") return;
      setBusy(true);
      const project = await api.createProject("", folder);
      // A project with no room cannot hold a conversation, so seed one.
      await api.createRoom(project.id, "general", "");
      await refreshRooms();
      notify("info", `Added ${project.name} with a #general room.`);
    } catch (e) {
      notify("error", errText(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <aside className="flex w-60 shrink-0 flex-col border-r border-line bg-sidebar">
      <div
        data-tauri-drag-region
        className="titlebar-drag flex h-11 items-center justify-between pr-2 pl-20"
      >
        {/* Both variants ship and CSS picks one, so the swap needs no theme
            state threaded down here. */}
        <img src={logoLight} alt="Rivendell" className="h-[18px] w-auto dark:hidden" />
        <img src={logoDark} alt="Rivendell" className="hidden h-[18px] w-auto dark:block" />
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-2">
        {grouped.length === 0 && (
          <p className="px-2 py-6 text-[12.5px] leading-relaxed text-muted">
            Add a project folder to begin. Each project can hold several rooms.
          </p>
        )}

        {grouped.map(({ project, rooms: rs }) => (
          <div key={project.id} className="mb-3">
            <div className="group flex items-center gap-1 px-2 py-1">
              {project.color ? (
                <span
                  className={`h-2.5 w-2.5 shrink-0 rounded ${swatchFor(project.color as AgentColor)}`}
                />
              ) : (
                <Icon name="folder" size={13} className="text-faint" />
              )}
              <span
                className="flex-1 truncate text-[11.5px] font-semibold tracking-wide text-muted uppercase"
                title={project.folder_path}
              >
                {project.name}
              </span>
              <button
                onClick={() => setNewRoomFor(project)}
                title="New room in this project"
                className="rounded p-0.5 text-muted opacity-0 transition group-hover:opacity-100 hover:bg-hover hover:text-strong"
              >
                <Icon name="plus" size={13} />
              </button>
              <button
                onClick={() => setSettingsFor(project)}
                title="Project settings, agents and keys"
                className="rounded p-0.5 text-muted opacity-0 transition group-hover:opacity-100 hover:bg-hover hover:text-strong"
              >
                <Icon name="gear" size={13} />
              </button>
            </div>

            {rs.map((room) => {
              const active = room.id === roomId;
              return (
                // A row, not a single button: the gear has to be its own
                // control, and nesting a button inside a button is invalid.
                <div key={room.id} className="group/room relative flex items-center">
                  <button
                    onClick={() => selectRoom(room.id)}
                    className={`flex w-full items-center gap-1.5 rounded-lg py-1.5 pr-7 pl-2 text-left transition ${
                      active
                        ? "bg-accent-soft font-medium text-accent-text ring-1 ring-accent/25"
                        : "text-soft hover:bg-hover"
                    }`}
                  >
                    <Icon name="hash" size={13} className={active ? "" : "text-faint"} />
                    <span className="flex-1 truncate">{room.name}</span>
                    {room.paused && (
                      <span title="Paused — nothing is dispatched or accepted">
                        <Icon name="pause" size={11} className="text-amber-500" />
                      </span>
                    )}
                    {room.open_threads > 0 && (
                      <span
                        className={`rounded-full px-1.5 text-[10.5px] tabular-nums transition group-hover/room:opacity-0 ${
                          active ? "bg-accent text-on-accent" : "bg-chip text-muted"
                        }`}
                      >
                        {room.open_threads}
                      </span>
                    )}
                  </button>

                  <button
                    title={`Settings and agents for #${room.name}`}
                    onClick={async () => {
                      // Room settings reads the selected room, so select it
                      // first — otherwise the gear on an unselected room would
                      // configure a different one.
                      if (room.id !== roomId) await selectRoom(room.id);
                      setSettingsForRoom(true);
                    }}
                    className="absolute right-1 rounded p-0.5 text-muted opacity-0 transition group-hover/room:opacity-100 hover:bg-hover hover:text-strong"
                  >
                    <Icon name="gear" size={12} />
                  </button>
                </div>
              );
            })}
          </div>
        ))}
      </div>

      <div className="space-y-1 border-t border-line p-2">
        <Button
          variant="subtle"
          size="sm"
          className="w-full !justify-start"
          onClick={addProject}
          disabled={busy}
        >
          <Icon name="plus" size={14} />
          Add project folder
        </Button>
        <div
          className="flex items-center gap-1.5 px-2 pt-1 text-[11px] text-faint"
          title={serverUrl || "The MCP server has not started"}
        >
          <span
            className={`h-1.5 w-1.5 rounded-full ${serverUrl ? "bg-emerald-500" : "bg-faint"}`}
          />
          <span className="truncate">{serverUrl ? serverUrl.replace(/^http:\/\//, "") : "starting…"}</span>
        </div>
      </div>

      {settingsForRoom && <RoomSettings onClose={() => setSettingsForRoom(false)} />}

      {settingsFor && (
        <ProjectSettings
          project={settingsFor}
          onClose={() => setSettingsFor(null)}
        />
      )}

      {newRoomFor && (
        <NewRoomModal
          project={newRoomFor}
          onClose={() => setNewRoomFor(null)}
          onCreated={async (id) => {
            setNewRoomFor(null);
            await refreshRooms();
            await selectRoom(id);
          }}
        />
      )}
    </aside>
  );
}

function NewRoomModal({
  project,
  onClose,
  onCreated,
}: {
  project: Project;
  onClose: () => void;
  onCreated: (id: number) => void;
}) {
  const notify = useStore((s) => s.notify);
  const [name, setName] = useState("");
  const [purpose, setPurpose] = useState("");
  const [busy, setBusy] = useState(false);

  async function submit() {
    if (!name.trim()) return;
    setBusy(true);
    try {
      onCreated(await api.createRoom(project.id, name, purpose));
    } catch (e) {
      notify("error", errText(e));
      setBusy(false);
    }
  }

  return (
    <Modal title="New room" subtitle={project.name} onClose={onClose}>
      <div className="space-y-3.5">
        <Field label="Name" hint="lowercase, no spaces">
          <Input
            autoFocus
            value={name}
            placeholder="security-review"
            onChange={(e) => setName(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && submit()}
          />
        </Field>
        <Field label="Purpose" hint="optional">
          <Textarea
            rows={2}
            value={purpose}
            placeholder="What this room is for. Shown to agents that join it."
            onChange={(e) => setPurpose(e.target.value)}
          />
        </Field>
        <div className="flex justify-end gap-2 pt-1">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} disabled={busy || !name.trim()}>
            Create room
          </Button>
        </div>
      </div>
    </Modal>
  );
}
