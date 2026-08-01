import { invoke } from "@tauri-apps/api/core";
import type {
  Agent,
  AgentProfile,
  ContextInput,
  EventNotice,
  NewAgentKey,
  Project,
  Room,
  Tag,
  ThreadDetail,
  ThreadStatus,
  ThreadSummary,
} from "./types";

export const api = {
  serverInfo: () => invoke<{ url: string; listening: boolean }>("server_info"),

  listProjects: () => invoke<Project[]>("list_projects"),
  createProject: (name: string, folder: string) =>
    invoke<Project>("create_project", { name, folder }),
  deleteProject: (id: number) => invoke<void>("delete_project", { id }),

  listRooms: () => invoke<Room[]>("list_rooms"),
  createRoom: (projectId: number, name: string, purpose: string) =>
    invoke<number>("create_room", { projectId, name, purpose }),
  updateRoom: (id: number, patch: Record<string, unknown>) =>
    invoke<void>("update_room", { id, patch }),
  deleteRoom: (id: number) => invoke<void>("delete_room", { id }),

  listProfiles: () => invoke<AgentProfile[]>("list_profiles"),
  upsertProfile: (profile: Record<string, unknown>) =>
    invoke<number>("upsert_profile", { profile }),

  listAgents: (roomId?: number) => invoke<Agent[]>("list_agents", { roomId: roomId ?? null }),
  createAgent: (args: {
    roomId: number;
    name: string;
    role: string;
    profileId: number | null;
    systemNote: string;
    color: string;
  }) => invoke<NewAgentKey>("create_agent", args),
  updateAgent: (agentId: number, patch: Record<string, unknown>) =>
    invoke<void>("update_agent", { agentId, patch }),
  rotateAgentKey: (agentId: number) => invoke<NewAgentKey>("rotate_agent_key", { agentId }),
  setAgentRevoked: (agentId: number, revoked: boolean) =>
    invoke<void>("set_agent_revoked", { agentId, revoked }),
  deleteAgent: (agentId: number) => invoke<void>("delete_agent", { agentId }),

  listTags: () => invoke<Tag[]>("list_tags"),

  listThreads: (roomId: number | null, status?: string, tag?: string, sort?: string) =>
    invoke<ThreadSummary[]>("list_threads", {
      roomId,
      status: status ?? null,
      tag: tag ?? null,
      sort: sort ?? null,
      limit: 200,
    }),
  getThread: (threadId: number) => invoke<ThreadDetail>("get_thread", { threadId }),
  createThread: (input: {
    room_id: number;
    title: string;
    body: string;
    tag: string;
    mentions: number[];
    context: ContextInput[];
    quorum?: number | null;
    include_diff: boolean;
  }) => invoke<number>("create_thread", { input, asAgentId: null }),
  reply: (input: {
    thread_id: number;
    body: string;
    verdict?: string | null;
    severity?: string | null;
    refs?: unknown;
  }) => invoke<number>("reply", { input, asAgentId: null }),
  editMessage: (
    messageId: number,
    input: {
      thread_id: number;
      body: string;
      verdict?: string | null;
      severity?: string | null;
      refs?: unknown;
    },
  ) => invoke<void>("edit_message", { messageId, input, asAgentId: null }),
  updateThread: (threadId: number, body: string) =>
    invoke<void>("update_thread", { threadId, body, asAgentId: null }),
  resolveThread: (threadId: number, summary: string, status?: string) =>
    invoke<string | null>("resolve_thread", {
      threadId,
      summary,
      status: status ?? null,
      asAgentId: null,
    }),
  setThreadStatus: (threadId: number, status: ThreadStatus) =>
    invoke<void>("set_thread_status", { threadId, status, asAgentId: null }),

  claimThread: (threadId: number, note = "") =>
    invoke<void>("claim_thread", { threadId, note, asAgentId: null }),

  search: (roomId: number | null, query: string) =>
    invoke<{ kind: string; ref_id: number; title: string; excerpt: string }[]>("search", {
      roomId,
      query,
      limit: 40,
    }),
  eventsSince: (cursor: number, roomId?: number) =>
    invoke<EventNotice[]>("events_since", { cursor, roomId: roomId ?? null }),

  filePreview: (roomId: number, path: string, startLine?: number, endLine?: number) =>
    invoke<{
      path: string;
      start_line: number;
      end_line: number;
      total_lines: number;
      content: string;
    }>("file_preview", {
      roomId,
      path,
      startLine: startLine ?? null,
      endLine: endLine ?? null,
    }),
  listProjectFiles: (roomId: number, path?: string) =>
    invoke<string[]>("list_project_files", { roomId, path: path ?? null }),
  gitStatus: (roomId: number) =>
    invoke<{ is_repo: boolean; branch: string | null; head: string | null; dirty: boolean }>(
      "git_status",
      { roomId },
    ),
};

/** Backend errors arrive as plain strings; make them readable wherever they surface. */
export function errText(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return JSON.stringify(e);
}
