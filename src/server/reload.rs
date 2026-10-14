//! Reloading the configuration.
//!
//! A reload re-reads and validates the configuration, builds new tables (resolving every secret),
//! and swaps them in atomically. In-flight requests finish on the tables they started with. A
//! configuration that fails validation is rejected and the old one stays in force, with the error
//! reported to the caller. The listen address, TLS material, the global concurrency limit and the
//! `Host` allow-list belong to the listener, which a reload does not rebuild: changing them needs
//! a restart, and the report says so.

use std::sync::Arc;

use super::{Shared, Tables};
use crate::config::schema::Config;
use crate::config::validate::ConfigError;
use crate::secret::{Secret, SecretResolver};

/// What a successful reload did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReloadReport {
    /// Warnings from building the tables (for example a literal secret).
    pub warnings: Vec<String>,
    /// Settings that changed but only take effect after a restart.
    pub restart_required: Vec<String>,
}

/// The settings of `new` that differ from `old` and belong to the listener.
#[must_use]
pub fn restart_required(old: &Config, new: &Config) -> Vec<String> {
    let (o, n) = (&old.server, &new.server);
    let mut v = Vec::new();
    macro_rules! check {
        ($($f:ident),*) => { $( if o.$f != n.$f { v.push(format!("server.{}", stringify!($f))); } )* };
    }
    check!(
        bind,
        port,
        unix_socket,
        tls,
        max_concurrent,
        allowed_hosts,
        insecure_remote,
        shutdown_timeout_secs
    );
    v
}

/// Builds tables from `config` and swaps them in.
///
/// The listener-owned settings of `config` are replaced by the ones actually in use, so the
/// configuration the gateway reports is the one it is running.
///
/// # Errors
///
/// Every problem found; the tables in force are untouched.
pub fn reload(
    shared: &Shared,
    mut config: Config,
    resolver: &dyn SecretResolver,
    gateway_token: &Secret,
) -> Result<ReloadReport, Vec<ConfigError>> {
    let problems = crate::config::validate(&config);
    if !problems.is_empty() {
        return Err(problems);
    }
    let current = shared.tables();
    let restart = restart_required(&current.config, &config);
    {
        let (cur, new) = (&current.config.server, &mut config.server);
        new.bind.clone_from(&cur.bind);
        new.port = cur.port;
        new.unix_socket.clone_from(&cur.unix_socket);
        new.tls.clone_from(&cur.tls);
        new.max_concurrent = cur.max_concurrent;
        new.allowed_hosts.clone_from(&cur.allowed_hosts);
        new.insecure_remote = cur.insecure_remote;
        new.shutdown_timeout_secs = cur.shutdown_timeout_secs;
    }
    let (tables, warnings) = Tables::build_with(
        Arc::new(config.clone()),
        resolver,
        gateway_token,
        shared.log.clone(),
    )?;
    shared.router.set_breaker_config((&config.circuit).into());
    shared.swap_tables(tables);
    Ok(ReloadReport {
        warnings,
        restart_required: restart,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::paths::EnvFn;
    use crate::secret::SystemResolver;

    #[test]
    fn only_listener_settings_need_a_restart() {
        let a = Config::default();
        let mut b = a.clone();
        assert!(restart_required(&a, &b).is_empty());
        b.server.port = 1234;
        b.server.allowed_hosts = vec!["x".into()];
        b.server.max_concurrent = 3;
        b.server.think_tags = false;
        b.server.unknown_fields = crate::config::schema::UnknownFieldsMode::Warn;
        assert_eq!(
            restart_required(&a, &b),
            [
                "server.port",
                "server.max_concurrent",
                "server.allowed_hosts"
            ]
        );
    }

    #[tokio::test]
    async fn a_valid_reload_swaps_and_an_invalid_one_changes_nothing() {
        let env: EnvFn<'_> = &|_| None;
        let resolver = SystemResolver::new(env);
        let token = Secret::new("t");
        let mut cfg = Config::default();
        cfg.server.port = 0;
        let (tables, _) = Tables::build(Arc::new(cfg.clone()), &resolver, &token).unwrap();
        let gw = crate::server::bind(&cfg, tables).await.unwrap();
        let shared = gw.shared();
        let before = shared.tables();
        let bound_port = gw.local_addr().port();

        let mut next = cfg.clone();
        next.server.think_tags = false;
        next.server.port = 4242;
        let report = reload(&shared, next, &resolver, &token).unwrap();
        assert_eq!(report.restart_required, ["server.port"]);
        let after = shared.tables();
        assert!(
            !after.config.server.think_tags,
            "the new setting is in force"
        );
        assert_eq!(
            after.config.server.port, 0,
            "the listener keeps the port it has"
        );
        assert!(
            before.config.server.think_tags,
            "an in-flight request keeps its tables"
        );
        let _ = bound_port;

        let mut bad = cfg.clone();
        bad.server.max_body_bytes = 0;
        let errs = reload(&shared, bad, &resolver, &token).unwrap_err();
        assert!(errs[0].path.contains("max_body_bytes"));
        assert!(
            Arc::ptr_eq(&shared.tables(), &after),
            "a rejected reload leaves the tables alone"
        );

        let mut missing_secret = cfg.clone();
        missing_secret
            .upstreams
            .push(crate::config::schema::UpstreamConfig {
                name: "a".into(),
                url: "http://a".into(),
                protocol: crate::config::schema::ProtocolName::Chat,
                flavor: crate::config::schema::FlavorName::Generic,
                api_key: Some(crate::config::schema::SecretRef::Env("NOT_SET".into())),
                max_concurrent: None,
                timeouts: Default::default(),
                rate: None,
                health_check: None,
                effort_levels: None,
                think_tags: None,
                proxy: None,
            });
        let errs = reload(&shared, missing_secret, &resolver, &token).unwrap_err();
        assert!(errs[0].message.contains("NOT_SET"));
        assert!(Arc::ptr_eq(&shared.tables(), &after));
    }
}
