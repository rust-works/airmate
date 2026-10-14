//! airmate: a local, default-closed AI router.
//!
//! A client speaks one protocol (the Anthropic Messages API, the OpenAI Chat Completions API or
//! the OpenAI Responses API) to airmate, which translates the request, picks an upstream model,
//! calls it, and translates the answer back, streaming included.

pub mod admin;
pub mod auth;
pub mod bodylog;
pub mod cli;
pub mod config;
pub mod keychain;
pub mod metrics;
pub mod obs;
pub mod router;
pub mod secret;
pub mod server;
pub mod usage;
