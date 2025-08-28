// crates/execution/src/engine_api/capabilities.rs

// This file will manage the negotiation and caching of capabilities
// with the execution client.

// --- REFERENCE IMPLEMENTATION ---
// use std::time::{Duration, Instant};
//
// Caches the capabilities of the remote execution client to avoid
// calling `engine_exchangeCapabilities` on every single API call.
// #[derive(Clone, Debug)]
// pub struct CachedCapabilities {
// pub capabilities: Vec<String>,
// expires_at: Instant,
// }
//
// impl CachedCapabilities {
// Creates a new cache with a given set of capabilities and a TTL.
// pub fn new(capabilities: Vec<String>) -> Self {
// Self {
// capabilities,
// A 5-minute TTL is a reasonable default.
// expires_at: Instant::now() + Duration::from_secs(300),
// }
// }
//
// Checks if the cache is still valid.
// pub fn is_valid(&self) -> bool {
// self.expires_at > Instant::now()
// }
// }
