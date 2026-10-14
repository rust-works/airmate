//! Routing: which upstream and model serves a request.

pub mod breaker;
pub mod clock;
pub mod glob;
pub mod table;

pub use table::{in_own_family, ModelEntry, ModelTable, Resolution, RouteTarget, UpstreamRt};
