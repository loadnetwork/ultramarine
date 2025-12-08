use core::{convert::TryFrom, fmt};

use bytes::Bytes;
use malachitebft_core_types::Round;
use malachitebft_proto::{self as proto, Error as ProtoError, Protobuf};
use malachitebft_signing_ed25519::Signature;
use prost::Message;
use serde::{Deserialize, Serialize};

use crate::{
    address::Address,
    // Phase 3: Import blob types for BlobSidecar
    aliases::Bytes as AlloyBytes,
    blob::{BYTES_PER_BLOB, Blob, KzgCommitment, KzgProof},
    codec::proto::{decode_signature, encode_signature},
    context::LoadContext,
    height::Height,
};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalData {
    pub bytes: Bytes,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub execution_requests: Vec<AlloyBytes>,
}

impl ProposalData {
    pub fn new(bytes: Bytes) -> Self {
        Self { bytes, execution_requests: Vec::new() }
    }

    pub fn with_execution_requests(bytes: Bytes, execution_requests: Vec<AlloyBytes>) -> Self {
        Self { bytes, execution_requests }
    }

    pub fn has_execution_requests(&self) -> bool {
        !self.execution_requests.is_empty()
    }

    pub fn size_bytes(&self) -> usize {
        std::mem::size_of::<u64>() + self.execution_requests.iter().map(|r| r.len()).sum::<usize>()
    }
}

impl fmt::Debug for ProposalData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProposalData")
            .field("bytes", &"<...>")
            .field("len", &self.bytes.len())
            .field("execution_requests", &self.execution_requests.len())
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "Round")]
enum RoundDef {
    Nil,
    Some(u32),
}

/// A blob sidecar containing blob data and cryptographic proofs
///
/// **Phase 4**: Extended with Ethereum Deneb compatibility fields:
/// - SignedBeaconBlockHeader (for proposer authentication)
/// - KZG commitment inclusion proof (17-branch Merkle proof)
///
/// This is streamed separately from the execution payload to keep consensus messages small.
/// Each blob sidecar contains:
/// - The actual blob data (131,072 bytes)
/// - KZG commitment (48 bytes)
/// - KZG proof (48 bytes) for verification
/// - Signed beacon block header (proposer signature)
/// - Merkle inclusion proof (17 × 32 = 544 bytes)
///
/// ## Purpose
///
/// BlobSidecars are streamed as separate ProposalPart messages to:
/// 1. Keep consensus messages lightweight (ValueMetadata only contains commitments)
/// 2. Allow validators to verify blobs before voting
/// 3. Enable efficient bandwidth usage (blobs are large)
/// 4. Provide Ethereum Deneb spec compatibility for tooling
///
/// ## Network Flow
///
/// ```text
/// Proposer                           Validators
///    │                                   │
///    ├──> ProposalPart::Init            ├──> Store metadata
///    ├──> ProposalPart::Data            ├──> Store execution payload
///    ├──> ProposalPart::BlobSidecar(0)  ├──> Verify signature + proof + KZG
///    ├──> ProposalPart::BlobSidecar(1)  ├──> Verify signature + proof + KZG
///    ├──> ...                            ├──> ...
///    └──> ProposalPart::Fin             └──> Vote if all verified
/// ```
///
/// ## Verification Layers (Phase 4)
///
/// 1. **Signature**: Verify signed_block_header with proposer's public key
/// 2. **Merkle Proof**: Verify kzg_commitment_inclusion_proof against body_root
/// 3. **KZG Proof**: Verify blob data matches kzg_commitment
///
/// ## Size
///
/// - Phase 3: ~131,169 bytes (blob + commitment + proof)
/// - Phase 4: ~132,921 bytes (+1,752 bytes for header + proof)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobSidecar {
    /// Index of this blob (0-5 for Deneb, 0-8 for Electra, 0-1023 for Ultramarine)
    ///
    /// This index corresponds to the position in ValueMetadata.blob_kzg_commitments.
    /// It's used to match blobs with their commitments during verification.
    pub index: u16,

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

    /// Signed beacon block header (Phase 4: Deneb compatibility)
    ///
    /// Contains:
    /// - BeaconBlockHeader with body_root that commits to all blob commitments
    /// - Ed25519 signature from the proposer (Ultramarine uses Ed25519, not BLS)
    ///
    /// This provides proposer authentication and links the blob to the beacon block.
    /// Size: ~192 bytes (header: 5 × 32 bytes + signature: 64 bytes)
    pub signed_block_header: crate::ethereum_compat::SignedBeaconBlockHeader,

    /// KZG commitment inclusion proof (Phase 4: Deneb compatibility)
    ///
    /// A 17-branch Merkle proof that proves:
    /// 1. This kzg_commitment is in the blob_kzg_commitments list at position `index`
    /// 2. The blob_kzg_commitments list is in BeaconBlockBody at field index 11
    /// 3. The BeaconBlockBody hashes to `signed_block_header.message.body_root`
    ///
    /// This prevents a malicious proposer from sending the wrong commitment for a block.
    /// Size: 17 × 32 = 544 bytes
    pub kzg_commitment_inclusion_proof: Vec<alloy_primitives::B256>,
}

impl BlobSidecar {
    /// Creates a new BlobSidecar (Phase 4: with header + proof)
    pub fn new(
        index: u16,
        blob: Blob,
        kzg_commitment: KzgCommitment,
        kzg_proof: KzgProof,
        signed_block_header: crate::ethereum_compat::SignedBeaconBlockHeader,
        kzg_commitment_inclusion_proof: Vec<alloy_primitives::B256>,
    ) -> Self {
        Self {
            index,
            blob,
            kzg_commitment,
            kzg_proof,
            signed_block_header,
            kzg_commitment_inclusion_proof,
        }
    }

    /// Creates a BlobSidecar with placeholder Phase 4 fields (temporary)
    ///
    /// This constructor is used during Phase 4 transition to allow Phase 3 code
    /// to continue working. Once Phase 4 is complete, all callsites should be
    /// updated to use `new()` with proper header and inclusion proof.
    ///
    /// TODO: Remove this method once Phase 4 is fully implemented
    /// https://github.com/loadnetwork/ultramarine/issues/TBD
    pub fn from_bundle_item(
        index: u16,
        blob: Blob,
        kzg_commitment: KzgCommitment,
        kzg_proof: KzgProof,
    ) -> Self {
        // Create placeholder signed block header
        let placeholder_header = crate::ethereum_compat::BeaconBlockHeader::new(
            0,                            // slot
            0,                            // proposer_index
            alloy_primitives::B256::ZERO, // parent_root
            alloy_primitives::B256::ZERO, // state_root
            alloy_primitives::B256::ZERO, // body_root
        );

        // Create placeholder signature (64 zero bytes)
        let placeholder_signature = Signature::from_bytes([0u8; 64]);

        let signed_block_header = crate::ethereum_compat::SignedBeaconBlockHeader::new(
            placeholder_header,
            placeholder_signature,
        );

        Self {
            index,
            blob,
            kzg_commitment,
            kzg_proof,
            signed_block_header,
            kzg_commitment_inclusion_proof: Vec::new(), // Empty proof vector
        }
    }

    /// Calculate size in bytes for this sidecar
    ///
    /// Size breakdown (Phase 4):
    /// - index: 2 bytes
    /// - blob: 131,072 bytes
    /// - commitment: 48 bytes
    /// - proof: 48 bytes
    /// - signed_block_header: ~192 bytes (5 × 32 + 64)
    /// - kzg_commitment_inclusion_proof: 544 bytes (17 × 32)
    /// Total: ~132,905 bytes
    pub fn size_bytes(&self) -> usize {
        2 + BYTES_PER_BLOB + 48 + 48 + 192 + (self.kzg_commitment_inclusion_proof.len() * 32)
    }
}

/// Protobuf conversion for BlobSidecar (Phase 4)
///
/// Enables standalone serialization/deserialization for use in SyncedValuePackage
impl malachitebft_proto::Protobuf for BlobSidecar {
    type Proto = crate::proto::BlobSidecar;

    fn from_proto(proto: Self::Proto) -> Result<Self, malachitebft_proto::Error> {
        // Convert bytes::Bytes to alloy_primitives::Bytes for Blob::new
        let blob_bytes = crate::aliases::Bytes::from(proto.blob.to_vec());
        let blob = Blob::new(blob_bytes).map_err(|e| ProtoError::Other(e))?;

        let kzg_commitment =
            KzgCommitment::from_slice(&proto.kzg_commitment).map_err(|e| ProtoError::Other(e))?;

        let kzg_proof = KzgProof::from_slice(&proto.kzg_proof).map_err(|e| ProtoError::Other(e))?;

        let signed_block_header = proto
            .signed_block_header
            .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("signed_block_header"))
            .and_then(|proto_header| {
                let message = proto_header
                    .message
                    .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("message"))?;

                let header = crate::ethereum_compat::BeaconBlockHeader::new(
                    message.slot,
                    message.proposer_index,
                    alloy_primitives::B256::from_slice(&message.parent_root),
                    alloy_primitives::B256::from_slice(&message.state_root),
                    alloy_primitives::B256::from_slice(&message.body_root),
                );

                let signature = proto_header
                    .signature
                    .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("signature"))
                    .and_then(|sig| {
                        let bytes = <[u8; 64]>::try_from(sig.bytes.as_ref()).map_err(|_| {
                            ProtoError::Other("Invalid signature length".to_string())
                        })?;
                        Ok(Signature::from_bytes(bytes))
                    })?;

                Ok(crate::ethereum_compat::SignedBeaconBlockHeader::new(header, signature))
            })?;

        let kzg_commitment_inclusion_proof: Vec<alloy_primitives::B256> = proto
            .kzg_commitment_inclusion_proof
            .iter()
            .map(|bytes| alloy_primitives::B256::from_slice(bytes))
            .collect();

        let index = u16::try_from(proto.index).map_err(|_| {
            ProtoError::Other(format!("Blob index {} exceeds u16::MAX", proto.index))
        })?;

        Ok(Self {
            index,
            blob,
            kzg_commitment,
            kzg_proof,
            signed_block_header,
            kzg_commitment_inclusion_proof,
        })
    }

    fn to_proto(&self) -> Result<Self::Proto, malachitebft_proto::Error> {
        // Convert SignedBeaconBlockHeader to proto
        let signed_block_header = crate::proto::SignedBeaconBlockHeader {
            message: Some(crate::proto::BeaconBlockHeader {
                slot: self.signed_block_header.message.slot,
                proposer_index: self.signed_block_header.message.proposer_index,
                parent_root: self.signed_block_header.message.parent_root.to_vec().into(),
                state_root: self.signed_block_header.message.state_root.to_vec().into(),
                body_root: self.signed_block_header.message.body_root.to_vec().into(),
            }),
            signature: Some(crate::proto::Signature {
                bytes: Bytes::copy_from_slice(
                    self.signed_block_header.signature.to_bytes().as_ref(),
                ),
            }),
        };

        Ok(crate::proto::BlobSidecar {
            index: self.index as u32,
            blob: Bytes::copy_from_slice(self.blob.data()),
            kzg_commitment: Bytes::from(self.kzg_commitment.as_bytes().to_vec()),
            kzg_proof: Bytes::from(self.kzg_proof.as_bytes().to_vec()),
            signed_block_header: Some(signed_block_header),
            kzg_commitment_inclusion_proof: self
                .kzg_commitment_inclusion_proof
                .iter()
                .map(|b256| Bytes::copy_from_slice(b256.as_slice()))
                .collect(),
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
            Part::Data(data) => {
                let execution_requests =
                    data.execution_requests.into_iter().map(AlloyBytes::from).collect();
                Ok(Self::Data(ProposalData { bytes: data.bytes, execution_requests }))
            }

            // Phase 3/4: Deserialize BlobSidecar from protobuf
            Part::BlobSidecar(sidecar) => {
                // Convert bytes::Bytes to alloy_primitives::Bytes for Blob::new
                let blob_bytes = crate::aliases::Bytes::from(sidecar.blob.to_vec());

                // Deserialize blob data (must be exactly 131,072 bytes)
                let blob = Blob::new(blob_bytes).map_err(|e| ProtoError::Other(e))?;

                // Deserialize KZG commitment (must be exactly 48 bytes)
                let kzg_commitment = KzgCommitment::from_slice(&sidecar.kzg_commitment)
                    .map_err(|e| ProtoError::Other(e))?;

                // Deserialize KZG proof (must be exactly 48 bytes)
                let kzg_proof =
                    KzgProof::from_slice(&sidecar.kzg_proof).map_err(|e| ProtoError::Other(e))?;

                // Phase 4: Deserialize SignedBeaconBlockHeader
                let signed_block_header = sidecar
                    .signed_block_header
                    .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("signed_block_header"))
                    .and_then(|proto_header| {
                        let message = proto_header
                            .message
                            .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("message"))?;

                        let header = crate::ethereum_compat::BeaconBlockHeader::new(
                            message.slot,
                            message.proposer_index,
                            alloy_primitives::B256::from_slice(&message.parent_root),
                            alloy_primitives::B256::from_slice(&message.state_root),
                            alloy_primitives::B256::from_slice(&message.body_root),
                        );

                        let signature = proto_header
                            .signature
                            .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("signature"))
                            .and_then(decode_signature)?;

                        Ok(crate::ethereum_compat::SignedBeaconBlockHeader::new(header, signature))
                    })?;

                // Phase 4: Deserialize kzg_commitment_inclusion_proof
                let kzg_commitment_inclusion_proof: Vec<alloy_primitives::B256> = sidecar
                    .kzg_commitment_inclusion_proof
                    .iter()
                    .map(|bytes| alloy_primitives::B256::from_slice(bytes))
                    .collect();

                let index = u16::try_from(sidecar.index).map_err(|_| {
                    ProtoError::Other(format!(
                        "Blob index {} exceeds u16::MAX in ProposalPart::BlobSidecar",
                        sidecar.index
                    ))
                })?;

                Ok(Self::BlobSidecar(BlobSidecar {
                    index,
                    blob,
                    kzg_commitment,
                    kzg_proof,
                    signed_block_header,
                    kzg_commitment_inclusion_proof,
                }))
            }

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
                part: Some(Part::Data(proto::ProposalData {
                    bytes: data.bytes.clone(),
                    execution_requests: data
                        .execution_requests
                        .iter()
                        .cloned()
                        .map(|req| req.0)
                        .collect(),
                })),
            }),

            // Phase 3/4: Serialize BlobSidecar to protobuf
            Self::BlobSidecar(sidecar) => {
                // Convert alloy_primitives::Bytes to bytes::Bytes for protobuf
                let blob_bytes: ::bytes::Bytes = sidecar.blob.data().to_vec().into();

                // Phase 4: Serialize SignedBeaconBlockHeader
                let proto_header = proto::SignedBeaconBlockHeader {
                    message: Some(proto::BeaconBlockHeader {
                        slot: sidecar.signed_block_header.message.slot,
                        proposer_index: sidecar.signed_block_header.message.proposer_index,
                        parent_root: sidecar
                            .signed_block_header
                            .message
                            .parent_root
                            .to_vec()
                            .into(),
                        state_root: sidecar.signed_block_header.message.state_root.to_vec().into(),
                        body_root: sidecar.signed_block_header.message.body_root.to_vec().into(),
                    }),
                    signature: Some(encode_signature(&sidecar.signed_block_header.signature)),
                };

                // Phase 4: Serialize kzg_commitment_inclusion_proof
                let kzg_commitment_inclusion_proof: Vec<::bytes::Bytes> = sidecar
                    .kzg_commitment_inclusion_proof
                    .iter()
                    .map(|b256| b256.to_vec().into())
                    .collect();

                Ok(Self::Proto {
                    part: Some(Part::BlobSidecar(proto::BlobSidecar {
                        index: sidecar.index as u32,
                        blob: blob_bytes,
                        kzg_commitment: sidecar.kzg_commitment.as_bytes().to_vec().into(),
                        kzg_proof: sidecar.kzg_proof.as_bytes().to_vec().into(),
                        signed_block_header: Some(proto_header),
                        kzg_commitment_inclusion_proof,
                    })),
                })
            }

            Self::Fin(fin) => Ok(Self::Proto {
                part: Some(Part::Fin(proto::ProposalFin {
                    signature: Some(encode_signature(&fin.signature)),
                })),
            }),
        }
    }
}
