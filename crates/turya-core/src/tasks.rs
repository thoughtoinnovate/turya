//! Supervised task registry (Rule 3.1: loop bookkeeping, no I/O, no providers).
//!
//! The engine runs turns as Tokio tasks, and a turn may itself spawn child
//! tasks (subagents, D3). A single `Option<JoinHandle>` cannot express that
//! shape: aborting a parent would leave children running. This registry owns
//! every live task, knows each one's parent, and guarantees that
//! `Esc`/`Ctrl+C` tears down a whole tree instead of orphaning work.
//!
//! Deliberately *not* a cancellation-token framework: `JoinHandle::abort`
//! is the one primitive that works for a task blocked on any future
//! (network, channel, process) without cooperative checkpoints inside them.

use std::collections::HashMap;
use std::time::Duration;
use tokio::task::JoinHandle;

/// Stable identifier for a live task. The engine mints these; the protocol
/// carries them so the UI can name a running turn or subagent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskId(pub String);

/// What a task is, for diagnostics and for tree rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// The top-level turn driven by a user prompt.
    Root,
    /// A nested task (subagent) with its own history and budget.
    Subagent,
}

#[derive(Debug)]
struct Entry {
    kind: TaskKind,
    parent: Option<TaskId>,
    handle: JoinHandle<()>,
}

/// How long `drain_all` waits for tasks to notice their abort before it
/// drops the handles. Aborting a Tokio task is not instantaneous: the task
/// must reach an await point. A short bound keeps shutdown predictable
/// instead of hanging on a wedged network call.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct TaskRegistry {
    map: HashMap<TaskId, Entry>,
}

impl TaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a running task. Returns its id so the caller can reference it
    /// in emitted events.
    pub fn insert(
        &mut self,
        id: &str,
        kind: TaskKind,
        parent: Option<&TaskId>,
        handle: JoinHandle<()>,
    ) -> TaskId {
        let tid = TaskId(id.to_string());
        self.map.insert(
            tid.clone(),
            Entry {
                kind,
                parent: parent.cloned(),
                handle,
            },
        );
        tid
    }

    pub fn is_alive(&self, id: &TaskId) -> bool {
        self.map.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn parent_of(&self, id: &TaskId) -> Option<TaskId> {
        self.map.get(id).and_then(|e| e.parent.clone())
    }

    pub fn kind_of(&self, id: &TaskId) -> Option<TaskKind> {
        self.map.get(id).map(|e| e.kind)
    }

    /// Ids of `id` and every descendant, in breadth-first order. Used by
    /// `abort_tree` so killing a subagent takes its own subagents with it.
    pub fn tree(&self, id: &TaskId) -> Vec<TaskId> {
        let mut out = vec![id.clone()];
        let mut i = 0;
        while i < out.len() {
            let current = out[i].clone();
            for (tid, entry) in &self.map {
                if entry.parent.as_ref() == Some(&current) && !out.contains(tid) {
                    out.push(tid.clone());
                }
            }
            i += 1;
        }
        out
    }

    /// Abort one task only, leaving its children running.
    pub fn abort(&mut self, id: &TaskId) -> bool {
        match self.map.remove(id) {
            Some(entry) => {
                entry.handle.abort();
                true
            }
            None => false,
        }
    }

    /// Abort `id` and every descendant. This is the turn-abort path: the user
    /// pressed `Esc`, so nothing they started may outlive the request.
    pub fn abort_tree(&mut self, id: &TaskId) -> usize {
        let tree = self.tree(id);
        let mut killed = 0;
        for tid in tree.into_iter().rev() {
            if self.abort(&tid) {
                killed += 1;
            }
        }
        killed
    }

    /// Abort everything. Used on shutdown and when a new prompt supersedes
    /// stale work.
    pub fn abort_all(&mut self) -> usize {
        let ids: Vec<TaskId> = self.map.keys().cloned().collect();
        let mut killed = 0;
        for id in ids.into_iter().rev() {
            if self.abort(&id) {
                killed += 1;
            }
        }
        killed
    }

    /// Drop finished tasks and reap their results. Call between turns: a task
    /// that already returned must not keep its (possibly large) frame alive.
    pub fn reap(&mut self) {
        self.map.retain(|_, entry| !entry.handle.is_finished());
    }

    /// Abort everything and wait, bounded, for the tasks to actually stop.
    /// Without the wait, a task killed mid-write can be observed after the
    /// caller believes shutdown completed.
    pub async fn drain_all(&mut self) {
        let mut handles = Vec::new();
        for (_, entry) in self.map.drain() {
            entry.handle.abort();
            handles.push(entry.handle);
        }
        if handles.is_empty() {
            return;
        }
        let _ = tokio::time::timeout(DRAIN_GRACE, async {
            for handle in handles {
                let _ = handle.await;
            }
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::sync::Notify;

    /// A task that parks until `gate` fires, so tests control its lifetime.
    async fn parked(started: Arc<AtomicBool>, gate: Arc<Notify>) {
        started.store(true, Ordering::SeqCst);
        gate.notified().await;
    }

    #[tokio::test]
    async fn abort_stops_one_task_and_reports_it() {
        let mut reg = TaskRegistry::new();
        let started = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(Notify::new());
        let id = reg.insert(
            "t1",
            TaskKind::Root,
            None,
            tokio::spawn(parked(started.clone(), gate.clone())),
        );
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        assert!(reg.is_alive(&id));
        assert!(reg.abort(&id));
        assert!(!reg.is_alive(&id));
        assert!(!reg.abort(&id), "second abort is a no-op");
    }

    #[tokio::test]
    async fn abort_tree_kills_children_and_grandchildren() {
        let mut reg = TaskRegistry::new();
        let started = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(Notify::new());
        let root = reg.insert(
            "root",
            TaskKind::Root,
            None,
            tokio::spawn(parked(started.clone(), gate.clone())),
        );
        let child = reg.insert(
            "child",
            TaskKind::Subagent,
            Some(&root),
            tokio::spawn(parked(started.clone(), gate.clone())),
        );
        let grandchild = reg.insert(
            "gc",
            TaskKind::Subagent,
            Some(&child),
            tokio::spawn(parked(started.clone(), gate.clone())),
        );
        let sibling = reg.insert(
            "other",
            TaskKind::Root,
            None,
            tokio::spawn(parked(started.clone(), gate.clone())),
        );
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }

        assert_eq!(reg.tree(&root).len(), 3, "root + child + grandchild");
        assert_eq!(reg.parent_of(&child).as_ref(), Some(&root));
        assert_eq!(reg.kind_of(&child), Some(TaskKind::Subagent));

        assert_eq!(reg.abort_tree(&root), 3);
        assert!(!reg.is_empty(), "the unrelated root survives");
        assert!(reg.is_alive(&sibling));
        assert!(!reg.is_alive(&grandchild));
    }

    #[tokio::test]
    async fn abort_all_empties_the_registry() {
        let mut reg = TaskRegistry::new();
        let started = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(Notify::new());
        for name in ["a", "b", "c"] {
            reg.insert(
                name,
                TaskKind::Root,
                None,
                tokio::spawn(parked(started.clone(), gate.clone())),
            );
        }
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        assert_eq!(reg.len(), 3);
        assert_eq!(reg.abort_all(), 3);
        assert!(reg.is_empty());
    }

    #[tokio::test]
    async fn drain_all_aborts_and_waits() {
        let mut reg = TaskRegistry::new();
        let started = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(Notify::new());
        reg.insert(
            "t",
            TaskKind::Root,
            None,
            tokio::spawn(parked(started.clone(), gate.clone())),
        );
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        reg.drain_all().await;
        assert!(reg.is_empty());
    }

    #[tokio::test]
    async fn reap_drops_finished_tasks_only() {
        let mut reg = TaskRegistry::new();
        let started = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(Notify::new());
        reg.insert("done", TaskKind::Root, None, tokio::spawn(async {}));
        reg.insert(
            "live",
            TaskKind::Root,
            None,
            tokio::spawn(parked(started.clone(), gate.clone())),
        );
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        reg.reap();
        assert_eq!(reg.len(), 1, "the finished task is reaped");
        assert!(reg.is_alive(&TaskId("live".to_string())));
    }
}
