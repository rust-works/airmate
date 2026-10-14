//! Outbound adapters: build an upstream request from the IR and read the reply.

pub mod chat;

use crate::flavor::Flavor;
use crate::ir::{Effort, Origin, Protocol};
use crate::policy::Ignored;
use crate::toolname::ToolNames;

/// An (upstream, model) pair with its per-target overrides: where one request is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The upstream's name.
    pub upstream: String,
    /// The protocol the upstream speaks.
    pub protocol: Protocol,
    /// The upstream's flavor.
    pub flavor: Flavor,
    /// The model name to send upstream.
    pub model: String,
    /// Overrides the flavor's accepted effort levels.
    pub effort_levels: Option<Vec<Effort>>,
    /// Caps the output token limit sent upstream.
    pub max_output_tokens_cap: Option<u32>,
    /// Whether a leading `<think>` block in content is split out as reasoning.
    pub think_tags: bool,
}

impl Target {
    /// A target with defaults, for the common case and for tests.
    #[must_use]
    pub fn new(upstream: &str, protocol: Protocol, flavor: Flavor, model: &str) -> Self {
        Self {
            upstream: upstream.to_owned(),
            protocol,
            flavor,
            model: model.to_owned(),
            effort_levels: None,
            max_output_tokens_cap: None,
            think_tags: true,
        }
    }

    /// Who produced reasoning that came from this target.
    #[must_use]
    pub fn origin(&self) -> Origin {
        Origin {
            upstream: self.upstream.clone(),
            protocol: self.protocol,
        }
    }

    /// The effort levels this target accepts, ascending.
    #[must_use]
    pub fn accepted_efforts(&self) -> &[Effort] {
        self.effort_levels
            .as_deref()
            .unwrap_or_else(|| self.flavor.effort_levels())
    }
}

/// A request ready to send.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamRequest {
    /// The path to append to the upstream's base URL, e.g. `/v1/chat/completions`.
    pub path: String,
    /// The JSON body.
    pub body: Vec<u8>,
    /// Whether the reply is a stream.
    pub stream: bool,
    /// The tool-name mapping to apply in reverse to the reply.
    pub tool_names: ToolNames,
    /// What was dropped while building, to report to the client.
    pub ignored: Vec<Ignored>,
}
