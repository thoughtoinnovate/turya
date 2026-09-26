use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::{Mutex, RwLock};
use turya_protocol::{PermissionDecision, PermissionMode, RiskLevel};

pub struct PermissionBroker {
    mode: RwLock<PermissionMode>,
    allowed_session_tools: RwLock<HashSet<String>>,
}

impl PermissionBroker {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode: RwLock::new(mode),
            allowed_session_tools: RwLock::new(HashSet::new()),
        }
    }

    pub fn set_mode(&self, new_mode: PermissionMode) {
        let mut mode = self.mode.write().unwrap();
        *mode = new_mode;
    }

    /// Evaluates if an action is pre-authorized or requires user challenge
    pub fn check(&self, tool_name: &str, risk: RiskLevel) -> Option<PermissionDecision> {
        let mode = *self.mode.read().unwrap();
        if mode == PermissionMode::Open {
            return Some(PermissionDecision::AllowOnce);
        }

        if self
            .allowed_session_tools
            .read()
            .unwrap()
            .contains(tool_name)
        {
            return Some(PermissionDecision::AllowOnce);
        }

        if mode == PermissionMode::ReviewForMe && risk == RiskLevel::Low {
            return Some(PermissionDecision::AllowOnce);
        }

        // None indicates the engine must issue PermissionRequested event
        None
    }

    pub fn record_decision(&self, tool_name: &str, decision: PermissionDecision) {
        if decision == PermissionDecision::AllowSession {
            self.allowed_session_tools
                .write()
                .unwrap()
                .insert(tool_name.to_string());
        }
    }
}

/// Routes a user's decision to the one waiter that asked for it.
///
/// This exists because a single shared receiver cannot serve concurrent
/// waiters. The old code drained the channel and threw away any answer whose
/// id was not its own, which is harmless for one outstanding request and
/// catastrophic for five: each child would swallow the other four's answers
/// and then block forever. Nothing is consumed destructively here - every
/// decision goes to the waiter registered under its id, and an answer nobody
/// is waiting for is reported rather than silently dropped.
pub struct PermissionRouter {
    pending: Mutex<HashMap<String, tokio::sync::oneshot::Sender<PermissionDecision>>>,
    /// Answers that arrived for a request nobody was waiting on. Counted so a
    /// mismatch is visible instead of being a silent hang.
    orphaned: Mutex<u64>,
    /// Set when the decision channel has closed, so nobody can arrive after
    /// the answer route is gone.
    ///
    /// Without this there is a window: the dispatcher sees the close, clears
    /// an empty map and exits, and a request registering a moment later waits
    /// on a oneshot that will never be sent. That is a hang with no error,
    /// and it is reachable in production - `Esc` drops the sender mid-turn.
    closed: std::sync::atomic::AtomicBool,
}

impl Default for PermissionRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionRouter {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            orphaned: Mutex::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Register interest in a request, returning the receiver to await.
    ///
    /// If an answer for this id somehow already arrived, it is delivered
    /// immediately rather than lost.
    pub fn register(
        &self,
        request_id: &str,
    ) -> (
        tokio::sync::oneshot::Receiver<PermissionDecision>,
        Option<PermissionDecision>,
    ) {
        // Checked before inserting, and the dispatcher sets the flag before
        // it clears, so a late arrival either short-circuits here or is
        // cleared out of the map. Either way it is denied, never stranded.
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            let (tx, rx) = tokio::sync::oneshot::channel();
            drop(tx);
            return (rx, None);
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .pending
            .lock()
            .unwrap()
            .insert(request_id.to_string(), tx)
            .is_some()
        {
            // A duplicate id would mean one waiter silently orphaned another.
            *self.orphaned.lock().unwrap() += 1;
        }
        (rx, None)
    }

    /// Deliver a decision. Returns false when nobody is waiting for it.
    pub fn resolve(&self, request_id: &str, decision: PermissionDecision) -> bool {
        match self.pending.lock().unwrap().remove(request_id) {
            Some(tx) => tx.send(decision).is_ok(),
            None => {
                *self.orphaned.lock().unwrap() += 1;
                false
            }
        }
    }

    /// Drop a waiter that gave up, so the map cannot grow without bound.
    pub fn forget(&self, request_id: &str) {
        self.pending.lock().unwrap().remove(request_id);
    }

    pub fn outstanding(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    /// Answers delivered to nobody. Non-zero means a routing bug.
    pub fn orphaned(&self) -> u64 {
        *self.orphaned.lock().unwrap()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Owns the session's decision channel and fans answers out by request id.
///
/// One of these exists per turn. Every task in the turn - the parent and any
/// number of subagents - awaits its own oneshot, so a decision reaches the
/// right waiter no matter how many are in flight or in what order the user
/// answers them.
pub fn spawn_permission_dispatcher(
    mut rx: tokio::sync::mpsc::Receiver<(String, PermissionDecision)>,
    router: std::sync::Arc<PermissionRouter>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some((id, decision)) = rx.recv().await {
            router.resolve(&id, decision);
        }
        // The channel closed: flag first, then clear, so nobody can register
        // into a map that is about to be emptied. Anyone still waiting sees
        // their oneshot fail and denies rather than hanging.
        router
            .closed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        router.pending.lock().unwrap().clear();
    })
}

#[cfg(test)]
mod router_tests {
    use super::*;

    #[tokio::test]
    async fn five_waiters_each_get_their_own_answer() {
        // The case that used to be impossible: five concurrent requests, five
        // answers, arriving in an order nobody chose.
        let router = std::sync::Arc::new(PermissionRouter::new());
        let mut waiters = Vec::new();
        for i in 0..5 {
            let (rx, _) = router.register(&format!("req-{i}"));
            waiters.push((format!("req-{i}"), rx));
        }
        assert_eq!(router.outstanding(), 5);

        // Answered back to front.
        for (id, _) in waiters.iter().rev() {
            assert!(router.resolve(id, PermissionDecision::AllowOnce));
        }
        for (id, rx) in waiters {
            assert_eq!(
                rx.await.expect("every waiter must be answered"),
                PermissionDecision::AllowOnce,
                "{id} got someone else's answer"
            );
        }
        assert_eq!(router.orphaned(), 0);
        assert_eq!(router.outstanding(), 0);
    }

    #[tokio::test]
    async fn an_answer_for_nobody_is_counted_not_dropped_silently() {
        let router = PermissionRouter::new();
        assert!(!router.resolve("ghost", PermissionDecision::Deny));
        assert_eq!(router.orphaned(), 1);
    }

    #[tokio::test]
    async fn a_forgotten_waiter_does_not_leak() {
        let router = PermissionRouter::new();
        router.register("a");
        assert_eq!(router.outstanding(), 1);
        router.forget("a");
        assert_eq!(router.outstanding(), 0);
    }

    #[tokio::test]
    async fn a_request_registered_after_the_channel_closes_is_denied_not_stranded() {
        // The window that made a subagent hang forever: the dispatcher is
        // gone, and a request arrives anyway. It must be denied immediately.
        let router = std::sync::Arc::new(PermissionRouter::new());
        let (tx, rx) = tokio::sync::mpsc::channel::<(String, PermissionDecision)>(1);
        let handle = spawn_permission_dispatcher(rx, router.clone());
        drop(tx);
        let _ = handle.await;
        assert!(router.is_closed());

        let (wait, _) = router.register("late");
        // Must resolve, not hang: the sender was dropped, so this is an Err,
        // which the engine reads as a denial.
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), wait)
                .await
                .is_ok()
        );
        assert_eq!(router.outstanding(), 0);
    }

    #[tokio::test]
    async fn the_dispatcher_routes_a_queued_answer() {
        let router = std::sync::Arc::new(PermissionRouter::new());
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let handle = spawn_permission_dispatcher(rx, router.clone());
        let (wait, _) = router.register("q1");
        tx.send(("q1".to_string(), PermissionDecision::AllowSession))
            .await
            .unwrap();
        assert_eq!(wait.await.unwrap(), PermissionDecision::AllowSession);
        drop(tx);
        let _ = handle.await;
    }
}
