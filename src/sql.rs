//! Read-only SQL over the run index. `brief` and `query` answer the usual questions; this is
//! the entry point for the ones they do not, without printing a whole run as JSON.
//!
//! The index is rebuilt from the run directories, so a query here never changes a record.
use crate::config::LoadedConfig;
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde::Serialize;
use serde_json::{Map, Value};
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
    pub row_count: usize,
    /// True when `--limit` cut the result; the query itself decides what is interesting.
    pub truncated: bool,
    pub rows: Vec<Value>,
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

pub fn query(config: &LoadedConfig, sql: &str, limit: usize) -> Result<SqlOutput> {
    let connection = open(config)?;
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
    let mut truncated = false;
    let mut cursor = statement.query([])?;
    while let Some(row) = cursor.next()? {
        if rows.len() >= limit {
            truncated = true;
            break;
        }
        let mut object = Map::new();
        for (index, column) in columns.iter().enumerate() {
            object.insert(column.clone(), value_of(row.get_ref(index)?));
        }
        rows.push(Value::Object(object));
    }
    Ok(SqlOutput {
        schema_version: 1,
        row_count: rows.len(),
        columns,
        truncated,
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
        let cells = output
            .columns
            .iter()
            .map(|column| match row.get(column) {
                Some(Value::Null) | None => String::new(),
                Some(Value::String(text)) => text.replace(['\t', '\n'], " "),
                Some(value) => value.to_string(),
            })
            .collect::<Vec<_>>();
        writeln!(writer, "{}", cells.join("\t"))?;
    }
    if output.truncated {
        writeln!(writer, "# truncated at {} rows", output.row_count)?;
    }
    Ok(())
}
