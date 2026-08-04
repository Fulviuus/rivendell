//! Who is on the wire right now.
//!
//! The listener has three ways of being held — the `wait_for_updates` long
//! poll, the `/ws` wake socket, and the SSE notification stream — and this is
//! the one place they all report to. Every hold registers a guard on the way
//! in and unregisters by being dropped, so a connection that ends any way at
//! all (return, error, client hangup) leaves no ghost behind.
//!
//! Deliberately in memory only. The registry describes sockets, and sockets do
//! not survive the app; after a restart it is empty and simply refills as
//! clients reconnect. And deliberately not the event log: who is connected is
//! the user's business, not something to announce to every agent listening on
//! `wait_for_updates` — the same reasoning that keeps run state on its own
//! channel.

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// How long after its last contact an agent still appears at all. An agent in
/// the poll loop spends most of its life connected, but the moments it is out
/// reacting to what came back are exactly the moments it is working — five
/// minutes (the same figure as the room give-up default) keeps it on the list
/// through that.
const LINGER: Duration = Duration::from_secs(300);

/// Silence longer than this counts as having been away, so the contact that
/// ends it is worth announcing. Anything shorter is the ordinary breathing of
/// the poll loop and would only make the UI flicker.
const AWAY: Duration = Duration::from_secs(60);

pub struct Presence {
    inner: Mutex<Inner>,
    /// Fires when the set of connections changes or somebody reappears. The UI
    /// refetches on it; nothing else listens.
    pub changed: broadcast::Sender<()>,
}

#[derive(Default)]
struct Inner {
    next_id: u64,
    conns: HashMap<u64, Conn>,
    /// Last contact per agent: the RFC3339 string is for display, the Instant
    /// for arithmetic — parsing the string back would be a second clock.
    seen: HashMap<i64, (String, Instant)>,
}

struct Conn {
    agent_id: i64,
    kind: &'static str,
    since: String,
}

/// One live connection's `{kind, since}`, as the UI sees it.
#[derive(Serialize, Clone)]
pub struct ConnInfo {
    pub kind: &'static str,
    pub since: String,
}

/// Everything the registry knows about one agent, before the store joins it
/// against who that agent actually is.
pub struct AgentPresence {
    pub agent_id: i64,
    pub connections: Vec<ConnInfo>,
    pub last_seen: String,
}

/// A presence row the UI can render: the registry's facts joined with the
/// agent's identity and — because an agent listens to exactly one project —
/// what it is listening to.
#[derive(Serialize)]
pub struct ConnectedAgent {
    pub agent_id: i64,
    pub name: String,
    pub icon: String,
    pub color: String,
    pub profile_label: Option<String>,
    pub project_id: i64,
    pub project_name: String,
    pub project_color: String,
    pub folder_path: String,
    /// Room names, because the rooms are what it hears.
    pub rooms: Vec<String>,
    pub connections: Vec<ConnInfo>,
    pub last_seen: String,
}

/// A held place in the registry. Dropping it is the disconnect.
pub struct ConnGuard {
    presence: Arc<Presence>,
    id: u64,
    agent_id: i64,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

impl Presence {
    pub fn new() -> Arc<Self> {
        let (changed, _) = broadcast::channel(64);
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            changed,
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // Same stance as the store: a poisoned lock means someone panicked
        // mid-update, and the map is still worth more than the whole app.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers a hold on the listener. Keep the guard for as long as the
    /// connection is real; dropping it is what says goodbye.
    pub fn connect(self: &Arc<Self>, agent_id: i64, kind: &'static str) -> ConnGuard {
        let stamp = now();
        let mut inner = self.lock();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.conns.insert(
            id,
            Conn { agent_id, kind, since: stamp.clone() },
        );
        inner.seen.insert(agent_id, (stamp, Instant::now()));
        drop(inner);
        let _ = self.changed.send(());
        ConnGuard { presence: self.clone(), id, agent_id }
    }

    /// Any authenticated request counts as contact, even one that holds
    /// nothing open. Only a reappearance is announced — announcing every tool
    /// call would have the UI refetching on each one for no visible change.
    pub fn touch(&self, agent_id: i64) {
        let mut inner = self.lock();
        let reappeared = match inner.seen.get(&agent_id) {
            Some((_, at)) => {
                at.elapsed() > AWAY && !inner.conns.values().any(|c| c.agent_id == agent_id)
            }
            None => true,
        };
        inner.seen.insert(agent_id, (now(), Instant::now()));
        drop(inner);
        if reappeared {
            let _ = self.changed.send(());
        }
    }

    /// The registry as of now, grouped by agent. Prunes as it reads: an entry
    /// with no live connection and nothing heard for `LINGER` is gone, which
    /// is the only decay this data ever needs.
    pub fn snapshot(&self) -> Vec<AgentPresence> {
        let mut inner = self.lock();
        let live: HashSet<i64> = inner.conns.values().map(|c| c.agent_id).collect();
        inner
            .seen
            .retain(|id, (_, at)| live.contains(id) || at.elapsed() < LINGER);

        let mut by_agent: HashMap<i64, AgentPresence> = inner
            .seen
            .iter()
            .map(|(&agent_id, (iso, _))| {
                (
                    agent_id,
                    AgentPresence { agent_id, connections: Vec::new(), last_seen: iso.clone() },
                )
            })
            .collect();
        for c in inner.conns.values() {
            by_agent
                .entry(c.agent_id)
                .or_insert_with(|| AgentPresence {
                    agent_id: c.agent_id,
                    connections: Vec::new(),
                    last_seen: c.since.clone(),
                })
                .connections
                .push(ConnInfo { kind: c.kind, since: c.since.clone() });
        }
        let mut rows: Vec<AgentPresence> = by_agent.into_values().collect();
        for row in &mut rows {
            // HashMap order is noise; oldest hold first is a fact.
            row.connections.sort_by(|a, b| a.since.cmp(&b.since));
        }
        rows
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        let mut inner = self.presence.lock();
        inner.conns.remove(&self.id);
        // The disconnect is itself the freshest contact.
        inner.seen.insert(self.agent_id, (now(), Instant::now()));
        drop(inner);
        let _ = self.presence.changed.send(());
    }
}
