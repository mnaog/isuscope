use rusqlite::Connection;
use std::{fs, process::Command};
use tempfile::tempdir;

#[path = "e2e/adapters.rs"]
mod adapters;
#[path = "e2e/analysis.rs"]
mod analysis;
#[path = "e2e/codex_context.rs"]
mod codex_context;
#[path = "e2e/discovery.rs"]
mod discovery;
#[path = "e2e/doctor.rs"]
mod doctor;
#[path = "e2e/enrichment.rs"]
mod enrichment;
#[path = "e2e/failure_modes.rs"]
mod failure_modes;
#[path = "e2e/init.rs"]
mod init;
#[path = "e2e/lifecycle.rs"]
mod lifecycle;
#[path = "e2e/log_rotation.rs"]
mod log_rotation;
