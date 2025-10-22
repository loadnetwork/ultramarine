# Blob Limit Configuration

**Last Updated**: 2025-10-21

## Overview

This document explains the blob limit configuration for the Ultramarine consensus client and how it differs from Ethereum mainnet.

---

## Blob Limits

### Protocol Limit: 1024 Blobs Per Block

The chain enforces a practical limit of **1024 blobs per block**, providing:

- **DA Capacity**: 1024 blobs × 131,072 bytes = **~134 MB per block**
- **Throughput**: 170× more capacity than Ethereum Deneb (786 KB)
- **Scalability**: Room to increase to 4096 without protocol changes
- **Manageability**: Reasonable validator and network load

### SSZ Capacity: 4096 Blobs

The underlying SSZ structure supports up to **4096 blobs** (full theoretical capacity):

- **Merkle Tree Depth**: log2(4096) = 12 levels
- **Future Growth**: Can increase protocol limit to 4096 without breaking changes
- **Compatibility**: Matches Ethereum consensus spec SSZ list capacity

---

## Constant Definitions

```rust
// crates/types/src/blob.rs

/// Maximum SSZ List capacity for blob commitments (4096)
/// Used for Merkle proof generation and SSZ serialization
pub const MAX_BLOB_COMMITMENTS_PER_BLOCK: usize = 4096;

/// Maximum number of blobs allowed per block (1024)
/// Enforced by protocol validation
pub const MAX_BLOBS_PER_BLOCK: usize = 1024;
```

---

## Comparison with Ethereum

| Network | Max Blobs | Capacity/Block | Constraint |
|---------|-----------|----------------|------------|
| **Ethereum Deneb** | 6 | ~786 KB | Blob gas limits |
| **Ethereum Electra** | 9 | ~1.18 MB | Blob gas limits |
| **Ultramarine** | **1024** | **~134 MB** | Protocol limit |
| **SSZ Theoretical Max** | 4096 | ~537 MB | Structure capacity |

---

## Where Limits Are Enforced

### 1. Blob Bundle Validation

**File**: `crates/types/src/blob.rs`

```rust
// BlobsBundle::validate() - Line ~652
let max_blobs = MAX_BLOBS_PER_BLOCK; // 1024
if self.blobs.len() > max_blobs {
    return Err(format!(
        "Too many blobs: got {}, maximum is {} (protocol limit)",
        self.blobs.len(),
        max_blobs
    ));
}
```

### 2. Value Metadata Validation

**File**: `crates/types/src/value_metadata.rs`

```rust
// ValueMetadata::validate() - Line ~292
if self.blob_count as usize > MAX_BLOBS_PER_BLOCK {
    return Err(format!(
        "Too many blobs: got {}, max is {} (protocol limit)",
        self.blob_count, MAX_BLOBS_PER_BLOCK
    ));
}
```

### 3. Merkle Proof Generation

**File**: `crates/types/src/ethereum_compat/merkle.rs`

```rust
// Uses MAX_BLOB_COMMITMENTS_PER_BLOCK for SSZ tree depth
let capacity = MAX_BLOB_COMMITMENTS_PER_BLOCK.next_power_of_two(); // 4096
let depth = capacity.ilog2() as usize; // 12 levels
```

---

## Rationale

### Why 1024 Instead of 4096?

1. **Network Load**: 134 MB blocks are manageable for validators
2. **Testing**: Easier to test with realistic blob counts
3. **Gradual Scaling**: Can increase limit based on network performance
4. **Storage**: More predictable storage growth patterns

### Why Keep 4096 SSZ Capacity?

1. **Future-Proof**: Can increase limit without protocol upgrade
2. **Compatibility**: Matches Ethereum spec SSZ structure
3. **Merkle Proofs**: Maintains consistent tree depth (12 levels)
4. **No Breaking Changes**: Protocol can grow organically

---

## Performance Implications

### With 1024 Blobs/Block

| Metric | Value |
|--------|-------|
| **Block Size** | ~134 MB |
| **Network Bandwidth** | ~134 MB download per validator |
| **KZG Verification Time** | ~1-2s (batch mode) |
| **Storage Growth** | ~19 GB/day (1 block/12s) |
| **Merkle Proof Size** | 13 × 32 bytes = 416 bytes |

### Validator Requirements

- **Network**: 100+ Mbps recommended
- **Storage**: 20+ GB/day for blob retention
- **CPU**: Multi-core for parallel KZG verification
- **RAM**: 8+ GB recommended

---

## Future Considerations

### Increasing the Limit

To increase from 1024 to higher values:

1. **Update constant**: Change `MAX_BLOBS_PER_BLOCK` in `blob.rs`
2. **Test network load**: Validate validator performance
3. **Monitor storage**: Ensure adequate retention capacity
4. **Coordinate upgrade**: All validators must update

### Maximum Theoretical Limit

The hard ceiling is **4096 blobs** due to:
- SSZ `List[KZGCommitment, MAX_BLOB_COMMITMENTS_PER_BLOCK]` capacity
- Merkle tree depth calculations
- Would require protocol-breaking changes to exceed

---

## References

- **EIP-4844 Spec**: [ethereum/EIPs/blob/master/EIPS/eip-4844.md](https://eips.ethereum.org/EIPS/eip-4844)
- **Consensus Spec**: `consensus-specs/specs/deneb/beacon-chain.md`
- **Blob Engine**: `crates/blob_engine/src/engine.rs`
- **Storage Implementation**: `crates/blob_engine/src/store/rocksdb.rs`

---

## Summary

✅ **Current Configuration**: 1024 blobs/block (~134 MB)
✅ **SSZ Capacity**: 4096 blobs (future growth headroom)
✅ **Production Ready**: Manageable validator load
✅ **Future-Proof**: Can scale to 4× current capacity without breaking changes
