//! `MemoryHook` adapter: SQLite-backed implementation of the kernel seam.
//!
//! Internal plugin (`turya-memory`). Owns all rusqlite access plus the
//! reflection loop; the engine only ever sees plain strings through the trait.

use async_trait::async_trait;
use std::sync::Mutex;
use turya_core::MemoryHook;

pub struct SqliteMemoryHook {
    store: Mutex<super::MemoryStore>,
}

impl SqliteMemoryHook {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, super::MemoryError> {
        Ok(Self {
            store: Mutex::new(super::MemoryStore::open(path)?),
        })
    }

    pub fn open_in_memory() -> Result<Self, super::MemoryError> {
        Ok(Self {
            store: Mutex::new(super::MemoryStore::open_in_memory()?),
        })
    }
}

#[async_trait]
impl MemoryHook for SqliteMemoryHook {
    async fn recall_rules(&self, _session_id: &str, prompt: &str, limit: usize) -> Vec<String> {
        self.store
            .lock()
            .ok()
            .and_then(|store| store.rules_for(prompt, limit).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|r| r.rule)
            .collect()
    }

    async fn record_turn_completed(
        &self,
        session_id: &str,
        turn_id: &str,
        prompt: &str,
        success: bool,
    ) {
        if let Ok(store) = self.store.lock() {
            let _ = store.record_event(
                session_id,
                "TurnCompleted",
                &serde_json::json!({"turn_id": turn_id, "prompt": prompt, "success": success}),
            );
            // Reflection loop lives here (plugin side), never in the kernel.
            let _ = store.reflect_session(session_id);
        }
    }

    async fn record_tool_error(
        &self,
        session_id: &str,
        tool_name: &str,
        call_id: &str,
        error: &str,
    ) {
        if let Ok(store) = self.store.lock() {
            let _ = store.record_event(
                session_id,
                "ToolError",
                &serde_json::json!({"tool": tool_name, "call_id": call_id, "error": error}),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn adapter_roundtrip_recall_record_reflect() {
        let hook = SqliteMemoryHook::open_in_memory().unwrap();
        // Empty store recalls nothing.
        assert!(hook.recall_rules("s", "hello", 3).await.is_empty());
        // A recorded failure is distilled into a recallable rule.
        hook.record_tool_error("s", "run_bash", "c1", "Exited with code: Some(1)")
            .await;
        hook.record_turn_completed("s", "t1", "do it", true).await;
        let rules = hook.recall_rules("s", "retry run_bash", 3).await;
        assert!(rules.iter().any(|r| r.contains("run_bash")));
    }
}
