//! `airmate-drift`: the comparison half of the scheduled drift workflows.
//!
//! ```text
//! airmate-drift anthropic <dir of SDK type files>
//! airmate-drift openai <openapi.yaml>
//! airmate-drift provider-docs <sentinels.yaml> <dir of fetched pages>
//! airmate-drift catalog <catalog.yaml> [--max-age-days N] [--today YYYY-MM-DD]
//! ```
//!
//! A Markdown report goes to stdout. Exit 0 means no drift, 1 drift, 2 the comparison could not be
//! made (a changed file layout is a 2, never a quiet 0).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use airmate_drift::{anthropic, catalog, openai, provider_docs, Error};

fn read(p: &Path) -> Result<String, Error> {
    std::fs::read_to_string(p).map_err(|e| Error(format!("cannot read {}: {e}", p.display())))
}

fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, String)>) -> Result<(), Error> {
    for entry in
        std::fs::read_dir(dir).map_err(|e| Error(format!("cannot read {}: {e}", dir.display())))?
    {
        let path = entry.map_err(|e| Error(e.to_string()))?.path();
        if path.is_dir() {
            walk(&path, base, out)?;
        } else if path.extension().is_some_and(|e| e == "py") {
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            out.push((rel, read(&path)?));
        }
    }
    Ok(())
}

fn run(args: &[String]) -> Result<(String, bool), Error> {
    let usage = || {
        Error("usage: airmate-drift anthropic DIR | openai FILE | provider-docs SENTINELS DIR | catalog FILE [--max-age-days N] [--today DATE]".to_owned())
    };
    match args.first().map(String::as_str) {
        Some("anthropic") => {
            let dir = PathBuf::from(args.get(1).ok_or_else(usage)?);
            let mut files = Vec::new();
            walk(&dir, &dir, &mut files)?;
            let d = anthropic::analyze(&files)?;
            Ok((d.report("Anthropic Messages API"), d.is_drift()))
        }
        Some("openai") => {
            let d = openai::analyze(&read(Path::new(args.get(1).ok_or_else(usage)?))?)?;
            Ok((
                d.report("OpenAI Chat Completions and Responses APIs"),
                d.is_drift(),
            ))
        }
        Some("provider-docs") => {
            let sentinels =
                provider_docs::parse(&read(Path::new(args.get(1).ok_or_else(usage)?))?)?;
            let dir = PathBuf::from(args.get(2).ok_or_else(usage)?);
            let mut fetched = BTreeMap::new();
            for p in &sentinels.pages {
                if let Ok(t) = std::fs::read_to_string(dir.join(format!("{}.html", p.name))) {
                    fetched.insert(p.name.clone(), t);
                }
            }
            let gone = provider_docs::missing(&sentinels, &fetched)?;
            let mut out = "## Provider documentation\n\n".to_owned();
            if gone.is_empty() {
                out.push_str("Every sentinel is still on its page.\n");
            } else {
                out.push_str("These strings are no longer on the documented page; re-check the flavor against it:\n\n");
                for (page, s) in &gone {
                    out.push_str(&format!("- `{page}`: `{s}`\n"));
                }
            }
            Ok((out, !gone.is_empty()))
        }
        Some("catalog") => {
            let text = read(Path::new(args.get(1).ok_or_else(usage)?))?;
            let mut max_age = 90;
            let mut today = catalog::today();
            let mut it = args[2..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--max-age-days" => {
                        max_age = it.next().and_then(|v| v.parse().ok()).ok_or_else(usage)?;
                    }
                    "--today" => today = it.next().cloned().ok_or_else(usage)?,
                    _ => return Err(usage()),
                }
            }
            let (age, stale) = catalog::check(&text, &today, max_age)?;
            let as_of = catalog::as_of(&text)?;
            let out = if stale {
                format!("## Model catalog\n\nThe catalog is as of {as_of}, {age} days old (the limit is {max_age}). Check context windows and limits against the providers' model pages and update `as_of`.\n")
            } else {
                format!("## Model catalog\n\nThe catalog is as of {as_of}, {age} days old (the limit is {max_age}).\n")
            };
            Ok((out, stale))
        }
        _ => Err(usage()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok((report, drift)) => {
            print!("{report}");
            ExitCode::from(u8::from(drift))
        }
        Err(e) => {
            eprintln!("airmate-drift: {e}");
            ExitCode::from(2)
        }
    }
}
