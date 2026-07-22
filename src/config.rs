//! TOML configuration for the `malkuth daemon` supervisor.
//!
//! ```toml
//! [daemon]
//! host = "127.0.0.1"
//!
//! [[services]]
//! id = "chest"
//! kind = "backend"
//! program = "/path/to/chest"
//! restart_policy = "permanent"
//!
//! [services.env]
//! KEY = "value"
//! ```

use std::collections::HashMap;

use serde::Deserialize;

use crate::RestartPolicy;
use crate::worker::WorkerSpec;

#[derive(Debug, Deserialize)]
pub struct DaemonConfig {
    #[serde(default)]
    pub daemon: DaemonSettings,
    #[serde(default)]
    pub services: Vec<ServiceDef>,
}

#[derive(Debug, Deserialize)]
pub struct DaemonSettings {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_rate_limit_window")]
    pub rate_limit_window_secs: u64,
    #[serde(default = "default_rate_limit_max")]
    pub rate_limit_max_restarts: u32,
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
    #[serde(default)]
    pub pid_file: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ServiceDef {
    pub id: String,
    #[serde(default)]
    pub kind: String,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub restart_policy: PolicyDef,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub working_dir: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyDef {
    #[default]
    Permanent,
    Transient,
    Temporary,
}

fn default_host() -> String { "127.0.0.1".into() }
fn default_rate_limit_window() -> u64 { 60 }
fn default_rate_limit_max() -> u32 { 5 }
fn default_cooldown() -> u64 { 30 }

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            host: default_host(),
            rate_limit_window_secs: default_rate_limit_window(),
            rate_limit_max_restarts: default_rate_limit_max(),
            cooldown_secs: default_cooldown(),
            pid_file: None,
        }
    }
}

impl DaemonConfig {
    pub fn from_file(path: &str) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read config {}: {}", path, e))?;
        toml::from_str(&content)
            .map_err(|e| format!("invalid TOML in {}: {}", path, e))
    }

    pub fn into_worker_specs(self) -> Vec<WorkerSpec> {
        self.services.into_iter().map(|svc| {
            let policy = match svc.restart_policy {
                PolicyDef::Permanent => RestartPolicy::Permanent,
                PolicyDef::Transient => RestartPolicy::Transient,
                PolicyDef::Temporary => RestartPolicy::Temporary,
            };
            let env: Vec<(String, String)> = svc.env.into_iter().collect();
            WorkerSpec {
                id: svc.id,
                kind: if svc.kind.is_empty() { "service".into() } else { svc.kind },
                program: svc.program,
                args: svc.args,
                env,
                restart_policy: policy,
            }
        }).collect()
    }
}
