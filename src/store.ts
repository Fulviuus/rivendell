import { create } from "zustand";
import { listen } from "@tauri-apps/api/event";
import { api } from "./api";
import type {
  Agent,
  AgentProfile,
  AwakeStatus,
  ConnectedAgent,
  EventNotice,
  Project,
  Room,
  Tag,
  ThreadDetail,
  ThreadSort,
  ThreadSummary,
} from "./types";

interface State {
  ready: boolean;
  serverUrl: string;

  projects: Project[];
  rooms: Room[];
  tags: Tag[];
  profiles: AgentProfile[];

  roomId: number | null;
  threads: ThreadSummary[];
  agents: Agent[];
  threadId: number | null;
  thread: ThreadDetail | null;

  statusFilter: string;
  tagFilter: string;
  sortBy: ThreadSort;
  toast: { kind: "error" | "info"; text: string } | null;
  /** Live run state by agent id, pushed from the supervisor. */
  awake: Record<number, AwakeStatus>;
  /** Who is holding a connection to the listener, pushed on its own channel. */
  connections: ConnectedAgent[];

  boot: () => Promise<void>;
  selectRoom: (id: number | null) => Promise<void>;
  selectThread: (id: number | null) => Promise<void>;
  refreshRooms: () => Promise<void>;
  refreshThreads: () => Promise<void>;
  refreshAgents: () => Promise<void>;
  refreshThread: () => Promise<void>;
  refreshAwake: () => Promise<void>;
  refreshConnections: () => Promise<void>;
  setFilters: (f: { status?: string; tag?: string; sort?: ThreadSort }) => Promise<void>;
  notify: (kind: "error" | "info", text: string) => void;
}

export const useStore = create<State>((set, get) => ({
  ready: false,
  serverUrl: "",
  projects: [],
  rooms: [],
  tags: [],
  profiles: [],
  roomId: null,
  threads: [],
  agents: [],
  threadId: null,
  thread: null,
  awake: {},
  connections: [],
  statusFilter: "open",
  tagFilter: "all",
  sortBy: "last_reply",
  toast: null,

  notify: (kind, text) => {
    set({ toast: { kind, text } });
    setTimeout(() => {
      // Only clear if this is still the message on screen.
      if (get().toast?.text === text) set({ toast: null });
    }, 6000);
  },

  boot: async () => {
    const [projects, rooms, tags, profiles, info] = await Promise.all([
      api.listProjects(),
      api.listRooms(),
      api.listTags(),
      api.listProfiles(),
      api.serverInfo(),
    ]);
    set({ projects, rooms, tags, profiles, serverUrl: info.url, ready: true });

    const first = rooms[0]?.id ?? null;
    if (first !== null) await get().selectRoom(first);

    // The backend's append-only event log drives every refresh; nothing polls.
    await listen<EventNotice>("rivendell://event", async (e) => {
      const s = get();
      const n = e.payload;
      // The badge on each room counts its open threads, so anything that
      // moves a thread in or out of that set changes it — resolving, closing,
      // reopening, opening a new one. This runs before the "is it the room I
      // am looking at" guard below on purpose: a thread resolved by an agent
      // in another room still has to update that room's badge.
      if (
        n.kind.startsWith("room.") ||
        n.kind.startsWith("project.") ||
        n.kind.startsWith("thread.")
      ) {
        await s.refreshRooms();
      }
      if (n.kind.startsWith("agent.")) await s.refreshAgents();
      // The connected list carries names and project details, so a rename
      // must reach it without waiting for the agent to reconnect.
      if (n.kind.startsWith("agent.") || n.kind.startsWith("project.")) {
        await s.refreshConnections();
      }
      if (n.room_id !== null && n.room_id !== s.roomId) return;
      await s.refreshThreads();
      if (n.thread_id !== null && n.thread_id === s.threadId) await s.refreshThread();
    });

    await listen<string>("rivendell://server", (e) => set({ serverUrl: e.payload }));

    // Run state rides its own channel rather than the shared event log: what
    // Rivendell started is the user's business, not something to announce to
    // every agent listening on wait_for_updates.
    await listen<AwakeStatus>("rivendell://awake", (e) => {
      const s = e.payload;
      set((prev) => ({ awake: { ...prev.awake, [s.agent_id]: s } }));
      if (s.trouble) get().notify("error", s.trouble);
    });
    await get().refreshAwake();

    // Presence rides its own channel too — the payload is just a nudge, the
    // list itself is fetched so it is always the joined, current view.
    await listen("rivendell://presence", () => void get().refreshConnections());
    await get().refreshConnections();
  },

  selectRoom: async (id) => {
    set({ roomId: id, threadId: null, thread: null, threads: [], agents: [] });
    if (id === null) return;
    await Promise.all([get().refreshThreads(), get().refreshAgents()]);
  },

  selectThread: async (id) => {
    set({ threadId: id, thread: null });
    if (id === null) return;
    await get().refreshThread();
  },

  refreshRooms: async () => {
    const [rooms, projects] = await Promise.all([api.listRooms(), api.listProjects()]);
    set({ rooms, projects });
  },

  refreshThreads: async () => {
    const { roomId, statusFilter, tagFilter, sortBy } = get();
    if (roomId === null) return;
    set({ threads: await api.listThreads(roomId, statusFilter, tagFilter, sortBy) });
  },

  refreshAgents: async () => {
    const { roomId } = get();
    if (roomId === null) return;
    set({ agents: await api.listAgents(roomId) });
  },

  refreshAwake: async () => {
    const rows = await api.awakeStatus();
    set({ awake: Object.fromEntries(rows.map((r) => [r.agent_id, r])) });
  },

  refreshConnections: async () => {
    set({ connections: await api.listConnections() });
  },

  refreshThread: async () => {
    const { threadId } = get();
    if (threadId === null) return;
    try {
      set({ thread: await api.getThread(threadId) });
    } catch {
      // Deleted out from under us — drop the selection rather than wedging.
      set({ threadId: null, thread: null });
    }
  },

  setFilters: async (f) => {
    set({
      statusFilter: f.status ?? get().statusFilter,
      tagFilter: f.tag ?? get().tagFilter,
      sortBy: f.sort ?? get().sortBy,
    });
    await get().refreshThreads();
  },
}));
