//! Routing: which upstream and model serves a request.

pub mod affinity;
pub mod breaker;
pub mod catalog;
pub mod classifier;
pub mod clock;
pub mod engine;
pub mod glob;
pub mod health;
pub mod limits;
pub mod rules;
pub mod state;
pub mod strategy;
pub mod table;

pub use table::{in_own_family, ModelEntry, ModelTable, Resolution, RouteTarget, UpstreamRt, Via};
