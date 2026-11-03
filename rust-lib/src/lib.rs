//! beacon_api_module — a proxyable, fail-closed Beacon API client.
//!
//! Holds the consensus-layer providers per chain, in one place, and serves the light-client
//! routes the verified proxy needs with failover across them. The core (`beacon`, `proxy`) is
//! plain Rust, tested with `cargo test --no-default-features`; the Logos glue is behind the
//! default `logos_module` feature.

mod beacon;
mod proxy;

pub use beacon::{
    deadline, derive_id, lag_slots, network, normalize_endpoint, valid_id, Attempt, BeaconApi, BeaconError,
    BuiltinProvider, ChainConfig, ConfigSource, Fetched, Health, HealthBook, LcRequest, Network, Plan, Provider,
    DEFAULT_PROVIDERS, MAX_HEAD_LAG_SLOTS, NETWORKS,
};

#[cfg(test)]
mod tests;

#[cfg(feature = "logos_module")]
mod glue;
