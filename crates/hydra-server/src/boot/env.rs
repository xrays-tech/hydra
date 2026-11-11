//! Startup environment parsing and validation (F6 split of `main.rs`).
//!
//! Every parser here is either pure (`parse_*`) or a thin `std::env` read over
//! one; the pure halves are unit-tested in this module so the whole accepted
//! vocabulary is pinned without touching the process environment. The F3 opt-in
//! security switches (`HYDRA_DB_REQUIRE_TLS`, `HYDRA_REDIS_REQUIRE_AUTH`) live
//! here as small pure predicates/enforcement helpers.

use hydra_server::admin::AdminService;
use hydra_server::cluster::NodeRole;

#[cfg(feature = "db-postgres")]
pub(crate) const DEFAULT_DB_URL: &str = "postgres://hydra:hydra@127.0.0.1:5432/hydra";
#[cfg(feature = "db-mysql")]
pub(crate) const DEFAULT_DB_URL: &str = "mysql://hydra:hydra@127.0.0.1:3306/hydra";
#[cfg(not(any(feature = "db-postgres", feature = "db-mysql")))]
pub(crate) const DEFAULT_DB_URL: &str = "";
pub(crate) const DEFAULT_LISTEN: &str = "0.0.0.0:8080";
pub(crate) const DEFAULT_ADMIN_LISTEN: &str = "127.0.0.1:8081";

/// `HYDRA_BREAKER_QUORUM`: minimum live votes for a provider to be
/// cluster-dead (default 1 = any live vote). A missing, unparseable or
/// non-positive value falls back to 1 rather than disabling the breaker
/// (0 would mean "never cluster-dead").
#[cfg(feature = "cluster-redis")]
pub(crate) fn breaker_quorum_from_env() -> usize {
    std::env::var("HYDRA_BREAKER_QUORUM")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|q| *q > 0)
        .unwrap_or(1)
}

/// `HYDRA_NON_ROUTE_STRATEGY` = `passthrough` | `reject` (case-insensitive).
///
/// What happens to a request that carries no `model` at all (a well-formed JSON
/// object with no such member — a request Hydra could NOT parse is rejected
/// outright, see `ModelField::Malformed`). `passthrough` forwards it to the
/// tenant's first live provider; `reject` answers `400 no_model_field`, which is
/// the only operator-facing backstop against a request reaching a provider
/// without the tenant model whitelist having been consulted.
///
/// Until now the documented `[proxy] non_route_strategy` was a ghost switch:
/// `main.rs` built `ProxyConfig::default()` and nothing ever read it, so
/// `Reject` was unreachable in the shipped binary (review M-8 / C3).
///
/// Unset → `passthrough` (the historical default). Anything else, including an
/// unknown word, FAILS STARTUP: an operator who typed `reject` and got a silent
/// fallback to `passthrough` would believe a safety control was on.
pub(crate) fn non_route_strategy_from_env(
) -> Result<hydra_server::proxy::config::NonRouteStrategy, String> {
    use hydra_server::proxy::config::NonRouteStrategy;
    match std::env::var("HYDRA_NON_ROUTE_STRATEGY") {
        Err(_) => Ok(NonRouteStrategy::Passthrough),
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "passthrough" => Ok(NonRouteStrategy::Passthrough),
            "reject" => Ok(NonRouteStrategy::Reject),
            other => Err(format!(
                "HYDRA_NON_ROUTE_STRATEGY={other:?} is not a known strategy (expected \"passthrough\" or \"reject\")"
            )),
        },
    }
}

/// Opt-in security switches (`HYDRA_DB_REQUIRE_TLS`, `HYDRA_REDIS_REQUIRE_AUTH`).
///
/// The documented values are `0` (the default) and `1`; the boolean words used
/// elsewhere in this tree are accepted too. Any OTHER non-empty value refuses
/// startup rather than silently reading as "off": an operator who typo'd a
/// security control must not get a node that looks healthy while the control is
/// disabled.
pub(crate) fn env_flag(key: &str, default: u32) -> Result<bool, String> {
    let raw = std::env::var(key).ok();
    parse_env_flag(key, raw.as_deref(), default)
}

/// Pure half of [`env_flag`], so the whole vocabulary is testable without
/// touching the process environment (the same shape as the other `parse_*`
/// helpers here). `default` is passed explicitly so the documented default is
/// visible at the call site.
fn parse_env_flag(key: &str, raw: Option<&str>, default: u32) -> Result<bool, String> {
    match raw.map(str::trim) {
        None | Some("") => Ok(default != 0),
        Some(value) => match value.to_ascii_lowercase().as_str() {
            "0" | "false" | "no" | "off" => Ok(false),
            "1" | "true" | "yes" | "on" => Ok(true),
            _ => Err(format!(
                "{key}={value:?} is not understood (expected 0 or 1; also accepted: \
                 true/false/yes/no/on/off); refusing to start rather than guessing whether \
                 a security control is on"
            )),
        },
    }
}

/// `HYDRA_DB_REQUIRE_TLS`: refuse boot when the operator asked for DB transport
/// security but `HYDRA_DB_URL` does not select an authenticated TLS transport.
/// Pure, so it is unit-tested here and can move verbatim when `main.rs` is split
/// into `boot/*`.
pub(crate) fn enforce_db_tls(db_url: &str, required: bool) -> Result<(), String> {
    if !required || hydra_server::db::db_url_satisfies_tls(db_url) {
        return Ok(());
    }
    Err(
        "HYDRA_DB_REQUIRE_TLS=1 but HYDRA_DB_URL does not require an authenticated TLS \
         transport; refusing to start. Accepted values: PostgreSQL \
         `sslmode=require|verify-ca|verify-full`; MySQL `ssl-mode` (or `sslmode`) = \
         `REQUIRED|VERIFY_CA|VERIFY_IDENTITY` (keys and values are case-insensitive). Add the \
         parameter to HYDRA_DB_URL or unset HYDRA_DB_REQUIRE_TLS."
            .to_string(),
    )
}

/// `HYDRA_REDIS_REQUIRE_AUTH`: refuse boot when the flag is on but the URL has no
/// non-empty password. `rediss://` is TLS, not authentication, so it does not
/// satisfy this check on its own. Pure, and gateable for the planned `boot/*`
/// split.
#[cfg(feature = "cluster-redis")]
pub(crate) fn enforce_redis_auth(redis_url: Option<&str>, required: bool) -> Result<(), String> {
    if !required || redis_url.is_some_and(hydra_server::redis::redis_url_has_auth) {
        return Ok(());
    }
    Err(
        "HYDRA_REDIS_REQUIRE_AUTH=1 but HYDRA_REDIS_URL has no non-empty password \
         component; refusing to start. Use `redis://:PASSWORD@host:port/db` (or \
         `rediss://:PASSWORD@host:port/db` for TLS) — `rediss://` alone is TLS, not \
         authentication. Unset HYDRA_REDIS_REQUIRE_AUTH to disable this check."
            .to_string(),
    )
}

/// Admin-surface startup gate: a cluster node must carry an admin token (each
/// node serves its own admin API), and any configured token must meet the
/// strength floor (design §13.3). Refusing to BOOT on a short token rather than
/// warning is deliberate: the gate has no rate limit or lockout, and the admin
/// API is the authority over every tenant, provider and stored upstream api-key.
pub(crate) fn validate_admin_access(role: NodeRole) -> Result<(), Box<dyn std::error::Error>> {
    if role.is_cluster()
        && std::env::var("HYDRA_ADMIN_TOKEN")
            .map(|t| t.is_empty())
            .unwrap_or(true)
    {
        // Each node serves its own admin API, so the token is what gates EACH
        // node's admin surface. A cluster node without one would expose an
        // unauthenticated admin API.
        return Err(
            "cluster mode requires HYDRA_ADMIN_TOKEN (each node serves its own admin API); \
             refusing to start"
                .into(),
        );
    }
    // Admin-token strength (design §13.3). Refuse to BOOT on a short token
    // rather than warn: the gate has no rate limit or lockout, and the admin
    // API is the authority over every tenant, provider and stored upstream
    // api-key — so a guessable token is a full gateway takeover. Shipping a
    // weak default is exactly the failure mode this prevents.
    if let Ok(token) = std::env::var("HYDRA_ADMIN_TOKEN") {
        if !token.is_empty() && token.len() < AdminService::MIN_ADMIN_TOKEN_LEN {
            return Err(format!(
                "HYDRA_ADMIN_TOKEN is too short ({} chars, minimum {}); \
                 generate one with `openssl rand -hex 32`; refusing to start",
                token.len(),
                AdminService::MIN_ADMIN_TOKEN_LEN
            )
            .into());
        }
    }
    Ok(())
}

/// `HYDRA_USAGE_SINK`: REQUIRED, with no default (ADR-0002 D-1) — "where does
/// the billing data go" is a decision, and every value this product could guess
/// is wrong for someone. The list of accepted values comes from the registry, so
/// the message cannot rot into naming a backend that no longer exists.
pub(crate) fn usage_sink_kind() -> Result<String, Box<dyn std::error::Error>> {
    match std::env::var("HYDRA_USAGE_SINK") {
        Ok(v) => Ok(v),
        Err(_) => Err(format!(
            "HYDRA_USAGE_SINK is not set; there is no default. Set it to one of: {} \
             (`none` records nothing at all and says so at startup)",
            hydra_server::usage::known_kinds()
        )
        .into()),
    }
}

/// `HYDRA_RESEAL_SECRETS=1` (or `true`) — run the one-shot re-seal and exit.
///
/// Container-friendly on purpose: the alternative (a CLI subcommand) has to fight
/// the Pingora argument parser, and this is an operation you run with
/// `docker run --rm -e ... <same image>`.
pub(crate) fn reseal_requested() -> bool {
    matches!(reseal_switch(), ResealSwitch::On)
}

/// The three states of `HYDRA_RESEAL_SECRETS` (see [`reseal_requested`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResealSwitch {
    /// Serve traffic normally.
    Off,
    /// Re-seal every stored secret and exit.
    On,
    /// A value this process does not understand — refused at startup.
    Invalid(String),
}

/// Parse the one-shot maintenance switch **strictly**.
///
/// Measured 2026-09-30 (`integration/test_key_rotation_live.py`, case K8): the previous version
/// was `matches!(var, Ok("1") | Ok("true") | Ok("yes"))`, so EVERY other value — `YES`, `on`,
/// `TRUE`, `reseal`, `2` — silently fell through to "serve traffic normally". The documented
/// procedure is "run this once, read the report line and the exit code"; an operator whose
/// container then came up and served had no signal at all that the rotation never ran, and the
/// controller in `ops.md` §3 (`docker run --rm -e HYDRA_RESEAL_SECRETS=1 …`) would sit there
/// serving instead of exiting. `upgrade_requested` is deliberately strict about the same shape
/// of mistake ("an operator who typos the flag gets the loud refusal instead of a silent
/// non-upgrade"); this switch now matches that standard. Values are matched case-insensitively
/// so `TRUE`/`Yes` do what a human means.
///
/// `ON_VALUES`/`OFF_VALUES` are documented in `ops.md` §1.2.
pub(crate) fn reseal_switch() -> ResealSwitch {
    parse_reseal_switch(std::env::var("HYDRA_RESEAL_SECRETS").ok().as_deref())
}

/// Pure half of [`reseal_switch`] — free of the process environment so the whole vocabulary can
/// be tested directly (the same shape as the `parse_*` helpers in `proxy::config`).
fn parse_reseal_switch(raw: Option<&str>) -> ResealSwitch {
    const ON_VALUES: [&str; 4] = ["1", "true", "yes", "on"];
    const OFF_VALUES: [&str; 5] = ["", "0", "false", "no", "off"];
    let Some(raw) = raw else {
        // Unset is the ordinary case: serve traffic.
        return ResealSwitch::Off;
    };
    let value = raw.trim().to_ascii_lowercase();
    if ON_VALUES.contains(&value.as_str()) {
        ResealSwitch::On
    } else if OFF_VALUES.contains(&value.as_str()) {
        ResealSwitch::Off
    } else {
        ResealSwitch::Invalid(raw.to_string())
    }
}

/// How long a node may go without re-registering before its row becomes
/// reapable (`HYDRA_REGISTRY_STALE_GRACE_SECS`, default 120).
///
/// This value is used ONLY here, at registration time: it is the TTL of the
/// Seconds Pingora may spend draining in-flight requests after SIGTERM
/// (`HYDRA_SHUTDOWN_DRAIN_SECS`, default 20).
///
/// This maps to Pingora's `grace_period_seconds`. The deployment's
/// `terminationGracePeriodSeconds` MUST exceed this value plus the final
/// runtime-shutdown step (5s) plus slack, or the process is SIGKILLed mid-drain
/// and both the usage-sink flush and the registry de-registration are lost. The
/// value is deployment-visible, so it is recorded in `dev-docs/ops.md`.
///
/// `0` is rejected: Pingora would read `Some(0)` as "no drain at all", silently
/// defeating the point of configuring it.
pub(crate) fn shutdown_drain_secs() -> u64 {
    parse_shutdown_drain_secs(std::env::var("HYDRA_SHUTDOWN_DRAIN_SECS").ok().as_deref())
}

/// Parse half of [`shutdown_drain_secs`], kept PURE so it can be tested without
/// touching the process environment (like the other config parsers here).
///
/// `0` is rejected: Pingora would read `Some(0)` as "no drain at all", silently
/// defeating the point of configuring it.
fn parse_shutdown_drain_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(20)
}

/// `-u` / `--upgrade`: inherit the running process's listening sockets instead of
/// binding them (Pingora's zero-downtime upgrade, `ops.md` §2).
///
/// Pure so it can be tested without touching the process environment — and so the
/// documented flag has exactly one definition. Only the exact short/long flags are
/// accepted (`--upgrade=1` is not, deliberately: an operator who typos the flag gets
/// the loud "address already in use" refusal instead of a silent non-upgrade).
#[must_use]
pub(crate) fn upgrade_requested(args: &[String]) -> bool {
    args.iter().any(|a| a == "-u" || a == "--upgrade")
}

#[cfg(test)]
mod drain_tests {
    use super::parse_shutdown_drain_secs;

    /// T9.4 — the drain window is configurable, `0` is rejected, and the default
    /// is the documented 20s. Without this, deleting the `grace_period_seconds`
    /// field from the `ServerConf` would silently restore Pingora's 300s default
    /// with every gate still green.
    ///
    /// Deliberately NOT inside the `cluster-redis` test module: the parser has
    /// nothing to do with clustering, and that module does not compile under the
    /// `--features server` gate.
    #[test]
    fn shutdown_drain_parsing_is_total() {
        assert_eq!(parse_shutdown_drain_secs(None), 20, "documented default");
        assert_eq!(parse_shutdown_drain_secs(Some("45")), 45);
        assert_eq!(parse_shutdown_drain_secs(Some(" 45 ")), 45, "trimmed");
        assert_eq!(
            parse_shutdown_drain_secs(Some("0")),
            20,
            "0 ⇒ no drain ⇒ default"
        );
        assert_eq!(parse_shutdown_drain_secs(Some("nope")), 20);
        assert_eq!(parse_shutdown_drain_secs(Some("-1")), 20);
    }
}

#[cfg(test)]
mod security_flag_tests {
    use super::{enforce_db_tls, parse_env_flag};

    /// F3: both opt-in switches default OFF and accept a strict vocabulary; a
    /// typo is refused instead of silently leaving a security control off.
    #[test]
    fn security_switches_default_off_and_parse_strictly() {
        for key in ["HYDRA_DB_REQUIRE_TLS", "HYDRA_REDIS_REQUIRE_AUTH"] {
            assert!(
                !parse_env_flag(key, None, 0).expect("unset = default"),
                "{key}"
            );
            for value in ["", "0", "false", "FALSE", "no", "off", " 0 "] {
                assert!(
                    !parse_env_flag(key, Some(value), 0).expect("off value"),
                    "{key}={value:?}"
                );
            }
            for value in ["1", "true", "TRUE", "yes", "on", " 1 "] {
                assert!(
                    parse_env_flag(key, Some(value), 0).expect("on value"),
                    "{key}={value:?}"
                );
            }
            for value in ["2", "enabled", "1x", "ture", "y"] {
                assert!(
                    parse_env_flag(key, Some(value), 0).is_err(),
                    "{key}={value:?} must be refused, not guessed"
                );
            }
        }
    }

    /// The DB refusal names the flag and every accepted value, and disabling the
    /// switch leaves the check inert.
    #[test]
    fn db_tls_refusal_is_precise() {
        assert!(enforce_db_tls("postgres://hydra@db/hydra?sslmode=prefer", true).is_err());
        assert!(enforce_db_tls("postgres://hydra@db/hydra?sslmode=prefer", false).is_ok());
        let err = enforce_db_tls("postgres://hydra@db/hydra?sslmode=prefer", true)
            .expect_err("weaker mode must be refused");
        assert!(err.contains("HYDRA_DB_REQUIRE_TLS"), "{err}");
        assert!(err.contains("verify-full"), "{err}");
        assert!(err.contains("VERIFY_IDENTITY"), "{err}");
    }

    /// `rediss://` is TLS, not auth: the Redis refusal must be explicit about
    /// that and name the flag.
    #[cfg(feature = "cluster-redis")]
    #[test]
    fn redis_auth_refusal_is_precise_and_tls_is_not_auth() {
        use super::enforce_redis_auth;

        assert!(enforce_redis_auth(Some("redis://:pw@redis:6379/0"), true).is_ok());
        assert!(enforce_redis_auth(Some("rediss://:pw@redis:6379/0"), true).is_ok());
        assert!(enforce_redis_auth(Some("rediss://redis:6379/0"), false).is_ok());
        assert!(enforce_redis_auth(None, false).is_ok());
        for url in [
            Some("rediss://redis:6379/0"),
            Some("redis://redis:6379/0"),
            None,
        ] {
            let err = enforce_redis_auth(url, true)
                .expect_err("no password must be refused when the flag is on");
            assert!(err.contains("HYDRA_REDIS_REQUIRE_AUTH"), "{err}");
            assert!(err.contains("not authentication"), "{err}");
        }
    }
}

#[cfg(all(test, feature = "cluster-redis"))]
mod tests {
    use super::*;

    /// The one-shot rotation switch is parsed STRICTLY and case-insensitively.
    ///
    /// Regression (measured 2026-09-30, `integration/test_key_rotation_live.py` K8): the previous
    /// `matches!(var, Ok("1") | Ok("true") | Ok("yes"))` turned every other value — `YES`, `on`,
    /// `TRUE`, `reseal`, `2` — into "serve traffic normally", so a typo in the documented one-shot
    /// command silently skipped the rotation while the node came up looking healthy.
    #[test]
    fn the_reseal_switch_is_strict_about_values_it_does_not_know() {
        use ResealSwitch::{Invalid, Off, On};
        // The ordinary cases.
        assert_eq!(parse_reseal_switch(None), Off);
        for v in ["", "0", "false", "no", "off", "  ", "FALSE", "Off"] {
            assert_eq!(
                parse_reseal_switch(Some(v)),
                Off,
                "{v:?} must mean 'serve normally'"
            );
        }
        for v in ["1", "true", "yes", "on", "TRUE", "Yes", " on ", "1 "] {
            assert_eq!(
                parse_reseal_switch(Some(v)),
                On,
                "{v:?} must mean 're-seal and exit'"
            );
        }
        // ...and everything else is REFUSED rather than silently served.
        for v in ["reseal", "2", "enabled", "y", "1x", "ture", "re-seal"] {
            match parse_reseal_switch(Some(v)) {
                Invalid(echoed) => assert_eq!(echoed, v, "the offending value must be echoed"),
                other => panic!("{v:?} must be Invalid, got {other:?}"),
            }
        }
    }

    /// F-5: `HYDRA_BREAKER_QUORUM` must actually be honored (before the fix
    /// it was a ghost env — mentioned in a comment and the docs, never read).
    /// The only test in this binary touches this key, so the process-global
    /// env is safe to mutate here.
    #[test]
    fn breaker_quorum_env_injection() {
        std::env::set_var("HYDRA_BREAKER_QUORUM", "3");
        assert_eq!(breaker_quorum_from_env(), 3, "injected quorum is honored");

        std::env::set_var("HYDRA_BREAKER_QUORUM", "bogus");
        assert_eq!(breaker_quorum_from_env(), 1, "unparseable → default 1");

        std::env::set_var("HYDRA_BREAKER_QUORUM", "0");
        assert_eq!(breaker_quorum_from_env(), 1, "non-positive → default 1");

        std::env::remove_var("HYDRA_BREAKER_QUORUM");
        assert_eq!(breaker_quorum_from_env(), 1, "unset → default 1");
    }
}

/// C3: the non-route strategy parser. NOT gated on `cluster-redis` — the proxy
/// config exists in every build, and this switch is the backstop that keeps a
/// model-less request away from a provider.
#[cfg(test)]
mod non_route_strategy_tests {
    use super::non_route_strategy_from_env;
    use hydra_server::proxy::config::NonRouteStrategy;

    #[test]
    fn non_route_strategy_env_is_validated() {
        std::env::remove_var("HYDRA_NON_ROUTE_STRATEGY");
        assert_eq!(
            non_route_strategy_from_env(),
            Ok(NonRouteStrategy::Passthrough),
            "unset keeps the historical default"
        );

        std::env::set_var("HYDRA_NON_ROUTE_STRATEGY", "reject");
        assert_eq!(non_route_strategy_from_env(), Ok(NonRouteStrategy::Reject));

        std::env::set_var("HYDRA_NON_ROUTE_STRATEGY", "REJECT");
        assert_eq!(
            non_route_strategy_from_env(),
            Ok(NonRouteStrategy::Reject),
            "case-insensitive"
        );

        std::env::set_var("HYDRA_NON_ROUTE_STRATEGY", " reject ");
        assert_eq!(
            non_route_strategy_from_env(),
            Ok(NonRouteStrategy::Reject),
            "surrounding whitespace is tolerated"
        );

        std::env::set_var("HYDRA_NON_ROUTE_STRATEGY", "passthrough");
        assert_eq!(
            non_route_strategy_from_env(),
            Ok(NonRouteStrategy::Passthrough)
        );

        for bad in ["rejct", "deny", "false", "1", ""] {
            std::env::set_var("HYDRA_NON_ROUTE_STRATEGY", bad);
            assert!(
                non_route_strategy_from_env().is_err(),
                "HYDRA_NON_ROUTE_STRATEGY={bad:?} must fail startup, not silently pass through"
            );
        }

        std::env::remove_var("HYDRA_NON_ROUTE_STRATEGY");
    }
}

/// The documented `-u` flag has its own test module so it runs in the default
/// `--features server` job (the pre-existing module is gated on `cluster-redis`).
#[cfg(test)]
mod upgrade_flag_tests {
    use super::upgrade_requested;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_string()).collect()
    }

    /// The regression this pins: `hydra -u` used to be a NO-OP (argv was never
    /// read), so the runbook's zero-downtime upgrade ended with the new process
    /// dying on `Address already in use` — measured 2026-09-29.
    #[test]
    fn the_upgrade_flag_is_recognised_exactly() {
        assert!(upgrade_requested(&argv(&["-u"])));
        assert!(upgrade_requested(&argv(&["--upgrade"])));
        assert!(upgrade_requested(&argv(&["-u", "--log", "info"])));
        assert!(!upgrade_requested(&argv(&[])));
        assert!(!upgrade_requested(&argv(&["--log", "info"])));
        assert!(
            !upgrade_requested(&argv(&["--upgrade=1"])),
            "no value form: the flag is boolean"
        );
        assert!(!upgrade_requested(&argv(&["-U"])), "case matters");
    }
}
