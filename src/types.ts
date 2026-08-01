export type Role = "CODER" | "ASSISTANT" | "HUMAN";

export type ThreadStatus =
  | "OPEN"
  | "AWAITING_REPLIES"
  | "NEEDS_CODER"
  | "RESOLVED"
  | "BLOCKED"
  | "WONTFIX";

export interface Project {
  id: number;
  name: string;
  folder_path: string;
  git_remote: string | null;
  color: string;
  created_at: string;
}

export interface Room {
  id: number;
  project_id: number;
  project_name: string;
  folder_path: string;
  name: string;
  purpose: string;
  paused: boolean;
  max_replies_per_agent: number;
  max_thread_messages: number;
  /** Seconds a thread waits on an assistant that has shown no sign of life. */
  response_timeout_secs: number;
  cost_cap_usd: number;
  /** After the first agent answers, seconds the others get to claim. */
  claim_window_secs: number;
  open_threads: number;
  created_at: string;
}

export interface AgentProfile {
  id: number;
  key: string;
  label: string;
  icon: string;
  launch_cmd: string;
  launch_args: string;
  mcp_install_mode: string;
  notes: string;
  builtin: boolean;
}

export interface Agent {
  id: number;
  project_id: number;
  name: string;
  role: Role;
  profile_id: number | null;
  profile_key: string | null;
  profile_label: string | null;
  icon: string;
  color: string;
  key_preview: string | null;
  system_note: string;
  created_at: string;
  revoked_at: string | null;
}

export interface Tag {
  key: string;
  label: string;
  color: string;
  instruction: string;
  requires_verdict: boolean;
  verdict_options: string[];
  expects_replies: boolean;
  builtin: boolean;
}

export interface ThreadContextItem {
  id: number;
  kind: string;
  path: string | null;
  start_line: number | null;
  end_line: number | null;
  content: string;
}

export interface Message {
  id: number;
  thread_id: number;
  agent_id: number;
  agent_name: string;
  agent_role: Role;
  icon: string;
  color: string;
  body: string;
  verdict: string | null;
  severity: string | null;
  refs: { path?: string; line?: number; note?: string }[];
  tokens_in: number;
  tokens_out: number;
  cost_usd: number;
  created_at: string;
  /** Set once the author has revised it. */
  edited_at: string | null;
}

export interface ThreadClaim {
  agent_id: number;
  agent_name: string;
  color: string;
  icon: string;
  note: string;
  claimed_at: string;
}

export interface ThreadSummary {
  id: number;
  room_id: number;
  room_name: string;
  title: string;
  tag: string;
  status: ThreadStatus;
  author_agent_id: number;
  author_name: string;
  author_icon: string;
  author_color: string;
  reply_count: number;
  responder_count: number;
  /** Claimed, still live, not yet answered. */
  in_progress: number;
  cost_usd: number;
  git_ref: string | null;
  created_at: string;
  updated_at: string;
  resolved_at: string | null;
  last_reply_at: string | null;
}

export const THREAD_SORTS = [
  { key: "last_reply", label: "Last reply" },
  { key: "created", label: "Newest" },
  { key: "activity", label: "Most active" },
] as const;

export type ThreadSort = (typeof THREAD_SORTS)[number]["key"];

export interface ThreadDetail extends ThreadSummary {
  body: string;
  git_dirty: boolean;
  resolution_summary: string | null;
  export_path: string | null;
  context: ThreadContextItem[];
  mentions: number[];
  claims: ThreadClaim[];
  messages: Message[];
}

export interface NewAgentKey {
  agent_id: number;
  api_key: string;
  mcp_json: string;
  claude_cli: string;
  shim_json: string;
}

export interface EventNotice {
  seq: number;
  room_id: number | null;
  thread_id: number | null;
  kind: string;
}

export interface ContextInput {
  kind: "file" | "diff" | "note";
  path?: string | null;
  start_line?: number | null;
  end_line?: number | null;
  content?: string | null;
}

export const STATUS_LABEL: Record<ThreadStatus, string> = {
  OPEN: "Open",
  AWAITING_REPLIES: "Awaiting replies",
  NEEDS_CODER: "Needs you",
  RESOLVED: "Resolved",
  BLOCKED: "Blocked",
  WONTFIX: "Won't fix",
};

export const SEVERITIES = ["CRITICAL", "HIGH", "MEDIUM", "LOW", "INFO"] as const;
