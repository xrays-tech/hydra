//! T10.1 — the test port allocator's band arithmetic and probe behaviour.
//!
//! These live in their own target on purpose: a `#[cfg(test)] mod tests` inside
//! `tests/common/mod.rs` would be compiled into EVERY test target that declares
//! `mod common`, so the same two assertions would run two dozen times and inflate
//! every suite tally (and hide a regression in the noise).

mod common;

/// The band is a HASH of the pid, not `pid % N`.
///
/// `pid % 100` handed two pids 100 apart the SAME 100-port block, so two test
/// processes could fight over one range — the mechanism behind a pass-through
/// assertion receiving a mystery 404 from another test's proxy.
#[test]
fn port_bands_do_not_repeat_for_nearby_pids() {
    let pid = 123_456u64;
    assert_ne!(
        common::port_band(pid),
        common::port_band(pid + 100),
        "pids 100 apart used to share a band — that is the defect this fixes"
    );
    assert_ne!(common::port_band(pid), common::port_band(pid + 1));
    // HONEST, asserted rather than implied: hashing spreads collisions, it does
    // not eliminate them.
    assert_eq!(
        common::port_band(pid),
        common::port_band(pid + common::PORT_BANDS),
        "pids PORT_BANDS apart still share a band (documented residual risk)"
    );
}

/// A probed port is bindable and distinct — i.e. the probe really is RELEASED
/// (the caller hands the port to Pingora, which binds it itself, so holding the
/// socket here would make that bind fail with EADDRINUSE).
#[test]
fn ephemeral_ports_are_bindable_and_distinct() {
    let a = common::ephemeral_port();
    let b = common::ephemeral_port();
    assert_ne!(a, b, "the caller must not get the same port twice");
    for port in [a, b] {
        assert!(
            std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
            "port {port} must still be bindable after the probe released it"
        );
    }
}
