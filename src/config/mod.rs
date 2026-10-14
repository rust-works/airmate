//! Configuration: a YAML file, validated, with profiles.
//!
//! The default port is 9997 and is set with `server.port`.

pub mod load;
pub mod paths;
pub mod schema;
pub mod validate;

pub use load::{check_all, load_str, parse, ConfigErrors};
pub use paths::{config_file, Dirs, Platform};
pub use schema::{Config, DEFAULT_PORT};
pub use validate::{validate, ConfigError};
