//! Turya CLI host: registry bootstrap, command surface, and the internal
//! plugins the binary wires together. A library target exists so the live
//! test harness (`tests/live.rs`) and future command modules can be tested
//! directly instead of only through a spawned process.

pub mod auth_cmd;
pub mod host_services;
pub mod live;
pub mod update;
