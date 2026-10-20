//! Read-only SQL over the run index. `brief` and `query` answer the usual questions; this is
//! the entry point for the ones they do not, without printing a whole run as JSON.
//!
//! The index is rebuilt from the run directories, so a query here never changes a record.
use crate::config::LoadedConfig;
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SqlFormat {
    Json,
    Tsv,
}

#[derive(Debug, Serialize)]
pub struct SqlOutput {
    pub schema_version: u32,
    pub columns: Vec<String>,
    /// Rows the statement returned, including the ones `--limit` left out (like `query`).
    pub total_count: usize,
    /// True when `--limit` cut the result; the query itself decides what is interesting.
    pub truncated: bool,
    /// Values in `columns` order, exactly as stored: no rounding, and REAL stays a float.
    pub rows: Vec<Vec<Value>>,
}

pub fn database_path(config: &LoadedConfig) -> std::path::PathBuf {
    config.data_dir.join("isuscope.sqlite3")
}

fn open(config: &LoadedConfig) -> Result<Connection> {
    let path = database_path(config);
    if !path.is_file() {
        anyhow::bail!(
            "{} does not exist yet; run `isuscope list` to rebuild the index from the saved runs",
            path.display()
        );
    }
    let connection = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open {}", path.display()))?;
    connection.pragma_update(None, "query_only", true)?;
    Ok(connection)
}

/// `CREATE` statements of the index, so a query can be written without reading isuscope's source.
pub fn schema(config: &LoadedConfig) -> Result<String> {
    let connection = open(config)?;
    let mut statement = connection
        .prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type DESC, name")?;
    let statements = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(statements.join(";\n") + ";\n")
}

/// 何も指定しなければ、1つのSQLに許す時間。相関サブクエリで行数の2乗になったSQLは、数万行で
/// 数分以上かかる（practice-14-seventhでは2分と5分待って手を止めた）。
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 30;

pub fn query(
    config: &LoadedConfig,
    sql: &str,
    limit: usize,
    timeout_seconds: u64,
) -> Result<SqlOutput> {
    run_limited(
        &open(config)?,
        sql,
        [],
        limit,
        timeout_seconds,
        "filter on the indexed columns shown by --schema",
    )
}

/// [`run`]を`timeout_seconds`（0なら無制限）で打ち切る。打ち切ったら、`hint`を添えて失敗する。
pub fn run_limited(
    connection: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    limit: usize,
    timeout_seconds: u64,
    hint: &str,
) -> Result<SqlOutput> {
    if timeout_seconds == 0 {
        return run(connection, sql, params, limit);
    }
    let handle = connection.get_interrupt_handle();
    let (finished, wait) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        let expired = matches!(
            wait.recv_timeout(std::time::Duration::from_secs(timeout_seconds)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        );
        if expired {
            handle.interrupt();
        }
        expired
    });
    let result = run(connection, sql, params, limit);
    let _ = finished.send(());
    let expired = watchdog.join().unwrap_or(false);
    match result {
        Err(_) if expired => anyhow::bail!(
            "the query was stopped after {timeout_seconds}s (--timeout-seconds; 0 for no limit); {hint}"
        ),
        result => result,
    }
}

/// `sql`を`connection`で実行し、`limit`行までを返す（`states`の一時的な索引も同じ形で出す）。
pub fn run(
    connection: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    limit: usize,
) -> Result<SqlOutput> {
    let mut statement = connection
        .prepare(sql)
        .with_context(|| format!("cannot prepare `{sql}`"))?;
    let columns = statement
        .column_names()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let mut unique = BTreeSet::new();
    if let Some(duplicate) = columns
        .iter()
        .find(|column| !unique.insert(column.as_str()))
    {
        anyhow::bail!(
            "SQL result contains duplicate column `{duplicate}`; use AS to give every selected column a unique name"
        );
    }
    let mut rows = Vec::new();
    let mut total_count = 0;
    let mut cursor = statement.query(params)?;
    while let Some(row) = cursor.next()? {
        total_count += 1;
        if rows.len() >= limit {
            continue;
        }
        rows.push(
            (0..columns.len())
                .map(|index| row.get_ref(index).map(value_of))
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    Ok(SqlOutput {
        schema_version: crate::model::OUTPUT_SCHEMA_VERSION,
        truncated: total_count > rows.len(),
        total_count,
        columns,
        rows,
    })
}

fn value_of(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(number) => Value::from(number),
        ValueRef::Real(number) => Value::from(number),
        ValueRef::Text(text) => Value::from(String::from_utf8_lossy(text).into_owned()),
        ValueRef::Blob(bytes) => Value::from(format!("<{} bytes>", bytes.len())),
    }
}

/// Tab separated rows with a header, for reading in a terminal without another tool.
pub fn write_tsv(output: &SqlOutput, mut writer: impl std::io::Write) -> Result<()> {
    writeln!(writer, "{}", output.columns.join("\t"))?;
    for row in &output.rows {
        let cells = row
            .iter()
            .map(|value| match value {
                Value::Null => String::new(),
                Value::String(text) => text.replace(['\t', '\n'], " "),
                value => value.to_string(),
            })
            .collect::<Vec<_>>();
        writeln!(writer, "{}", cells.join("\t"))?;
    }
    if output.truncated {
        writeln!(
            writer,
            "# showing {} of {} rows",
            output.rows.len(),
            output.total_count
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_over_the_time_limit_stops_with_the_hint() {
        let connection = Connection::open_in_memory().unwrap();
        let endless = "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n) \
                       SELECT COUNT(*) AS rows FROM n";
        let started = std::time::Instant::now();
        let error = run_limited(&connection, endless, [], 10, 1, "try an index").unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let message = error.to_string();
        assert!(message.contains("stopped after 1s"), "{message}");
        assert!(message.contains("try an index"), "{message}");
        // 止めた後も同じ接続で次のSQLを流せる。
        let output = run_limited(&connection, "SELECT 1 AS one", [], 10, 1, "").unwrap();
        assert_eq!(output.rows, [[Value::from(1)]]);
    }
}
