//! Internal helpers backing the higher-level pullers.
//!
//! * [`fast_puller`] (feature `fast-puller`): the `FastDownPuller`
//!   type and its `FastDownPullerOptions`, plus `build_client`
//!   which constructs a correctly-configured `SmartRedirectClient`.
//! * [`getifaddrs`] (feature `getifaddrs`): enumerates the machine's non-virtual
//!   local IP addresses for multi-interface download setups.

#[cfg(feature = "fast-puller")]
#[cfg(not(target_family = "wasm"))]
pub mod fast_puller;
#[cfg(feature = "getifaddrs")]
pub mod getifaddrs;
