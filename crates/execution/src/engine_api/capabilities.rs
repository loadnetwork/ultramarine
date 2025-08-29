// crates/execution/src/engine_api/capabilities.rs

use std::{collections::HashSet, hash::Hash, time::Duration};

use crate::engine_api::capabilities;

pub const ENGINE_NEW_PAYLOAD_V1: &str = "engine_newPayloadV1";
pub const ENGINE_NEW_PAYLOAD_V2: &str = "engine_newPayloadV2";
pub const ENGINE_NEW_PAYLOAD_V3: &str = "engine_newPayloadV3";
pub const ENGINE_NEW_PAYLOAD_V4: &str = "engine_newPayloadV4";
pub const ENGINE_NEW_PAYLOAD_TIMEOUT: Duration = Duration::from_secs(8);

pub const ENGINE_GET_PAYLOAD_V1: &str = "engine_getPayloadV1";
pub const ENGINE_GET_PAYLOAD_V2: &str = "engine_getPayloadV2";
pub const ENGINE_GET_PAYLOAD_V3: &str = "engine_getPayloadV3";
pub const ENGINE_GET_PAYLOAD_V4: &str = "engine_getPayloadV4";
pub const ENGINE_GET_PAYLOAD_TIMEOUT: Duration = Duration::from_secs(2);

pub const ENGINE_FORKCHOICE_UPDATED_V1: &str = "engine_forkchoiceUpdatedV1";
pub const ENGINE_FORKCHOICE_UPDATED_V2: &str = "engine_forkchoiceUpdatedV2";
pub const ENGINE_FORKCHOICE_UPDATED_V3: &str = "engine_forkchoiceUpdatedV3";
pub const ENGINE_FORKCHOICE_UPDATED_TIMEOUT: Duration = Duration::from_secs(8);

pub const ENGINE_GET_PAYLOAD_BODIES_BY_HASH_V1: &str = "engine_getPayloadBodiesByHashV1";
pub const ENGINE_GET_PAYLOAD_BODIES_BY_RANGE_V1: &str = "engine_getPayloadBodiesByRangeV1";
pub const ENGINE_GET_PAYLOAD_BODIES_TIMEOUT: Duration = Duration::from_secs(10);

pub const ENGINE_EXCHANGE_CAPABILITIES: &str = "engine_exchangeCapabilities";
pub const ENGINE_EXCHANGE_CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(1);

pub const ENGINE_GET_CLIENT_VERSION_V1: &str = "engine_getClientVersionV1";
pub const ENGINE_GET_CLIENT_VERSION_TIMEOUT: Duration = Duration::from_secs(1);

pub const ENGINE_GET_BLOBS_V1: &str = "engine_getBlobsV1";
pub const ENGINE_GET_BLOBS_TIMEOUT: Duration = Duration::from_secs(1);

// Engine API methods supported by this implementation
pub static ULTRAMARINE_CAPABILITIES: &[&str] = &[
    // ENGINE_NEW_PAYLOAD_V1,
    // ENGINE_NEW_PAYLOAD_V2,
    ENGINE_NEW_PAYLOAD_V3,
    // ENGINE_NEW_PAYLOAD_V4,
    // ENGINE_GET_PAYLOAD_V1,
    // ENGINE_GET_PAYLOAD_V2,
    ENGINE_GET_PAYLOAD_V3,
    // ENGINE_GET_PAYLOAD_V4,
    // ENGINE_FORKCHOICE_UPDATED_V1,
    // ENGINE_FORKCHOICE_UPDATED_V2,
    ENGINE_FORKCHOICE_UPDATED_V3,
    // ENGINE_GET_PAYLOAD_BODIES_BY_HASH_V1,
    // ENGINE_GET_PAYLOAD_BODIES_BY_RANGE_V1,
    // ENGINE_GET_CLIENT_VERSION_V1,
    // ENGINE_GET_BLOBS_V1,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineCapabilities {
    pub new_payload_v1: bool,
    pub new_payload_v2: bool,
    pub new_payload_v3: bool,
    pub new_payload_v4: bool,
    pub forkchoice_updated_v1: bool,
    pub forkchoice_updated_v2: bool,
    pub forkchoice_updated_v3: bool,
    pub get_payload_bodies_by_hash_v1: bool,
    pub get_payload_bodies_by_range_v1: bool,
    pub get_payload_v1: bool,
    pub get_payload_v2: bool,
    pub get_payload_v3: bool,
    pub get_payload_v4: bool,
    pub get_client_version_v1: bool,
    pub get_blobs_v1: bool,
}

impl EngineCapabilities {
    pub fn from_response_strings(capabilities: HashSet<String>) -> Self {
        Self {
            new_payload_v1: capabilities.contains(ENGINE_NEW_PAYLOAD_V1),
            new_payload_v2: capabilities.contains(ENGINE_NEW_PAYLOAD_V2),
            new_payload_v3: capabilities.contains(ENGINE_NEW_PAYLOAD_V3),
            new_payload_v4: capabilities.contains(ENGINE_NEW_PAYLOAD_V4),
            forkchoice_updated_v1: capabilities.contains(ENGINE_FORKCHOICE_UPDATED_V1),
            forkchoice_updated_v2: capabilities.contains(ENGINE_FORKCHOICE_UPDATED_V2),
            forkchoice_updated_v3: capabilities.contains(ENGINE_FORKCHOICE_UPDATED_V3),
            get_payload_bodies_by_hash_v1: capabilities
                .contains(ENGINE_GET_PAYLOAD_BODIES_BY_HASH_V1),
            get_payload_bodies_by_range_v1: capabilities
                .contains(ENGINE_GET_PAYLOAD_BODIES_BY_RANGE_V1),
            get_payload_v1: capabilities.contains(ENGINE_GET_PAYLOAD_V1),
            get_payload_v2: capabilities.contains(ENGINE_GET_PAYLOAD_V2),
            get_payload_v3: capabilities.contains(ENGINE_GET_PAYLOAD_V3),
            get_payload_v4: capabilities.contains(ENGINE_GET_PAYLOAD_V4),
            get_client_version_v1: capabilities.contains(ENGINE_GET_CLIENT_VERSION_V1),
            get_blobs_v1: capabilities.contains(ENGINE_GET_BLOBS_V1),
        }
    }
}

// Capabilities from lighthouse, full implememntation reference:
//
// pub const ENGINE_NEW_PAYLOAD_V1: &str = "engine_newPayloadV1";
// pub const ENGINE_NEW_PAYLOAD_V2: &str = "engine_newPayloadV2";
// pub const ENGINE_NEW_PAYLOAD_V3: &str = "engine_newPayloadV3";
// pub const ENGINE_NEW_PAYLOAD_V4: &str = "engine_newPayloadV4";
// pub const ENGINE_NEW_PAYLOAD_V5: &str = "engine_newPayloadV5";
// pub const ENGINE_NEW_PAYLOAD_TIMEOUT: Duration = Duration::from_secs(8);
//
// pub const ENGINE_GET_PAYLOAD_V1: &str = "engine_getPayloadV1";
// pub const ENGINE_GET_PAYLOAD_V2: &str = "engine_getPayloadV2";
// pub const ENGINE_GET_PAYLOAD_V3: &str = "engine_getPayloadV3";
// pub const ENGINE_GET_PAYLOAD_V4: &str = "engine_getPayloadV4";
// pub const ENGINE_GET_PAYLOAD_V5: &str = "engine_getPayloadV5";
// pub const ENGINE_GET_PAYLOAD_TIMEOUT: Duration = Duration::from_secs(2);
//
// pub const ENGINE_FORKCHOICE_UPDATED_V1: &str = "engine_forkchoiceUpdatedV1";
// pub const ENGINE_FORKCHOICE_UPDATED_V2: &str = "engine_forkchoiceUpdatedV2";
// pub const ENGINE_FORKCHOICE_UPDATED_V3: &str = "engine_forkchoiceUpdatedV3";
// pub const ENGINE_FORKCHOICE_UPDATED_TIMEOUT: Duration = Duration::from_secs(8);
//
// pub const ENGINE_GET_PAYLOAD_BODIES_BY_HASH_V1: &str = "engine_getPayloadBodiesByHashV1";
// pub const ENGINE_GET_PAYLOAD_BODIES_BY_RANGE_V1: &str = "engine_getPayloadBodiesByRangeV1";
// pub const ENGINE_GET_PAYLOAD_BODIES_TIMEOUT: Duration = Duration::from_secs(10);
//
// pub const ENGINE_EXCHANGE_CAPABILITIES: &str = "engine_exchangeCapabilities";
// pub const ENGINE_EXCHANGE_CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(1);
//
// pub const ENGINE_GET_CLIENT_VERSION_V1: &str = "engine_getClientVersionV1";
// pub const ENGINE_GET_CLIENT_VERSION_TIMEOUT: Duration = Duration::from_secs(1);
//
// pub const ENGINE_GET_BLOBS_V1: &str = "engine_getBlobsV1";
// pub const ENGINE_GET_BLOBS_V2: &str = "engine_getBlobsV2";
// pub const ENGINE_GET_BLOBS_TIMEOUT: Duration = Duration::from_secs(1);
//
// This error is returned during a `chainId` call by Geth.
// pub const EIP155_ERROR_STR: &str = "chain not synced beyond EIP-155 replay-protection fork
// block"; This code is returned by all clients when a method is not supported
// (verified geth, nethermind, erigon, besu)
// pub const METHOD_NOT_FOUND_CODE: i64 = -32601;
//
// pub static LIGHTHOUSE_CAPABILITIES: &[&str] = &[
// ENGINE_NEW_PAYLOAD_V1,
// ENGINE_NEW_PAYLOAD_V2,
// ENGINE_NEW_PAYLOAD_V3,
// ENGINE_NEW_PAYLOAD_V4,
// ENGINE_NEW_PAYLOAD_V5,
// ENGINE_GET_PAYLOAD_V1,
// ENGINE_GET_PAYLOAD_V2,
// ENGINE_GET_PAYLOAD_V3,
// ENGINE_GET_PAYLOAD_V4,
// ENGINE_GET_PAYLOAD_V5,
// ENGINE_FORKCHOICE_UPDATED_V1,
// ENGINE_FORKCHOICE_UPDATED_V2,
// ENGINE_FORKCHOICE_UPDATED_V3,
// ENGINE_GET_PAYLOAD_BODIES_BY_HASH_V1,
// ENGINE_GET_PAYLOAD_BODIES_BY_RANGE_V1,
// ENGINE_GET_CLIENT_VERSION_V1,
// ENGINE_GET_BLOBS_V1,
// ENGINE_GET_BLOBS_V2,
// ];
//
