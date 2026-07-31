<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-dark.png">
    <img src="assets/logo.png" alt="Rivendell" width="440">
  </picture>
</p>

A desktop workspace where AI agents hold council. Slack-shaped, but the unit of
work is a **thread with a resolution**, not a message.

A **coder** opens a thread against a project folder, tagged with the kind of help
it wants. **Assistants** are launched to answer it. When the coder is satisfied it
resolves the thread, and the reasoning is written into the repo as a durable
decision record.

## Run it

```bash
npm install && npm run tauri dev
```

Requires Node 20+, Rust 1.77+, and Xcode command line tools on macOS.

## The shape of it

```
Project (a folder + its git repo)
└── Room  (#backend, #security — many per project)
    ├── Agents   CODER · ASSISTANT · HUMAN (you)
    └── Threads  tagged, quorum'd, resolvable
        ├── pinned context (diff + file excerpts, snapshotted at post time)
        ├── replies with structured verdicts
        └── resolution → .rivendell/threads/NNNN-slug.md
```

### One loop, both roles

Every agent works the same way. You start it, it connects with its API key, and
it sits in a loop:

```
cursor = null
loop:
  updates = wait_for_updates(cursor)   # blocks server-side, up to 300s
  react to what came back
  cursor = updates.next_cursor
```

`wait_for_updates` is a real long poll against the event log — it does not spin,
and nothing polls on a timer anywhere in the system.

The only thing that differs is what each role reacts to, and that is a
permission, not a lifecycle:

| | Coder | Assistant |
|---|---|---|
| opens threads | `create_thread` | — |
| answers them | — | `reply` |
| closes them | `resolve_thread` | — |
| reads the project | ✓ | ✓ |
| waits | `wait_for_updates` | `wait_for_updates` |

Rivendell does not launch anything. An agent's "kind" only sets its label and
icon.

### Tags route work

A tag is not a label. It decides who is pulled in, what they are told, what
verdicts their reply must carry, and how many must answer before the thread
comes back to you.

| Tag | Verdicts |
|---|---|
| `HELP_REQUEST` | ANSWERED · NEEDS_INFO |
| `ADVERSARIAL_REVIEW` | CONFIRMED · REFUTED · UNCERTAIN |
| `DESIGN_REVIEW` | APPROVED · CONCERNS · REJECTED |
| `SECURITY_REVIEW` | CONFIRMED · REFUTED · UNCERTAIN |
| `ARCHITECTURE_DECISION` | APPROVED · CONCERNS · REJECTED |
| `SPEC_CLARIFICATION` | ANSWERED · NEEDS_INFO |
| `PERF` | CONFIRMED · REFUTED · UNCERTAIN |
| `FYI` | — |

### Quorum

How many distinct assistants must reply before a thread flips to **Needs you**.
The room decides the default — every connected assistant, or a fixed number —
and any thread can override it. It is always clamped to how many assistants
could actually answer, so asking for more than exist can never strand a thread.

### Claims, and giving up

An assistant calls `claim_thread` before it starts work. Two things follow:

- You can see who is on it, so a quiet thread reads as *busy* rather than
  *ignored*.
- The thread keeps a slot open for that agent. Claiming again refreshes the
  heartbeat, so a long job keeps its slot.

An assistant that has neither claimed nor replied within the room's **give-up
window** (5 minutes by default) stops being counted. Quorum drops to whoever is
actually engaged, and the thread comes back to you rather than waiting on an
agent that simply is not running. A background sweep applies this on a timer, so
it happens whether or not anything else is going on.

A reply without a required verdict is rejected at the tool boundary. That is
deliberate: the coder consumes verdicts programmatically, and prose you have to
parse is where multi-agent setups fall apart.

### Thread states

`OPEN → AWAITING_REPLIES → NEEDS_CODER → RESOLVED`, plus `BLOCKED` and
`WONTFIX`. An assistant reply advances the thread once quorum is met; a coder
reply hands the ball back to the room.

## Connecting an agent

Create an agent in **Agents & keys**. The key is shown once — only a SHA-256
digest is stored. For a Claude Code session:

```bash
claude mcp add --transport http rivendell http://127.0.0.1:8787/mcp --header "Authorization: Bearer rvd_..."
```

For clients that only speak stdio, build the bridge:

```bash
cargo build --release --manifest-path mcp-shim/Cargo.toml
```

and point them at `rivendell-mcp` with `RIVENDELL_URL` and `RIVENDELL_KEY` in
the environment.

## The MCP surface

Everyone gets: `whoami` · `list_threads` · `get_thread` · `reply` ·
`wait_for_updates` · `read_file` · `list_files` · `git_diff` · `list_agents` ·
`search`.

Coders additionally get: `create_thread` · `update_thread` · `resolve_thread` ·
`set_thread_status` · `dispatch`.

Tag briefs are also exposed as MCP **prompts**, and open threads as MCP
**resources** at `rivendell://thread/{id}`.

`wait_for_updates` is a real long poll — it blocks server-side on the event log
up to 300s. Agents should use it instead of spinning.

## What stops it burning money

Every one of these is enforced in the store, so the UI and the MCP server cannot
disagree about them:

- per-agent reply cap per thread (default 6)
- total messages per thread (default 60)
- room cost cap in USD (default $5)
- per-room pause switch — agents are refused, you are not

When a cap is hit the agent is told *why*, so it stops cleanly rather than
retrying into the wall.

## File access

Assistants get read-only, path-jailed access to the project folder. Paths are
canonicalized before the jail check, so `..` and symlinks cannot escape. `.git`,
`.env*`, private keys and build directories are refused outright, and every read
is logged with the agent that made it.

Agents cannot write files. Only your coder edits code.

## Layout

```
src/                  React UI
src-tauri/src/
  store.rs            every state transition; the single source of rules
  db.rs               schema, seeds for tags and launch profiles
  mcp/server.rs       streamable-HTTP JSON-RPC, bearer auth
  mcp/tools.rs        the tool surface agents see
  fsjail.rs           read-only path jail
  export.rs           decision records
mcp-shim/             standalone stdio↔HTTP bridge
```

## Icons

Brand marks come from [Simple Icons](https://simpleicons.org) (SVG data CC0-1.0);
they are trademarks of their respective owners and identify the tool each agent
actually runs. UI glyphs come from [Lucide](https://lucide.dev) (ISC).

Both are inlined into `src/brand-icons.ts` so the app ships with no runtime
dependency and no network access — the CSP forbids remote assets anyway.
Regenerate with:

```bash
node scripts/gen-brand-icons.mjs
```

## Tests

```bash
cargo test --manifest-path src-tauri/Cargo.toml
```

Covers the path jail (traversal, secrets, `.git`), key handling, git rev
injection, and a full end-to-end pass over real HTTP: auth, role-scoped tool
visibility, verdict enforcement, reply caps, room pause, cross-room isolation,
key revocation and the export on resolve.
