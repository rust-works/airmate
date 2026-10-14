//! Drift detection: has a provider's API moved since airmate's field policy tables were written?
//!
//! The scheduled workflows fetch provider definitions with `curl` and `gh` and hand the files to
//! this tool, which does only the comparing. Anything the provider defines that a policy table does
//! not class is drift: it would hit the unknown-field rejection and fail a real client. Entries
//! only airmate knows (legacy fields, client extensions) are reported for information.
//!
//! A parser here understands only the patterns the provider's files use today. A layout it does
//! not recognise is an error, never an empty diff that looks like "no drift".

use std::collections::BTreeSet;
use std::fmt::Write as _;

use airmate_core::policy::{Entry, Group};

pub mod anthropic;
pub mod catalog;
pub mod openai;
pub mod provider_docs;

/// What a comparison found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Drift {
    /// What the provider defines and the table does not class: `(what, name)`.
    pub unclassed: Vec<(String, String)>,
    /// What the table classes and the provider no longer defines, for information.
    pub gone: Vec<(String, String)>,
}

impl Drift {
    /// Whether anything needs a human's attention.
    #[must_use]
    pub fn is_drift(&self) -> bool {
        !self.unclassed.is_empty()
    }

    /// A Markdown report.
    #[must_use]
    pub fn report(&self, title: &str) -> String {
        let mut out = format!("## {title}\n\n");
        if self.unclassed.is_empty() {
            out.push_str(
                "No drift: every field the provider defines is classed in the policy table.\n",
            );
        } else {
            out.push_str("Defined by the provider but not classed in the policy table (a client sending one is rejected):\n\n");
            for (what, name) in &self.unclassed {
                let _ = writeln!(out, "- {what}: `{name}`");
            }
        }
        if !self.gone.is_empty() {
            out.push_str(
                "\nClassed in the table but no longer defined by the provider (informational):\n\n",
            );
            for (what, name) in &self.gone {
                let _ = writeln!(out, "- {what}: `{name}`");
            }
        }
        out
    }
}

/// Why a comparison could not be made.
#[derive(Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// The names of the table's rows in `group`.
#[must_use]
pub fn table_names(table: &[Entry], group: Group) -> BTreeSet<String> {
    table
        .iter()
        .filter(|e| e.group == group)
        .map(|e| e.name.to_owned())
        .collect()
}

/// Compares what the provider defines with what the table classes.
#[must_use]
pub fn compare(what: &str, provider: &BTreeSet<String>, table: &BTreeSet<String>) -> Drift {
    Drift {
        unclassed: provider
            .difference(table)
            .map(|n| (what.to_owned(), n.clone()))
            .collect(),
        gone: table
            .difference(provider)
            .map(|n| (what.to_owned(), n.clone()))
            .collect(),
    }
}

/// Merges drifts.
#[must_use]
pub fn merge(parts: Vec<Drift>) -> Drift {
    let mut out = Drift::default();
    for p in parts {
        out.unclassed.extend(p.unclassed);
        out.gone.extend(p.gone);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_difference_is_split_into_unclassed_and_gone() {
        let d = compare(
            "request field",
            &set(&["a", "b", "new"]),
            &set(&["a", "b", "legacy"]),
        );
        assert_eq!(
            d.unclassed,
            vec![("request field".to_owned(), "new".to_owned())]
        );
        assert_eq!(
            d.gone,
            vec![("request field".to_owned(), "legacy".to_owned())]
        );
        assert!(d.is_drift());
        let r = d.report("Anthropic");
        assert!(
            r.contains("- request field: `new`")
                && r.contains("informational")
                && r.contains("`legacy`"),
            "{r}"
        );
    }

    #[test]
    fn identical_sets_are_no_drift_even_with_airmate_only_entries() {
        let d = compare("x", &set(&["a"]), &set(&["a"]));
        assert!(!d.is_drift());
        assert!(d.report("t").contains("No drift"));
        let only_gone = compare("x", &set(&["a"]), &set(&["a", "extra"]));
        assert!(
            !only_gone.is_drift(),
            "entries only airmate knows are not drift"
        );
    }

    #[test]
    fn merging_keeps_everything() {
        let m = merge(vec![
            compare("a", &set(&["x"]), &set(&[])),
            compare("b", &set(&[]), &set(&["y"])),
        ]);
        assert_eq!((m.unclassed.len(), m.gone.len()), (1, 1));
    }
}
