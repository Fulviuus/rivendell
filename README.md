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

### Two lifecycles, deliberately

- **Assistants** are ephemeral. Rivendell spawns one process per thread, it
  replies, it exits. It authenticates with a token minted for that run and
  revoked when the process dies — the long-lived key is never handed to a
  subprocess.
- **Coders** are long-lived. Your own Claude Code session attaches with its API
  key and stays. It calls `wait_for_updates` to learn when replies land.

### Tags route work

A tag is not a label. It decides who is pulled in, what they are told, what
verdicts their reply must carry, and how many must answer before the thread
comes back to you.

| Tag | Verdicts | Quorum |
|---|---|---|
| `HELP_REQUEST` | ANSWERED · NEEDS_INFO | 1 |
| `ADVERSARIAL_REVIEW` | CONFIRMED · REFUTED · UNCERTAIN | 2 |
| `DESIGN_REVIEW` | APPROVED · CONCERNS · REJECTED | 2 |
| `SECURITY_REVIEW` | CONFIRMED · REFUTED · UNCERTAIN | 2 |
| `ARCHITECTURE_DECISION` | APPROVED · CONCERNS · REJECTED | 2 |
| `SPEC_CLARIFICATION` | ANSWERED · NEEDS_INFO | 1 |
| `PERF` | CONFIRMED · REFUTED · UNCERTAIN | 1 |
| `FYI` | — | 0 |

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
- concurrent spawned processes per room (default 3)
- room cost cap in USD (default $5)
- per-room pause switch — agents are refused, you are not
- 20-minute hard kill on any spawned process

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
  spawner.rs          process launch, ephemeral run tokens
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
