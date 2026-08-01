import { useEffect, useState } from "react";
import { api, errText } from "../api";
import { useStore } from "../store";
import { Avatar, Button, Field, Icon, Input, Modal, Textarea } from "../ui";
import { AgentRoster } from "./AgentRoster";
import { ConnectionModal, EditAgentModal, NewAgentModal } from "./AgentModals";
import type { Agent, NewAgentKey } from "../types";

export function RoomSettings({ onClose }: { onClose: () => void }) {
  const { rooms, roomId, agents, refreshRooms, refreshAgents, selectRoom, notify } = useStore();
  const room = rooms.find((r) => r.id === roomId);

  const [purpose, setPurpose] = useState(room?.purpose ?? "");
  const [maxReplies, setMaxReplies] = useState(String(room?.max_replies_per_agent ?? 6));
  const [maxMessages, setMaxMessages] = useState(String(room?.max_thread_messages ?? 60));
  const [costCap, setCostCap] = useState(String(room?.cost_cap_usd ?? 5));
  const [timeout, setTimeoutSecs] = useState(String((room?.response_timeout_secs ?? 300) / 60));
  const [claimWindow, setClaimWindow] = useState(String(room?.claim_window_secs ?? 120));
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [adding, setAdding] = useState(false);
  // Everyone in the project, so the picker can offer those not in this room.
  const [projectAgents, setProjectAgents] = useState<Agent[]>([]);
  const [editing, setEditing] = useState<Agent | null>(null);
  const [rotating, setRotating] = useState<Agent | null>(null);
  const [revealed, setRevealed] = useState<{ key: NewAgentKey; name: string; rotated: boolean } | null>(
    null,
  );
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!room) return;
    api
      .listAgents()
      .then((all) => setProjectAgents(all.filter((a) => a.project_id === room.project_id)))
      .catch(() => setProjectAgents([]));
  }, [room?.project_id, agents.length]);

  if (!room) return null;

  const notHere = projectAgents.filter((a) => !agents.some((m) => m.id === a.id));

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
        projectId={room.project_id}
        roomId={room.id}
        roomName={room.name}
        onClose={() => setAdding(false)}
        onCreated={async (key, name) => {
          setAdding(false);
          await refreshAgents();
          setRevealed({ key, name, rotated: false });
        }}
      />
    );
  }
  if (editing) {
    return (
      <EditAgentModal
        agent={editing}
        onClose={async () => {
          setEditing(null);
          await refreshAgents();
        }}
      />
    );
  }
  if (rotating) {
    const target = rotating;
    return (
      <Modal
        title={`Issue a new key for ${target.name}?`}
        subtitle="The current one stops working immediately."
        onClose={() => setRotating(null)}
      >
        <div className="space-y-3.5">
          <p className="text-[13px] leading-relaxed text-body">
            Only a hash of a key is stored, so the current one cannot be shown again. Anything
            connected as <b>{target.name}</b> will start getting 401s until you give it the new key.
          </p>
          <div className="flex justify-end gap-2">
            <Button onClick={() => setRotating(null)}>Cancel</Button>
            <Button
              variant="danger"
              onClick={async () => {
                try {
                  const key = await api.rotateAgentKey(target.id);
                  setRotating(null);
                  await refreshAgents();
                  setRevealed({ key, name: target.name, rotated: true });
                } catch (e) {
                  notify("error", errText(e));
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

  async function save() {
    if (!room) return;
    setBusy(true);
    try {
      await api.updateRoom(room.id, {
        purpose,
        max_replies_per_agent: Number(maxReplies) || 1,
        max_thread_messages: Number(maxMessages) || 10,
        cost_cap_usd: Number(costCap) || 0,
        response_timeout_secs: Math.max(30, Math.round((Number(timeout) || 5) * 60)),
        claim_window_secs: Math.max(0, Number(claimWindow) || 0),
      });
      await refreshRooms();
      onClose();
    } catch (e) {
      notify("error", errText(e));
      setBusy(false);
    }
  }

  async function togglePause() {
    if (!room) return;
    try {
      await api.updateRoom(room.id, { paused: !room.paused });
      await refreshRooms();
    } catch (e) {
      notify("error", errText(e));
    }
  }

  return (
    <Modal wide title={`#${room.name}`} subtitle={room.folder_path} onClose={onClose}>
      <div className="space-y-3.5">
        <div
          className={`flex items-center gap-3 rounded-xl p-3 ring-1 ${
            room.paused
              ? "bg-amber-50 ring-amber-300 dark:bg-amber-500/8 dark:ring-amber-500/25"
              : "bg-card shadow-card ring-line"
          }`}
        >
          <Icon
            name={room.paused ? "pause" : "play"}
            size={16}
            className={room.paused ? "text-amber-500" : "text-emerald-500"}
          />
          <div className="flex-1">
            <p className="font-semibold text-strong">{room.paused ? "Paused" : "Running"}</p>
            <p className="text-[11.5px] text-muted">
              {room.paused
                ? "Nothing is dispatched and no agent post is accepted. You can still post."
                : "Threads dispatch assistants automatically."}
            </p>
          </div>
          <Button size="sm" onClick={togglePause}>
            {room.paused ? "Resume" : "Pause room"}
          </Button>
        </div>

        <div>
          <h3 className="mb-1 text-[11.5px] font-semibold tracking-wide text-soft uppercase">
            Agents in this room
          </h3>
          <p className="mb-2 text-[11.5px] leading-relaxed text-muted">
            An agent belongs to the project and joins rooms. Taking one out of #{room.name} does
            not delete it — it keeps its key and its other rooms, and comes back to the list below.
            Deleting one for good is in project settings.
          </p>

          <AgentRoster
            mode="room"
            agents={agents}
            addLabel="Create a new agent, and put it here"
            onAdd={() => setAdding(true)}
            onEdit={setEditing}
            onRotate={setRotating}
            onChanged={refreshAgents}
            onRemove={async (a) => {
              try {
                await api.leaveRoom(room.id, a.id);
                await refreshAgents();
              } catch (e) {
                notify("error", errText(e));
              }
            }}
          />

          {notHere.length > 0 && (
            <div className="mt-2 rounded-xl bg-code p-2.5 ring-1 ring-line">
              <p className="mb-1.5 text-[11.5px] text-muted">
                Already in this project — click to add to #{room.name}:
              </p>
              <div className="flex flex-wrap gap-1.5">
                {notHere.map((a) => (
                  <button
                    key={a.id}
                    onClick={async () => {
                      try {
                        await api.joinRoom(room.id, a.id);
                        await refreshAgents();
                      } catch (e) {
                        notify("error", errText(e));
                      }
                    }}
                    className="inline-flex items-center gap-1.5 rounded-lg bg-card px-2 py-1 text-[12.5px] text-body shadow-card ring-1 ring-line transition hover:ring-accent/40"
                  >
                    <Avatar name={a.name} icon={a.icon} color={a.color} size={16} />
                    {a.name}
                    <Icon name="plus" size={11} className="text-faint" />
                  </button>
                ))}
              </div>
            </div>
          )}
        </div>

        <Field label="Purpose">
          <Textarea rows={2} value={purpose} onChange={(e) => setPurpose(e.target.value)} />
        </Field>


        <div className="grid grid-cols-2 gap-3">
          <Field label="Replies per agent" hint="per thread">
            <Input value={maxReplies} onChange={(e) => setMaxReplies(e.target.value)} />
          </Field>
          <Field label="Messages per thread">
            <Input value={maxMessages} onChange={(e) => setMaxMessages(e.target.value)} />
          </Field>
          <Field label="Cost cap" hint="USD, room total; 0 = off">
            <Input value={costCap} onChange={(e) => setCostCap(e.target.value)} />
          </Field>
          <Field label="Claim window" hint="seconds after the first answer">
            <Input value={claimWindow} onChange={(e) => setClaimWindow(e.target.value)} />
          </Field>
          <Field label="Drop a silent worker" hint="minutes without a reply">
            <Input value={timeout} onChange={(e) => setTimeoutSecs(e.target.value)} />
          </Field>
        </div>

        <p className="text-[11.5px] leading-relaxed text-faint">
          Caps are what stops agents replying to each other all night. When one is hit the agent is
          told why, so it can stop cleanly instead of retrying. A thread waits indefinitely for its
          first answer; that answer opens the claim window for the others, and anyone silent through
          it is left out. An agent that claims but then goes quiet is dropped once the second timer
          passes, so one stalled worker cannot hold a thread open.
        </p>

        <div className="flex items-center justify-between border-t border-line pt-3">
          {confirmDelete ? (
            <div className="flex items-center gap-2">
              <span className="text-[12.5px] text-soft">Delete #{room.name} and its threads?</span>
              <Button
                size="sm"
                variant="danger"
                onClick={async () => {
                  await api.deleteRoom(room.id);
                  await refreshRooms();
                  await selectRoom(null);
                  onClose();
                }}
              >
                Delete
              </Button>
              <Button size="sm" variant="subtle" onClick={() => setConfirmDelete(false)}>
                Cancel
              </Button>
            </div>
          ) : (
            <Button size="sm" variant="subtle" onClick={() => setConfirmDelete(true)}>
              <Icon name="trash" size={12} />
              Delete room
            </Button>
          )}
          <Button variant="primary" onClick={save} disabled={busy}>
            Save
          </Button>
        </div>
      </div>
    </Modal>
  );
}
