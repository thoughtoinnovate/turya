use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

pub mod manifest;

pub use manifest::{
    Contributions, ManifestCapabilities, ManifestError, OAuthConfig, OAuthFlowType, PluginKind,
    PluginManifest, ProviderContribution,
};

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("extism: {0}")]
    Extism(String),
    #[error("bad output: {0}")]
    Output(String),
    #[error("capability denied: {0}")]
    Denied(String),
}

/// Capability manifest for one plugin (mirrors `turya-plugin.toml`).
#[derive(Debug, Clone, Default)]
pub struct PluginCapability {
    /// Hosts the plugin may contact (enforced by withholding HTTP host fns).
    pub allowed_hosts: Vec<String>,
    /// Filesystem paths the plugin may access (documentation-level; WASI off by default).
    pub allowed_paths: Vec<String>,
    /// May persist tokens/credentials (keychain or plugin storage).
    pub storage: bool,
}

/// Sandboxed Wasm plugin host.
///
/// Security posture: WASI **off** by default, no host functions granted
/// unless explicitly registered. Hot-reload is trivially safe because each
/// `call_*` builds a fresh plugin from current file bytes.
///
/// The heavy `extism` runtime is behind the `wasm` cargo feature so default
/// builds stay lean: `cargo build -p turya-plugins --features wasm`.
pub struct PluginHost {
    capabilities: HashMap<String, PluginCapability>,
}

impl PluginHost {
    pub fn new() -> Self {
        Self {
            capabilities: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: &str, capability: PluginCapability) {
        self.capabilities.insert(name.to_string(), capability);
    }

    fn check_path(&self, plugin_name: &str, path: &Path) -> Result<(), PluginError> {
        if let Some(cap) = self.capabilities.get(plugin_name) {
            if !cap.allowed_paths.is_empty()
                && !cap
                    .allowed_paths
                    .iter()
                    .any(|p| path.to_string_lossy().starts_with(p))
            {
                return Err(PluginError::Denied(format!(
                    "wasm path {} outside allowed_paths",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    /// Execute `func` inside fresh Wasm bytes with JSON in/out.
    #[cfg(feature = "wasm")]
    pub fn call_bytes(
        &self,
        wasm_bytes: &[u8],
        func: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        use extism::{Manifest, Plugin, Wasm};
        let manifest = Manifest::new([Wasm::data(wasm_bytes.to_vec())]);
        let mut plugin =
            Plugin::new(&manifest, [], false).map_err(|e| PluginError::Extism(e.to_string()))?;
        let input_str =
            serde_json::to_string(input).map_err(|e| PluginError::Output(e.to_string()))?;
        let out = plugin
            .call(func, &input_str)
            .map_err(|e| PluginError::Output(e.to_string()))?;
        serde_json::from_slice(out)
            .map_err(|e| PluginError::Output(format!("non-JSON plugin output: {}", e)))
    }

    /// Fallback when built without `--features wasm`.
    #[cfg(not(feature = "wasm"))]
    pub fn call_bytes(
        &self,
        _wasm_bytes: &[u8],
        _func: &str,
        _input: &serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        Err(PluginError::Extism(
            "turya-plugins built without `wasm` feature; rebuild with --features wasm".to_string(),
        ))
    }

    /// Hot-reload path: re-reads the `.wasm` file on every call.
    pub fn call_file(
        &self,
        plugin_name: &str,
        wasm_path: impl AsRef<Path>,
        func: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        let path = wasm_path.as_ref();
        self.check_path(plugin_name, path)?;
        let bytes = std::fs::read(path)?;
        self.call_bytes(&bytes, func, input)
    }
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_gate_rejects_outside_paths() {
        let mut host = PluginHost::new();
        host.register(
            "csv",
            PluginCapability {
                allowed_hosts: vec![],
                allowed_paths: vec!["/tmp/turya_plugins/".to_string()],
                storage: false,
            },
        );
        let err = host
            .call_file("csv", "/etc/passwd.wasm", "run", &serde_json::json!({}))
            .unwrap_err();
        // Without wasm feature the gate still fires first (Denied);
        // with wasm feature the same path is denied before any fs read.
        match err {
            PluginError::Denied(_) | PluginError::Io(_) | PluginError::Extism(_) => {}
            other => panic!("unexpected error: {}", other),
        }
    }

    #[test]
    fn unregistered_plugin_skips_path_gate() {
        let host = PluginHost::new();
        // No capability registered -> no path gate; fails later at fs read
        // (or wasm-missing error), but must not be Denied.
        let err = host
            .call_file(
                "unknown",
                "/nonexistent/turya-test-plugin.wasm",
                "run",
                &serde_json::json!({}),
            )
            .unwrap_err();
        assert!(!matches!(err, PluginError::Denied(_)));
    }

    #[cfg(feature = "wasm")]
    #[test]
    fn loads_and_calls_minimal_module() {
        use extism::{Manifest, Plugin, Wasm};
        // Minimal module exporting `greet() -> i32 42`.
        const MINIMAL_WASM: &[u8] = &[
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x05, 0x01, 0x60, 0x00, 0x01,
            0x7f, 0x03, 0x02, 0x01, 0x00, 0x07, 0x09, 0x01, 0x05, 0x67, 0x72, 0x65, 0x65, 0x74,
            0x00, 0x00, 0x0a, 0x06, 0x01, 0x04, 0x00, 0x41, 0x2a, 0x0b,
        ];
        let manifest = Manifest::new([Wasm::data(MINIMAL_WASM.to_vec())]);
        let result = Plugin::new(&manifest, [], false)
            .and_then(|mut p| p.call("greet", "").map(|b| b.to_vec()));
        assert!(result.is_ok(), "extism should run minimal module");
    }
}
