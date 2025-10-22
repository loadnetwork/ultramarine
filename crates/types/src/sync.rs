use bytes::Bytes;
use malachitebft_proto::{Error as ProtoError, Protobuf};
use prost::Message;

use crate::{proposal_part::BlobSidecar, proto};

/// Versioned payload embedded inside `Value.extensions` during state sync.
///
/// Carries the full execution payload SSZ bytes plus any blob sidecars so that
/// lagging peers can reconstruct decided blocks without requiring additional
/// Malachite RPCs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncedValueExtensions {
    /// Incremented when the encoding changes.
    pub version: u32,
    /// SSZ-encoded execution payload corresponding to the decided value.
    pub execution_payload_ssz: Bytes,
    /// All blob sidecars associated with the decided value.
    pub blob_sidecars: Vec<BlobSidecar>,
}

impl SyncedValueExtensions {
    /// First version of the encoding.
    pub const CURRENT_VERSION: u32 = 1;

    /// Creates a new payload using the current encoding version.
    pub fn new(execution_payload_ssz: Bytes, blob_sidecars: Vec<BlobSidecar>) -> Self {
        Self { version: Self::CURRENT_VERSION, execution_payload_ssz, blob_sidecars }
    }

    /// Encode the payload into bytes suitable for storing in `Value.extensions`.
    pub fn encode(&self) -> Result<Bytes, ProtoError> {
        let proto = self.to_proto()?;
        Ok(Bytes::from(proto.encode_to_vec()))
    }

    /// Decode a payload that was previously serialized with [`Self::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let proto = proto::SyncedValueExtensions::decode(bytes)?;
        Self::from_proto(proto)
    }
}

impl Protobuf for SyncedValueExtensions {
    type Proto = proto::SyncedValueExtensions;

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        if proto.version != Self::CURRENT_VERSION {
            return Err(ProtoError::Other(
                format!(
                    "unsupported SyncedValueExtensions version: {}, expected {}",
                    proto.version,
                    Self::CURRENT_VERSION
                )
                .into(),
            ));
        }

        let blob_sidecars = proto
            .blob_sidecars
            .into_iter()
            .map(BlobSidecar::from_proto)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            version: proto.version,
            execution_payload_ssz: proto.execution_payload_ssz,
            blob_sidecars,
        })
    }

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        let blob_sidecars =
            self.blob_sidecars.iter().map(BlobSidecar::to_proto).collect::<Result<Vec<_>, _>>()?;

        Ok(Self::Proto {
            version: self.version,
            execution_payload_ssz: self.execution_payload_ssz.clone(),
            blob_sidecars,
        })
    }
}
