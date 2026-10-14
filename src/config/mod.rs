//! Configuration: a YAML file, validated, with profiles.
//!
//! The default port is 9997 and is set with `server.port`.

pub mod load;
pub mod overrides;
pub mod paths;
pub mod resolve;
pub mod schema;
pub mod validate;

pub use load::{check_all, load_str, parse, ConfigErrors};
pub use overrides::{Overrides, QuickStart};
pub use paths::{config_file, Dirs, Platform};
pub use resolve::{resolve, show, Inputs, Resolved, Source};
pub use schema::{Config, DEFAULT_PORT};
pub use validate::{validate, ConfigError};
