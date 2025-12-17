use bytes::Bytes;
use malachitebft_core_types::Round;
use malachitebft_proto::{Error as ProtoError, Protobuf};
use malachitebft_signing_ed25519::{PublicKey, Signature};
use prost::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    address::Address,
    aliases::B256,
    blob::KzgCommitment,
    codec::proto::{decode_signature, encode_signature},
    height::Height,
    proto,
    signing::Ed25519Provider,
};

const ARCHIVE_NOTICE_DOMAIN: &[u8] = b"ArchiveNoticeV0";

/// Represents the archiving status for an individual blob index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum BlobArchivalStatus {
    /// Blob is still stored locally and awaiting archival.
    Pending,
    /// Blob has been archived and verified via an [`ArchiveRecord`].
    Archived(ArchiveRecord),
}

impl BlobArchivalStatus {
    pub fn is_archived(&self) -> bool {
        matches!(self, Self::Archived(_))
    }
}

/// Signed archival notice broadcast by proposers after uploading blobs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveNotice {
    pub body: ArchiveNoticeBody,
    pub signature: Signature,
}

impl ArchiveNotice {
    pub fn new(body: ArchiveNoticeBody, signature: Signature) -> Self {
        Self { body, signature }
    }

    /// Create and sign an archive notice with the given signer.
    pub fn sign(body: ArchiveNoticeBody, signer: &Ed25519Provider) -> Self {
        let signing_root = body.signing_root();
        let signature = signer.sign(signing_root.as_slice());
        Self { body, signature }
    }

    /// Verifies the notice signature against the provided public key.
    pub fn verify(&self, public_key: &PublicKey) -> bool {
        let signing_root = self.body.signing_root();
        public_key.verify(signing_root.as_slice(), &self.signature).is_ok()
    }

    pub fn blob_index(&self) -> u16 {
        self.body.blob_index
    }
}

/// Archive record persisted once a notice has been verified.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveRecord {
    pub body: ArchiveNoticeBody,
    pub signature: Signature,
}

impl ArchiveRecord {
    pub fn from_notice(notice: ArchiveNotice) -> Self {
        Self { body: notice.body, signature: notice.signature }
    }

    pub fn blob_index(&self) -> u16 {
        self.body.blob_index
    }

    pub fn into_notice(self) -> ArchiveNotice {
        ArchiveNotice { body: self.body, signature: self.signature }
    }

    pub fn to_notice(&self) -> ArchiveNotice {
        ArchiveNotice { body: self.body.clone(), signature: self.signature }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "Round")]
enum RoundSerde {
    Nil,
    Some(u32),
}

/// Core payload of an archive notice/record (signing pre-image).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveNoticeBody {
    pub height: Height,
    #[serde(with = "RoundSerde")]
    pub round: Round,
    pub blob_index: u16,
    pub kzg_commitment: KzgCommitment,
    pub blob_keccak: B256,
    pub provider_id: String,
    pub locator: String,
    pub archived_by: Address,
    pub archived_at: u64,
}

impl ArchiveNoticeBody {
    pub fn signing_root(&self) -> [u8; 32] {
        let proto = self.to_proto();
        let mut buf = Vec::with_capacity(proto.encoded_len());
        proto.encode(&mut buf).expect("ArchiveNoticeBody protbuf encoding should not fail");
        let mut hasher = Sha256::new();
        hasher.update(ARCHIVE_NOTICE_DOMAIN);
        hasher.update(buf);
        hasher.finalize().into()
    }

    pub fn blob_index(&self) -> u16 {
        self.blob_index
    }

    pub fn to_proto(&self) -> proto::ArchiveNoticeBody {
        let archived_by = self
            .archived_by
            .to_proto()
            .expect("Address::to_proto should not fail for fixed-size address");
        proto::ArchiveNoticeBody {
            height: self.height.as_u64(),
            round: self.round_to_proto(),
            blob_index: u32::from(self.blob_index),
            kzg_commitment: Bytes::copy_from_slice(self.kzg_commitment.as_bytes()),
            blob_keccak: Bytes::copy_from_slice(self.blob_keccak.as_slice()),
            provider_id: self.provider_id.clone(),
            locator: self.locator.clone(),
            archived_by: Some(archived_by),
            archived_at: self.archived_at,
        }
    }

    fn round_to_proto(&self) -> i32 {
        if self.round == Round::Nil { -1 } else { self.round.as_i64() as i32 }
    }

    fn round_from_proto(value: i32) -> Round {
        if value < 0 { Round::Nil } else { Round::new(value as u32) }
    }

    pub fn from_proto(proto: proto::ArchiveNoticeBody) -> Result<Self, ProtoError> {
        Ok(Self {
            height: Height::new(proto.height),
            round: Self::round_from_proto(proto.round),
            blob_index: u16::try_from(proto.blob_index).map_err(|_| {
                ProtoError::Other(format!("blob_index {} does not fit in u16", proto.blob_index))
            })?,
            kzg_commitment: KzgCommitment::from_slice(&proto.kzg_commitment)
                .map_err(ProtoError::Other)?,
            blob_keccak: B256::from_slice(&proto.blob_keccak),
            provider_id: proto.provider_id,
            locator: proto.locator,
            archived_by: proto
                .archived_by
                .ok_or_else(|| ProtoError::missing_field::<proto::ArchiveNoticeBody>("archived_by"))
                .and_then(Address::from_proto)?,
            archived_at: proto.archived_at,
        })
    }
}

impl Protobuf for ArchiveNotice {
    type Proto = proto::ArchiveNotice;

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        let body_proto =
            proto.body.ok_or_else(|| ProtoError::missing_field::<Self::Proto>("body"))?;
        let body = ArchiveNoticeBody::from_proto(body_proto)?;
        let signature_proto =
            proto.signature.ok_or_else(|| ProtoError::missing_field::<Self::Proto>("signature"))?;
        let signature = decode_signature(signature_proto)?;
        Ok(Self { body, signature })
    }

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        let proto = proto::ArchiveNotice {
            body: Some(self.body.to_proto()),
            signature: Some(encode_signature(&self.signature)),
        };
        Ok(proto)
    }
}

impl Protobuf for ArchiveRecord {
    type Proto = proto::ArchiveRecord;

    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        let body_proto =
            proto.body.ok_or_else(|| ProtoError::missing_field::<Self::Proto>("body"))?;
        let body = ArchiveNoticeBody::from_proto(body_proto)?;
        let signature_proto = proto
            .archive_signature
            .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("archive_signature"))?;
        let signature = decode_signature(signature_proto)?;
        Ok(Self { body, signature })
    }

    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        let proto = proto::ArchiveRecord {
            body: Some(self.body.to_proto()),
            archive_signature: Some(encode_signature(&self.signature)),
        };
        Ok(proto)
    }
}

/// A job to archive blobs for a specific height.
///
/// This is passed to the archiver worker which performs the actual upload
/// and generates signed archive notices.
#[derive(Clone, Debug)]
pub struct ArchiveJob {
    /// Height of the committed block.
    pub height: Height,
    /// Round of the committed block.
    pub round: Round,
    /// Indices of blobs to archive.
    pub blob_indices: Vec<u16>,
    /// KZG commitments for each blob.
    pub commitments: Vec<KzgCommitment>,
    /// Keccak256 hashes of each blob.
    pub blob_keccaks: Vec<B256>,
    /// Versioned hashes for each blob (kzg_to_versioned_hash output).
    pub versioned_hashes: Vec<B256>,
}
