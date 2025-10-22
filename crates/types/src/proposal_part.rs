use core::fmt;

use bytes::Bytes;
use malachitebft_core_types::Round;
use malachitebft_proto::{self as proto, Error as ProtoError, Protobuf};
use malachitebft_signing_ed25519::Signature;
use prost::Message;
use serde::{Deserialize, Serialize};

use crate::{
    address::Address,
    // Phase 3: Import blob types for BlobSidecar
    blob::{BYTES_PER_BLOB, Blob, KzgCommitment, KzgProof},
    codec::proto::{decode_signature, encode_signature},
    context::LoadContext,
    height::Height,
};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalData {
    pub bytes: Bytes,
}

impl ProposalData {
    pub fn new(bytes: Bytes) -> Self {
        Self { bytes }
    }

    pub fn size_bytes(&self) -> usize {
        std::mem::size_of::<u64>()
    }
}

impl fmt::Debug for ProposalData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProposalData")
            .field("bytes", &"<...>")
            .field("len", &self.bytes.len())
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "Round")]
enum RoundDef {
    Nil,
    Some(u32),
}

/// A blob sidecar containing blob data and cryptographic proofs (Phase 3: EIP-4844)
///
/// This is streamed separately from the execution payload to keep consensus messages small.
/// Each blob sidecar contains:
/// - The actual blob data (131,072 bytes)
/// - KZG commitment (48 bytes)
/// - KZG proof (48 bytes) for verification
///
/// ## Purpose
///
/// BlobSidecars are streamed as separate ProposalPart messages to:
/// 1. Keep consensus messages lightweight (ValueMetadata only contains commitments)
/// 2. Allow validators to verify blobs before voting
/// 3. Enable efficient bandwidth usage (blobs are large)
///
/// ## Network Flow
///
/// ```text
/// Proposer                           Validators
///    │                                   │
///    ├──> ProposalPart::Init            ├──> Store metadata
///    ├──> ProposalPart::Data            ├──> Store execution payload
///    ├──> ProposalPart::BlobSidecar(0)  ├──> Store & verify blob 0
///    ├──> ProposalPart::BlobSidecar(1)  ├──> Store & verify blob 1
///    ├──> ...                            ├──> ...
///    └──> ProposalPart::Fin             └──> Vote if all verified
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobSidecar {
    /// Index of this blob (0-5 for Deneb, 0-8 for Electra)
    ///
    /// This index corresponds to the position in ValueMetadata.blob_kzg_commitments.
    /// It's used to match blobs with their commitments during verification.
    pub index: u8,

    /// The blob data (exactly 131,072 bytes)
    ///
    /// This is the actual data payload. Validators will verify this matches
    /// the commitment using the KZG proof.
    pub blob: Blob,

    /// KZG commitment for this blob (48 bytes)
    ///
    /// This must match the commitment at `ValueMetadata.blob_kzg_commitments[index]`.
    /// Validators will check this during assembly.
    pub kzg_commitment: KzgCommitment,

    /// KZG proof for verification (48 bytes)
    ///
    /// This proof allows validators to verify that the blob data matches
    /// the commitment without trusting the proposer.
    pub kzg_proof: KzgProof,
}

impl BlobSidecar {
    /// Creates a new BlobSidecar
    pub fn new(index: u8, blob: Blob, kzg_commitment: KzgCommitment, kzg_proof: KzgProof) -> Self {
        Self { index, blob, kzg_commitment, kzg_proof }
    }

    /// Calculate size in bytes for this sidecar
    ///
    /// Size breakdown:
    /// - index: 1 byte
    /// - blob: 131,072 bytes
    /// - commitment: 48 bytes
    /// - proof: 48 bytes
    /// Total: ~131,169 bytes
    pub fn size_bytes(&self) -> usize {
        1 + BYTES_PER_BLOB + 48 + 48
    }
}

impl Protobuf for BlobSidecar {
    type Proto = proto::BlobSidecar;

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        // Convert bytes::Bytes to alloy_primitives::Bytes for Blob::new
        let blob_bytes = crate::aliases::Bytes::from(proto.blob.to_vec());

        let blob = Blob::new(blob_bytes).map_err(|e| ProtoError::Other(e.into()))?;
        let kzg_commitment = KzgCommitment::from_slice(&proto.kzg_commitment)
            .map_err(|e| ProtoError::Other(e.into()))?;
        let kzg_proof =
            KzgProof::from_slice(&proto.kzg_proof).map_err(|e| ProtoError::Other(e.into()))?;

        Ok(Self { index: proto.index as u8, blob, kzg_commitment, kzg_proof })
    }

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        let blob_bytes: ::bytes::Bytes = self.blob.data().to_vec().into();

        Ok(Self::Proto {
            index: self.index as u32,
            blob: blob_bytes,
            kzg_commitment: self.kzg_commitment.as_bytes().to_vec().into(),
            kzg_proof: self.kzg_proof.as_bytes().to_vec().into(),
        })
    }
}

/// ProposalPart represents a single part of a proposal stream
///
/// ## Phase 3: Extended with BlobSidecar Support
///
/// Proposals are broken down into parts and streamed over the network:
///
/// 1. **Init**: Metadata (height, round, proposer)
/// 2. **Data**: Execution payload (transactions, receipts, etc.)
/// 3. **BlobSidecar** (NEW): Blob data with KZG proofs (one per blob)
/// 4. **Fin**: Signature over all parts
///
/// This streaming approach allows:
/// - Large proposals (with blobs) to be transmitted efficiently
/// - Validators to start verification before receiving complete proposal
/// - Network bandwidth to be used optimally
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalPart {
    Init(ProposalInit),
    Data(ProposalData),
    /// Phase 3: Blob sidecar with KZG proof (EIP-4844)
    BlobSidecar(BlobSidecar),
    Fin(ProposalFin),
}

impl ProposalPart {
    pub fn get_type(&self) -> &'static str {
        match self {
            Self::Init(_) => "init",
            Self::Data(_) => "data",
            Self::BlobSidecar(_) => "blob_sidecar",
            Self::Fin(_) => "fin",
        }
    }

    pub fn as_init(&self) -> Option<&ProposalInit> {
        match self {
            Self::Init(init) => Some(init),
            _ => None,
        }
    }

    pub fn as_data(&self) -> Option<&ProposalData> {
        match self {
            Self::Data(data) => Some(data),
            _ => None,
        }
    }

    /// Phase 3: Accessor for BlobSidecar variant
    pub fn as_blob_sidecar(&self) -> Option<&BlobSidecar> {
        match self {
            Self::BlobSidecar(sidecar) => Some(sidecar),
            _ => None,
        }
    }

    pub fn to_sign_bytes(&self) -> Bytes {
        proto::Protobuf::to_bytes(self).unwrap()
    }

    pub fn size_bytes(&self) -> usize {
        // original: self.to_sign_bytes().len() // FIXME: This is dumb

        // our fix, to check
        // First, convert to the generated protobuf struct. This is cheap.
        // The unwrap is kept, as the original code also assumed this succeeds.
        let proto_msg = self.to_proto().unwrap();

        // Now, call the efficient size calculation method.
        // This calculates the length without allocating the byte buffer.
        proto_msg.encoded_len()
    }
}

/// A part of a value for a height, round. Identified in this scope by the sequence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalInit {
    pub height: Height,
    #[serde(with = "RoundDef")]
    pub round: Round,
    pub proposer: Address,
}

impl ProposalInit {
    pub fn new(height: Height, round: Round, proposer: Address) -> Self {
        Self { height, round, proposer }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalFin {
    pub signature: Signature,
}

impl ProposalFin {
    pub fn new(signature: Signature) -> Self {
        Self { signature }
    }
}

impl malachitebft_core_types::ProposalPart<LoadContext> for ProposalPart {
    fn is_first(&self) -> bool {
        matches!(self, Self::Init(_))
    }

    fn is_last(&self) -> bool {
        matches!(self, Self::Fin(_))
    }
}

impl Protobuf for ProposalPart {
    type Proto = crate::proto::ProposalPart;

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        use crate::proto::proposal_part::Part;

        let part = proto.part.ok_or_else(|| ProtoError::missing_field::<Self::Proto>("part"))?;

        match part {
            Part::Init(init) => Ok(Self::Init(ProposalInit {
                height: Height::new(init.height),
                round: Round::new(init.round),
                proposer: init
                    .proposer
                    .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("proposer"))
                    .and_then(Address::from_proto)?,
            })),
            Part::Data(data) => Ok(Self::Data(ProposalData::new(data.bytes))),

            // Phase 3: Deserialize BlobSidecar from protobuf
            Part::BlobSidecar(sidecar) => Ok(Self::BlobSidecar(BlobSidecar::from_proto(sidecar)?)),

            Part::Fin(fin) => Ok(Self::Fin(ProposalFin {
                signature: fin
                    .signature
                    .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("signature"))
                    .and_then(decode_signature)?,
            })),
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        use crate::{proto, proto::proposal_part::Part};

        match self {
            Self::Init(init) => Ok(Self::Proto {
                part: Some(Part::Init(proto::ProposalInit {
                    height: init.height.as_u64(),
                    round: init.round.as_u32().unwrap(),
                    proposer: Some(init.proposer.to_proto()?),
                })),
            }),
            Self::Data(data) => Ok(Self::Proto {
                part: Some(Part::Data(proto::ProposalData { bytes: data.bytes.clone() })),
            }),

            // Phase 3: Serialize BlobSidecar to protobuf
            Self::BlobSidecar(sidecar) => {
                Ok(Self::Proto { part: Some(Part::BlobSidecar(sidecar.to_proto()?)) })
            }

            Self::Fin(fin) => Ok(Self::Proto {
                part: Some(Part::Fin(proto::ProposalFin {
                    signature: Some(encode_signature(&fin.signature)),
                })),
            }),
        }
    }
}
