//! The field policy.
//!
//! The load-bearing safety rule: **never silently drop a field that changes behaviour.** Every
//! top-level request field, message-level key, content-block type, tool field, `output_config`
//! key, `thinking` type and display value, stop reason and stream delta type is classed, per
//! inbound protocol, in one table in code:
//!
//! - **translated**: sent upstream in its equivalent form;
//! - **ignored**: accepted but not sent, and reported;
//! - **rejected**: `400 invalid_request_error` naming the field; nothing is sent upstream.
//!
//! A field the table does not know is rejected, since there is no way to tell whether dropping it
//! is safe. [`UnknownFields::Warn`] downgrades unknown fields to ignored, for a client newer than
//! the gateway.

pub mod anthropic;

use std::fmt;

use serde_json::json;

use crate::ir::Protocol;

/// What happens to a field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Sent upstream in its equivalent form.
    Translated,
    /// Accepted but not sent; reported to the client and logged.
    Ignored,
    /// The request is refused.
    Rejected,
}

impl Class {
    /// The name used in the generated tables.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Translated => "translated",
            Self::Ignored => "ignored",
            Self::Rejected => "rejected",
        }
    }
}

/// The kind of thing a table row classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Group {
    /// A top-level request field.
    Request,
    /// A key of a message object.
    MessageKey,
    /// A content block type.
    ContentBlock,
    /// A key inside a content block, named `<type>.<key>`.
    BlockField,
    /// A tool definition field.
    ToolField,
    /// A `tool_choice` field.
    ToolChoiceField,
    /// A `thinking` object field.
    ThinkingField,
    /// A `thinking.type` value.
    ThinkingType,
    /// A `thinking.display` value.
    ThinkingDisplay,
    /// An `output_config` key.
    OutputConfigKey,
    /// A stop reason.
    StopReason,
    /// A stream delta type.
    StreamDelta,
}

impl Group {
    /// Every group, in the order the generated tables list them.
    pub const ALL: [Group; 12] = [
        Self::Request,
        Self::MessageKey,
        Self::ThinkingField,
        Self::ThinkingType,
        Self::ThinkingDisplay,
        Self::OutputConfigKey,
        Self::ContentBlock,
        Self::BlockField,
        Self::ToolField,
        Self::ToolChoiceField,
        Self::StopReason,
        Self::StreamDelta,
    ];

    /// The heading used in the generated tables.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Request => "Request fields",
            Self::MessageKey => "Message keys",
            Self::ContentBlock => "Content blocks",
            Self::BlockField => "Content block fields",
            Self::ToolField => "Tool fields",
            Self::ToolChoiceField => "Tool choice fields",
            Self::ThinkingField => "Thinking fields",
            Self::ThinkingType => "Thinking types",
            Self::ThinkingDisplay => "Thinking display values",
            Self::OutputConfigKey => "Output config keys",
            Self::StopReason => "Stop reasons",
            Self::StreamDelta => "Stream deltas",
        }
    }
}

/// One row of a policy table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// What the row classes.
    pub group: Group,
    /// The field, type or value name.
    pub name: &'static str,
    /// How it is handled.
    pub class: Class,
    /// How, in words.
    pub handling: &'static str,
}

/// Builds a table row.
#[must_use]
pub const fn entry(
    group: Group,
    name: &'static str,
    class: Class,
    handling: &'static str,
) -> Entry {
    Entry {
        group,
        name,
        class,
        handling,
    }
}

/// What to do with a field the table does not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnknownFields {
    /// Reject the request (the default).
    #[default]
    Reject,
    /// Ignore the field and report it.
    Warn,
}

impl UnknownFields {
    /// Parses the configuration value.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "reject" => Some(Self::Reject),
            "warn" => Some(Self::Warn),
            _ => None,
        }
    }
}

/// Policy settings that affect parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PolicyConfig {
    /// The treatment of unknown fields.
    pub unknown_fields: UnknownFields,
}

/// A request part that was accepted but not sent upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ignored {
    /// The name reported in `x-airmate-ignored`.
    pub name: String,
    /// Why.
    pub reason: String,
}

/// A policy rejection: the request is refused with `400 invalid_request_error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyError {
    /// The offending field.
    pub field: String,
    /// The message for the client.
    pub message: String,
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PolicyError {}

/// A parsed value plus what was ignored while parsing it.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed<T> {
    /// The value.
    pub value: T,
    /// Parts accepted but not sent, deduplicated.
    pub ignored: Vec<Ignored>,
}

/// The table for an inbound protocol.
#[must_use]
pub fn table(inbound: Protocol) -> &'static [Entry] {
    match inbound {
        Protocol::Anthropic => anthropic::TABLE,
        _ => &[],
    }
}

/// The verdict on one field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Handle it.
    Translate,
    /// Skip it; it has been recorded as ignored.
    Skip,
}

/// A policy evaluator for one request: classes fields and collects what was ignored.
#[derive(Debug)]
pub struct Policy {
    table: &'static [Entry],
    config: PolicyConfig,
    ignored: Vec<Ignored>,
}

impl Policy {
    /// A policy over `table`.
    #[must_use]
    pub fn new(table: &'static [Entry], config: PolicyConfig) -> Self {
        Self {
            table,
            config,
            ignored: Vec::new(),
        }
    }

    /// A policy with no table, only collecting drops made while building an upstream request.
    #[must_use]
    pub fn collector() -> Self {
        Self::new(&[], PolicyConfig::default())
    }

    /// The table row for `group`/`name`, if any.
    #[must_use]
    pub fn lookup(&self, group: Group, name: &str) -> Option<&'static Entry> {
        self.table
            .iter()
            .find(|e| e.group == group && e.name == name)
    }

    /// Classes one field.
    ///
    /// # Errors
    ///
    /// A [`PolicyError`] for a rejected field, or an unknown one under
    /// [`UnknownFields::Reject`].
    pub fn check(&mut self, group: Group, name: &str) -> Result<Decision, PolicyError> {
        match self.lookup(group, name) {
            Some(e) => match e.class {
                Class::Translated => Ok(Decision::Translate),
                Class::Ignored => {
                    self.ignore(name, e.handling);
                    Ok(Decision::Skip)
                }
                Class::Rejected => Err(PolicyError {
                    field: name.to_owned(),
                    message: format!("`{name}` is not supported by airmate: {}", e.handling),
                }),
            },
            None => match self.config.unknown_fields {
                UnknownFields::Reject => Err(PolicyError {
                    field: name.to_owned(),
                    message: format!(
                        "`{name}` is not known to airmate; set `unknown_fields: warn` to ignore \
                         unknown fields"
                    ),
                }),
                UnknownFields::Warn => {
                    self.ignore(name, "not known to airmate");
                    Ok(Decision::Skip)
                }
            },
        }
    }

    /// Records a part as ignored without consulting the table (for example a drop made while
    /// building an upstream request). Duplicates by name are collapsed.
    pub fn ignore(&mut self, name: &str, reason: &str) {
        if !self.ignored.iter().any(|i| i.name == name) {
            self.ignored.push(Ignored {
                name: name.to_owned(),
                reason: reason.to_owned(),
            });
        }
    }

    /// Consumes the policy and returns what it ignored.
    #[must_use]
    pub fn into_ignored(self) -> Vec<Ignored> {
        self.ignored
    }
}

/// The value of the `x-airmate-ignored` header: names, comma-separated, deduplicated.
#[must_use]
pub fn ignored_header(ignored: &[Ignored]) -> Option<String> {
    let mut names: Vec<&str> = Vec::new();
    for i in ignored {
        if !names.contains(&i.name.as_str()) {
            names.push(&i.name);
        }
    }
    (!names.is_empty()).then(|| names.join(","))
}

/// The table as markdown, one section per group.
#[must_use]
pub fn render_markdown(table: &[Entry]) -> String {
    let mut out = String::new();
    for group in Group::ALL {
        let rows: Vec<&Entry> = table.iter().filter(|e| e.group == group).collect();
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "#### {}\n\n| Name | Class | Handling |\n|---|---|---|\n",
            group.title()
        ));
        for e in rows {
            out.push_str(&format!(
                "| `{}` | {} | {} |\n",
                e.name,
                e.class.as_str(),
                e.handling
            ));
        }
        out.push('\n');
    }
    out
}

/// The table as JSON.
#[must_use]
pub fn render_json(table: &[Entry]) -> String {
    let rows: Vec<_> = table
        .iter()
        .map(|e| {
            json!({
                "group": e.group.title(),
                "name": e.name,
                "class": e.class.as_str(),
                "handling": e.handling,
            })
        })
        .collect();
    serde_json::to_string_pretty(&rows).expect("a JSON array serialises")
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &[Entry] = &[
        entry(Group::Request, "model", Class::Translated, "mapped"),
        entry(Group::Request, "metadata", Class::Ignored, "no equivalent"),
        entry(
            Group::Request,
            "safeguards",
            Class::Rejected,
            "cannot run upstream",
        ),
    ];

    fn policy(unknown: UnknownFields) -> Policy {
        Policy::new(
            T,
            PolicyConfig {
                unknown_fields: unknown,
            },
        )
    }

    #[test]
    fn translated_fields_pass() {
        let mut p = policy(UnknownFields::Reject);
        assert_eq!(p.check(Group::Request, "model"), Ok(Decision::Translate));
        assert!(p.into_ignored().is_empty());
    }

    #[test]
    fn ignored_fields_are_skipped_and_reported_once() {
        let mut p = policy(UnknownFields::Reject);
        assert_eq!(p.check(Group::Request, "metadata"), Ok(Decision::Skip));
        assert_eq!(p.check(Group::Request, "metadata"), Ok(Decision::Skip));
        let ignored = p.into_ignored();
        assert_eq!(ignored.len(), 1);
        assert_eq!(ignored[0].name, "metadata");
        assert_eq!(ignored[0].reason, "no equivalent");
    }

    #[test]
    fn rejected_fields_name_the_field() {
        let mut p = policy(UnknownFields::Reject);
        let err = p.check(Group::Request, "safeguards").unwrap_err();
        assert_eq!(err.field, "safeguards");
        assert!(
            err.message.contains("`safeguards`") && err.message.contains("cannot run upstream")
        );
        assert_eq!(err.to_string(), err.message);
    }

    #[test]
    fn unknown_fields_are_rejected_by_default() {
        let mut p = policy(UnknownFields::Reject);
        let err = p.check(Group::Request, "brand_new").unwrap_err();
        assert!(err.message.contains("not known to airmate"));
    }

    #[test]
    fn unknown_fields_can_be_downgraded_to_ignored() {
        let mut p = policy(UnknownFields::Warn);
        assert_eq!(p.check(Group::Request, "brand_new"), Ok(Decision::Skip));
        assert_eq!(p.into_ignored()[0].name, "brand_new");
    }

    #[test]
    fn rejection_is_not_downgraded_by_warn() {
        let mut p = policy(UnknownFields::Warn);
        assert!(p.check(Group::Request, "safeguards").is_err());
    }

    #[test]
    fn the_same_name_in_another_group_is_unknown() {
        let mut p = policy(UnknownFields::Reject);
        assert!(p.check(Group::ToolField, "model").is_err());
    }

    #[test]
    fn the_ignored_header_is_deduplicated() {
        let ig = |n: &str| Ignored {
            name: n.into(),
            reason: String::new(),
        };
        assert_eq!(ignored_header(&[]), None);
        assert_eq!(
            ignored_header(&[ig("a"), ig("b"), ig("a")]).as_deref(),
            Some("a,b")
        );
    }

    #[test]
    fn unknown_field_modes_parse() {
        assert_eq!(UnknownFields::parse("reject"), Some(UnknownFields::Reject));
        assert_eq!(UnknownFields::parse("warn"), Some(UnknownFields::Warn));
        assert_eq!(UnknownFields::parse("shrug"), None);
    }

    #[test]
    fn markdown_lists_each_group_with_its_rows() {
        let md = render_markdown(T);
        assert!(md.starts_with("#### Request fields\n"));
        assert!(md.contains("| `safeguards` | rejected | cannot run upstream |"));
        assert!(!md.contains("Tool fields"));
    }

    #[test]
    fn json_lists_every_row() {
        let v: serde_json::Value = serde_json::from_str(&render_json(T)).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 3);
        assert_eq!(v[1]["class"], "ignored");
    }

    #[test]
    fn group_and_class_names_are_nonempty() {
        for g in Group::ALL {
            assert!(!g.title().is_empty());
        }
        for c in [Class::Translated, Class::Ignored, Class::Rejected] {
            assert!(!c.as_str().is_empty());
        }
    }
}
