//! Provider documentation drift: some flavors rest on a documented quirk (Fireworks rejecting
//! `thinking` together with `reasoning_effort`, say). The sentinel file lists, per page, strings the
//! page must still contain; one going missing prompts a human to re-check the flavor.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::Error;

/// One page to check.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Page {
    /// A short name; the fetched file is `<name>.html`.
    pub name: String,
    /// Where the workflow fetches it from.
    pub url: String,
    /// Strings the page must contain.
    pub expect: Vec<String>,
}

/// The sentinel file.
#[derive(Debug, Deserialize)]
pub struct Sentinels {
    /// The pages.
    pub pages: Vec<Page>,
}

/// Parses the sentinel file.
///
/// # Errors
///
/// The text is not a valid sentinel file.
pub fn parse(yaml: &str) -> Result<Sentinels, Error> {
    serde_norway::from_str(yaml)
        .map_err(|e| Error(format!("the sentinel file does not parse: {e}")))
}

/// Checks the fetched pages (`name -> text`) and returns the sentinels that went missing, as
/// `(page, sentinel)`.
///
/// # Errors
///
/// A page was not fetched, or came back empty: that is a failed fetch, not a changed page.
pub fn missing(
    sentinels: &Sentinels,
    fetched: &BTreeMap<String, String>,
) -> Result<Vec<(String, String)>, Error> {
    let mut out = Vec::new();
    for page in &sentinels.pages {
        let text = fetched
            .get(&page.name)
            .filter(|t| t.trim().len() > 200)
            .ok_or_else(|| {
                Error(format!(
                    "the page `{}` ({}) was not fetched or is empty",
                    page.name, page.url
                ))
            })?;
        let lower = text.to_ascii_lowercase();
        for s in &page.expect {
            if !lower.contains(&s.to_ascii_lowercase()) {
                out.push((page.name.clone(), s.clone()));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENTINELS: &str = "pages:\n  - name: fireworks-reasoning\n    url: https://docs.fireworks.ai/guides/reasoning\n    expect: [ reasoning_effort, thinking ]\n";

    fn page(text: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(
            "fireworks-reasoning".to_owned(),
            format!("{text} {}", "filler ".repeat(60)),
        )])
    }

    #[test]
    fn sentinels_still_present_means_nothing_to_do() {
        let s = parse(SENTINELS).unwrap();
        assert!(
            missing(&s, &page("Use Reasoning_Effort or thinking, not both."))
                .unwrap()
                .is_empty(),
            "matching ignores case"
        );
    }

    #[test]
    fn a_vanished_sentinel_is_reported() {
        let s = parse(SENTINELS).unwrap();
        let m = missing(&s, &page("Only thinking is documented now.")).unwrap();
        assert_eq!(
            m,
            vec![(
                "fireworks-reasoning".to_owned(),
                "reasoning_effort".to_owned()
            )]
        );
    }

    #[test]
    fn a_page_that_did_not_arrive_is_an_error_not_a_change() {
        let s = parse(SENTINELS).unwrap();
        assert!(missing(&s, &BTreeMap::new()).is_err());
        assert!(missing(
            &s,
            &BTreeMap::from([("fireworks-reasoning".to_owned(), "404".to_owned())])
        )
        .is_err());
        assert!(parse("pages: nope").is_err());
    }
}
