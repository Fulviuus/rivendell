import { useState } from "react";
import { api, errText } from "../api";
import { useStore } from "../store";
import { Button, Field, Icon, Input, Modal, Textarea } from "../ui";

export function RoomSettings({ onClose }: { onClose: () => void }) {
  const { rooms, roomId, refreshRooms, selectRoom, notify } = useStore();
  const room = rooms.find((r) => r.id === roomId);

  const [purpose, setPurpose] = useState(room?.purpose ?? "");
  const [maxReplies, setMaxReplies] = useState(String(room?.max_replies_per_agent ?? 6));
  const [maxMessages, setMaxMessages] = useState(String(room?.max_thread_messages ?? 60));
  const [costCap, setCostCap] = useState(String(room?.cost_cap_usd ?? 5));
  const [timeout, setTimeoutSecs] = useState(String((room?.response_timeout_secs ?? 300) / 60));
  const [claimWindow, setClaimWindow] = useState(String(room?.claim_window_secs ?? 120));
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [busy, setBusy] = useState(false);

  if (!room) return null;

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
    <Modal title={`#${room.name}`} subtitle={room.folder_path} onClose={onClose}>
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
