//! `airmate usage`: what was used and what it cost, from the usage ledger.

use std::io::Write;

use serde_json::json;

use super::common::{load, CliError, Global, Process};
use crate::config::overrides::Overrides;
use crate::usage::{aggregate, parse_since, read_file, render_table, GroupBy};

/// How `usage` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// An aligned table.
    Table,
    /// One JSON document.
    Json,
}

fn io(e: std::io::Error) -> CliError {
    CliError::runtime(format!("cannot write output: {e}"))
}

/// Prints the ledger aggregated by `by`.
///
/// # Errors
///
/// An unreadable configuration or ledger, or a `--since` that does not parse.
pub fn usage(
    p: &Process,
    global: &Global,
    by: GroupBy,
    since: Option<&str>,
    format: Format,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let loaded = load(p, global, Overrides::default())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let since = since
        .map(|s| parse_since(s, now).map_err(|e| CliError::usage(format!("--since: {e}"))))
        .transpose()?;
    let path = loaded.dirs.usage_ledger();
    let records = read_file(&path).map_err(CliError::runtime)?;
    let rows = aggregate(&records, by, since);
    match format {
        Format::Json => {
            let doc = json!({ "ledger": path.display().to_string(), "rows": rows });
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&doc).unwrap_or_default()
            )
            .map_err(io)
        }
        Format::Table => {
            if rows.is_empty() {
                let why = if loaded.resolved.config.usage.ledger {
                    "nothing has been recorded in that period"
                } else {
                    "`usage.ledger` is off, so nothing is being recorded"
                };
                writeln!(out, "no usage: {why} ({})", path.display()).map_err(io)
            } else {
                write!(out, "{}", render_table(by, &rows)).map_err(io)
            }
        }
    }
}
