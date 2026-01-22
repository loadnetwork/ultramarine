//! Global constants shared across Ultramarine crates.

/// Default execution gas limit for Load Network blocks.
pub const LOAD_EXECUTION_GAS_LIMIT: u64 = 2_000_000_000;

/// Minimum time between blocks in seconds (slot duration).
/// Protocol rule: validators reject proposals violating this.
pub const LOAD_MIN_BLOCK_TIME_SECS: u64 = 1;

/// Maximum allowed clock drift in seconds (geth/ETH canonical value).
/// Protocol rule: validators reject proposals with timestamp > now + drift.
pub const LOAD_MAX_FUTURE_DRIFT_SECS: u64 = 15;

#[cfg(test)]
mod tests {
    use super::*;

    /// Documents and verifies protocol constants.
    /// These values are critical for network consensus:
    /// - LOAD_MIN_BLOCK_TIME_SECS=1: EVM timestamp granularity (1 block/sec max)
    /// - LOAD_MAX_FUTURE_DRIFT_SECS=15: geth canonical value for clock tolerance
    /// - LOAD_EXECUTION_GAS_LIMIT=2B: high throughput at 1 block/sec
    #[test]
    fn constants_have_expected_values() {
        assert_eq!(LOAD_EXECUTION_GAS_LIMIT, 2_000_000_000);
        assert_eq!(LOAD_MIN_BLOCK_TIME_SECS, 1);
        assert_eq!(LOAD_MAX_FUTURE_DRIFT_SECS, 15);
    }
}
