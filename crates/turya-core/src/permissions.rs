use std::collections::HashSet;
use std::sync::RwLock;
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
