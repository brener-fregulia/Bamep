//! Read-only host pre-flight for the Issue #70 isolated-network proof.
//!
//! **Opt-in and non-mutating.** It does nothing unless
//! `BAMEP_VE_NETWORK_HOST_TEST=1` is set, and even then it only *reads* host
//! state (`ip -V`, `/dev/net/tun`) — it never creates a bridge, TAP, veth, or
//! namespace. An ordinary `cargo test` never touches host networking.
//!
//! The actual privileged proof — create the isolated network, run the BVE
//! PXE-first, observe the fixture logging the deterministic MAC, tear down,
//! and repeat to show no stale state — is the owner-run example, which never
//! calls `sudo` itself:
//!
//! ```text
//! sudo cargo run -p bamep-ve --example bve_isolated_net -- setup   bve-net-proof
//! sudo cargo run -p bamep-ve --example bve_isolated_net -- start-fixture bve-net-proof   # terminal A
//!      cargo run -p bamep-ve --example bve_isolated_net -- run-bve bve-net-proof          # terminal B
//! sudo cargo run -p bamep-ve --example bve_isolated_net -- teardown bve-net-proof
//! ```

#![cfg(unix)]

use bamep_ve::{check_network_prerequisites, BveId, BveNetworkPlan, MAX_IFNAME_LEN};

#[test]
fn host_can_reach_the_isolated_network_prerequisites() {
    if std::env::var_os("BAMEP_VE_NETWORK_HOST_TEST").is_none() {
        eprintln!(
            "skipping isolated-network pre-flight: set BAMEP_VE_NETWORK_HOST_TEST=1 to run it \
             (read-only; the privileged proof is `cargo run -p bamep-ve --example bve_isolated_net`)"
        );
        return;
    }

    check_network_prerequisites()
        .expect("`ip` must be runnable and /dev/net/tun must be a usable character device");

    // The derived names a real setup would use are within kernel limits.
    let plan = BveNetworkPlan::for_bve(&BveId::new("bve-net-proof").unwrap());
    for name in [
        plan.bridge().as_str(),
        plan.tap().as_str(),
        plan.veth_host().as_str(),
        plan.veth_peer().as_str(),
    ] {
        assert!(name.len() <= MAX_IFNAME_LEN, "{name:?} exceeds IFNAMSIZ");
    }
    eprintln!(
        "pre-flight ok: prerequisites present; planned names bridge={} tap={} veth={}/{} netns={}",
        plan.bridge(),
        plan.tap(),
        plan.veth_host(),
        plan.veth_peer(),
        plan.netns()
    );
}
