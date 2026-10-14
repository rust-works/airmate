//! The price table, and what a request cost.
//!
//! No prices ship with airmate: they change, and a wrong built-in price would be worse than none.
//! A price comes from the target (`price:`), a catalog override, or a price file named by
//! `usage.prices`. A request whose model has no price is counted with cost *unknown*, never zero.

use std::path::Path;

use serde::{Deserialize, Serialize};

use airmate_core::ir::Usage;

use crate::config::schema::Price;
use crate::router::glob::glob_match;

/// One row of a price file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceEntry {
    /// A glob over upstream model names.
    #[serde(rename = "match")]
    pub pattern: String,
    /// Input tokens, in dollars per million.
    pub input_per_mtok: f64,
    /// Output tokens, in dollars per million.
    pub output_per_mtok: f64,
    /// Cached input tokens, if priced differently.
    #[serde(default)]
    pub cache_read_per_mtok: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PriceFile {
    #[serde(default)]
    prices: Vec<PriceEntry>,
}

/// Prices by model name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PriceTable {
    entries: Vec<PriceEntry>,
}

impl PriceTable {
    /// The built-in table: empty, on purpose.
    #[must_use]
    pub fn builtin() -> Self {
        Self::default()
    }

    /// Reads a price file (`prices: [ { match, input_per_mtok, output_per_mtok } ]`).
    ///
    /// # Errors
    ///
    /// The file cannot be read or parsed, or holds a negative price.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read the price file {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Parses the text of a price file.
    ///
    /// # Errors
    ///
    /// The text is not a valid price file.
    pub fn parse(text: &str) -> Result<Self, String> {
        let f: PriceFile = if text.trim().is_empty() {
            PriceFile::default()
        } else {
            serde_norway::from_str(text).map_err(|e| e.to_string())?
        };
        for e in &f.prices {
            if e.pattern.is_empty()
                || e.input_per_mtok < 0.0
                || e.output_per_mtok < 0.0
                || e.cache_read_per_mtok.is_some_and(|c| c < 0.0)
            {
                return Err(format!("the price for `{}` is invalid: prices must not be negative and `match` must not be empty", e.pattern));
            }
        }
        Ok(Self { entries: f.prices })
    }

    /// The table named by `usage.prices`: `builtin`, or a file path.
    ///
    /// # Errors
    ///
    /// The named file is unreadable or invalid.
    pub fn load(spec: &str) -> Result<Self, String> {
        if spec == "builtin" {
            Ok(Self::builtin())
        } else {
            Self::from_file(Path::new(spec))
        }
    }

    /// The price of an upstream model: the first matching row.
    #[must_use]
    pub fn lookup(&self, model: &str) -> Option<Price> {
        self.entries
            .iter()
            .find(|e| glob_match(&e.pattern, model))
            .map(|e| Price {
                input_per_mtok: e.input_per_mtok,
                output_per_mtok: e.output_per_mtok,
                cache_read_per_mtok: e.cache_read_per_mtok,
            })
    }

    /// How many rows there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The cost of `usage` at `price`, in US dollars. Cached tokens are charged at the cache-read
/// price when there is one, else at the input price. Tokens written to a cache are charged at the
/// input price: providers that bill them charge at least that, and a budget that counted them as
/// free would under-count.
#[must_use]
pub fn cost_usd(price: &Price, usage: &Usage) -> f64 {
    let cached_rate = price.cache_read_per_mtok.unwrap_or(price.input_per_mtok);
    ((usage.input_tokens + usage.cache_creation_input_tokens) as f64 * price.input_per_mtok
        + usage.cache_read_input_tokens as f64 * cached_rate
        + usage.output_tokens as f64 * price.output_per_mtok)
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price() -> Price {
        Price {
            input_per_mtok: 2.0,
            output_per_mtok: 8.0,
            cache_read_per_mtok: Some(0.5),
        }
    }

    #[test]
    fn nothing_ships_built_in() {
        assert!(PriceTable::builtin().is_empty());
        assert_eq!(PriceTable::builtin().lookup("gpt-4.1"), None);
        assert!(PriceTable::load("builtin").unwrap().is_empty());
    }

    #[test]
    fn a_price_file_is_matched_by_glob_first_row_wins() {
        let t = PriceTable::parse("prices:\n  - { match: \"gpt-4.1-mini*\", input_per_mtok: 0.4, output_per_mtok: 1.6 }\n  - { match: \"gpt-4.1*\", input_per_mtok: 2.0, output_per_mtok: 8.0, cache_read_per_mtok: 0.5 }\n").unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t.lookup("gpt-4.1-mini").unwrap().input_per_mtok, 0.4);
        assert_eq!(t.lookup("gpt-4.1").unwrap().cache_read_per_mtok, Some(0.5));
        assert_eq!(t.lookup("claude-x"), None);
    }

    #[test]
    fn bad_price_files_are_rejected() {
        assert!(PriceTable::parse(
            "prices:\n  - { match: x, input_per_mtok: -1, output_per_mtok: 1 }\n"
        )
        .is_err());
        assert!(PriceTable::parse(
            "prices:\n  - { match: \"\", input_per_mtok: 1, output_per_mtok: 1 }\n"
        )
        .is_err());
        assert!(PriceTable::parse("prices:\n  - { match: x, input_per_mtok: 1 }\n").is_err());
        assert!(PriceTable::parse(
            "prices:\n  - { match: x, input_per_mtok: 1, output_per_mtok: 1, surprise: 1 }\n"
        )
        .is_err());
        assert!(PriceTable::parse("").unwrap().is_empty());
        assert!(PriceTable::load("/definitely/missing/prices.yaml")
            .unwrap_err()
            .contains("cannot read"));
    }

    #[test]
    fn cost_charges_cached_tokens_at_the_cache_price_when_there_is_one() {
        let u = Usage {
            input_tokens: 1_000_000,
            output_tokens: 500_000,
            cache_read_input_tokens: 2_000_000,
            ..Usage::default()
        };
        assert!((cost_usd(&price(), &u) - (2.0 + 4.0 + 1.0)).abs() < 1e-9);
        let no_cache_price = Price {
            cache_read_per_mtok: None,
            ..price()
        };
        assert!(
            (cost_usd(&no_cache_price, &u) - (2.0 + 4.0 + 4.0)).abs() < 1e-9,
            "cached tokens fall back to the input price"
        );
        assert_eq!(cost_usd(&price(), &Usage::default()), 0.0);
        let written = Usage {
            cache_creation_input_tokens: 1_000_000,
            ..Usage::default()
        };
        assert!(
            (cost_usd(&price(), &written) - 2.0).abs() < 1e-9,
            "cache writes cost at least the input price"
        );
    }
}
