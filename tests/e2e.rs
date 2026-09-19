use rusqlite::Connection;
use std::{fs, process::Command};
use tempfile::tempdir;

#[path = "e2e/access_log.rs"]
mod access_log;
#[path = "e2e/adapters.rs"]
mod adapters;
#[path = "e2e/agent_context.rs"]
mod agent_context;
#[path = "e2e/analysis.rs"]
mod analysis;
#[path = "e2e/benchmark_messages.rs"]
mod benchmark_messages;
#[path = "e2e/brief_shape.rs"]
mod brief_shape;
#[path = "e2e/bucket_windows.rs"]
mod bucket_windows;
#[path = "e2e/changes.rs"]
mod changes;
#[path = "e2e/concurrency.rs"]
mod concurrency;
#[path = "e2e/database_views.rs"]
mod database_views;
#[path = "e2e/discovery.rs"]
mod discovery;
#[path = "e2e/doctor.rs"]
mod doctor;
#[path = "e2e/documented_commands.rs"]
mod documented_commands;
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
#[path = "e2e/node_disk.rs"]
mod node_disk;
#[path = "e2e/operation_lock.rs"]
mod operation_lock;
#[path = "e2e/parallel.rs"]
mod parallel;
#[path = "e2e/query.rs"]
mod query;
#[path = "e2e/score_inputs.rs"]
mod score_inputs;
#[path = "e2e/sql.rs"]
mod sql;
