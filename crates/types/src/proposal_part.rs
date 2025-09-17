use core::fmt;

use bytes::Bytes;
use malachitebft_core_types::Round;
use malachitebft_proto::{self as proto, Error as ProtoError, Protobuf};
use malachitebft_signing_ed25519::Signature;
use prost::Message;
use serde::{Deserialize, Serialize};

use crate::{
    address::Address,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalPart {
    Init(ProposalInit),
    Data(ProposalData),
    Fin(ProposalFin),
}

impl ProposalPart {
    pub fn get_type(&self) -> &'static str {
        match self {
            Self::Init(_) => "init",
            Self::Data(_) => "data",
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
            Self::Fin(fin) => Ok(Self::Proto {
                part: Some(Part::Fin(proto::ProposalFin {
                    signature: Some(encode_signature(&fin.signature)),
                })),
            }),
        }
    }
}
