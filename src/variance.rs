//! How much the score moves when nothing changed: `isuscope variance`.
//!
//! Runs that measured the same configuration (`runs.config_key`, see
//! [`crate::repeat::config_key`]) are repeated measurements of one value. For each run, the
//! spread is the pooled relative standard deviation of those groups as they stood when the run
//! started, so it is the variation that was known then. Only facts are reported; what counts
//! as a real difference is left to the reader.

use crate::{runner::short_id, storage::Store};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
pub struct VarianceRow {
    pub run: String,
    /// First 12 hex digits of the configuration key; absent when the run has none.
    pub config_key: Option<String>,
    pub score: i64,
    pub passed: Option<bool>,
    /// Measured with `--repeat`.
    pub repeat: bool,
    /// Pooled relative standard deviation over the configurations with at least two measured
    /// runs strictly before this one: `sqrt(Σ_c Σ_i (score_i / mean_c - 1)² / Σ_c (n_c - 1))`.
    pub spread: Option<f64>,
    /// How many configurations and runs `spread` came from.
    pub spread_configs: usize,
    pub spread_runs: usize,
    #[serde(skip)]
    pub id: String,
    #[serde(skip)]
    full_config_key: Option<String>,
    #[serde(skip)]
    measured: bool,
}

/// One run of the configuration `variance --run` shows.
#[derive(Debug, Serialize)]
pub struct ConfigRun {
    pub run: String,
    pub score: i64,
    pub passed: Option<bool>,
    pub repeat: bool,
}

/// Every finished run with a score, oldest first.
pub fn rows(store: &Store) -> Result<Vec<VarianceRow>> {
    let mut statement = store.connection.prepare(
        "SELECT id, config_key, score, passed, state, repeat_reason IS NOT NULL
         FROM runs WHERE state != 'running' AND score IS NOT NULL ORDER BY started_at, id",
    )?;
    let runs = statement
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let config_key: Option<String> = row.get(1)?;
            let passed: Option<bool> = row.get(3)?;
            let state: String = row.get(4)?;
            Ok(VarianceRow {
                run: short_id(&id).to_owned(),
                config_key: config_key.as_deref().map(crate::storage::short_config_key),
                score: row.get(2)?,
                passed,
                repeat: row.get(5)?,
                spread: None,
                spread_configs: 0,
                spread_runs: 0,
                // A measurement: the benchmark ran to its end and passed (`pending` and
                // interrupted runs have other states).
                measured: passed == Some(true)
                    && matches!(state.as_str(), "complete" | "degraded")
                    && config_key.is_some(),
                full_config_key: config_key,
                id,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(with_spread(runs))
}

fn with_spread(mut runs: Vec<VarianceRow>) -> Vec<VarianceRow> {
    let mut scores = BTreeMap::<String, Vec<f64>>::new();
    for run in &mut runs {
        let (spread, configs, count) = pooled(&scores);
        run.spread = spread;
        run.spread_configs = configs;
        run.spread_runs = count;
        if run.measured
            && let Some(key) = &run.full_config_key
        {
            scores
                .entry(key.clone())
                .or_default()
                .push(run.score as f64);
        }
    }
    runs
}

/// The pooled relative standard deviation, the configurations and the runs it used.
fn pooled(scores: &BTreeMap<String, Vec<f64>>) -> (Option<f64>, usize, usize) {
    let (mut squares, mut freedom, mut configs, mut runs) = (0.0, 0usize, 0, 0);
    for values in scores.values().filter(|values| values.len() >= 2) {
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        if mean == 0.0 {
            continue;
        }
        squares += values
            .iter()
            .map(|value| (value / mean - 1.0).powi(2))
            .sum::<f64>();
        freedom += values.len() - 1;
        configs += 1;
        runs += values.len();
    }
    let spread = (freedom > 0).then(|| (squares / freedom as f64).sqrt());
    (spread, configs, runs)
}

/// The runs that measured the same configuration as `row`, oldest first.
pub fn config_runs(rows: &[VarianceRow], row: &VarianceRow) -> Vec<ConfigRun> {
    let Some(key) = &row.full_config_key else {
        return Vec::new();
    };
    rows.iter()
        .filter(|other| other.full_config_key.as_ref() == Some(key))
        .map(|other| ConfigRun {
            run: other.run.clone(),
            score: other.score,
            passed: other.passed,
            repeat: other.repeat,
        })
        .collect()
}

/// The column names above [`describe`]'s lines.
pub fn header() -> String {
    format!(
        "{:<8}  {:<12}  {:>8}  {:<4}  {:<6}  spread",
        "run", "config_key", "score", "pass", "repeat"
    )
}

/// One line per run for the terminal.
pub fn describe(row: &VarianceRow) -> String {
    let spread = match row.spread {
        Some(spread) => format!(
            "{:.2}% ({} configs, {} runs)",
            spread * 100.0,
            row.spread_configs,
            row.spread_runs
        ),
        None => "-".into(),
    };
    format!(
        "{}  {:<12}  {:>8}  {:<4}  {:<6}  {spread}",
        row.run,
        row.config_key.as_deref().unwrap_or("-"),
        row.score,
        match row.passed {
            Some(true) => "PASS",
            Some(false) => "FAIL",
            None => "-",
        },
        if row.repeat { "repeat" } else { "-" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, score: i64, measured: bool) -> VarianceRow {
        VarianceRow {
            run: format!("{key}{score}"),
            config_key: Some(key.into()),
            score,
            passed: Some(measured),
            repeat: false,
            spread: None,
            spread_configs: 0,
            spread_runs: 0,
            id: String::new(),
            full_config_key: Some(key.into()),
            measured,
        }
    }

    #[test]
    fn the_spread_pools_configurations_measured_before_each_run() {
        let rows = with_spread(vec![
            row("a", 90, true),
            row("a", 110, true),
            row("b", 200, true),
            // A failed run is not a measurement.
            row("b", 10, false),
            row("b", 200, true),
            row("c", 50, true),
        ]);
        assert_eq!(rows[0].spread, None);
        assert_eq!(rows[1].spread, None);
        // a: mean 100, deviations ±0.1, one degree of freedom.
        let expected = (0.02f64 / 1.0).sqrt();
        assert!((rows[2].spread.unwrap() - expected).abs() < 1e-12);
        assert_eq!((rows[2].spread_configs, rows[2].spread_runs), (1, 2));
        assert_eq!((rows[4].spread_configs, rows[4].spread_runs), (1, 2));
        // b adds two equal scores: same squares, one more degree of freedom.
        let pooled = (0.02f64 / 2.0).sqrt();
        assert!((rows[5].spread.unwrap() - pooled).abs() < 1e-12);
        assert_eq!((rows[5].spread_configs, rows[5].spread_runs), (2, 4));
        assert_eq!(config_runs(&rows, &rows[3]).len(), 3);
    }
}
