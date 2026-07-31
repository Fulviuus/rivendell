import { create } from "zustand";
import { listen } from "@tauri-apps/api/event";
import { api } from "./api";
import type {
  Agent,
  AgentProfile,
  EventNotice,
  Project,
  Room,
  Tag,
  ThreadDetail,
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
  toast: { kind: "error" | "info"; text: string } | null;

  boot: () => Promise<void>;
  selectRoom: (id: number | null) => Promise<void>;
  selectThread: (id: number | null) => Promise<void>;
  refreshRooms: () => Promise<void>;
  refreshThreads: () => Promise<void>;
  refreshAgents: () => Promise<void>;
  refreshThread: () => Promise<void>;
  setFilters: (f: { status?: string; tag?: string }) => Promise<void>;
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
  statusFilter: "open",
  tagFilter: "all",
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
      if (n.kind.startsWith("room.") || n.kind.startsWith("project.")) {
        await s.refreshRooms();
      }
      if (n.room_id !== null && n.room_id !== s.roomId) return;
      if (n.kind.startsWith("agent.")) await s.refreshAgents();
      await s.refreshThreads();
      if (n.thread_id !== null && n.thread_id === s.threadId) await s.refreshThread();
    });

    await listen<string>("rivendell://server", (e) => set({ serverUrl: e.payload }));
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
    const { roomId, statusFilter, tagFilter } = get();
    if (roomId === null) return;
    set({ threads: await api.listThreads(roomId, statusFilter, tagFilter) });
  },

  refreshAgents: async () => {
    const { roomId } = get();
    if (roomId === null) return;
    set({ agents: await api.listAgents(roomId) });
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
    });
    await get().refreshThreads();
  },
}));
