use bytes::Bytes;
pub use malachitebft_core_types::Extension;
use malachitebft_core_types::{NilOrVal, Round, SignedExtension, VoteType};
use malachitebft_proto::{Error as ProtoError, Protobuf};

use crate::{address::Address, context::LoadContext, height::Height, proto, value::ValueId};

/// A vote for a value in a round
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Vote {
    pub typ: VoteType,
    pub height: Height,
    pub round: Round,
    pub value: NilOrVal<ValueId>,
    pub validator_address: Address,
    pub extension: Option<SignedExtension<LoadContext>>,
}

impl Vote {
    pub fn new_prevote(
        height: Height,
        round: Round,
        value: NilOrVal<ValueId>,
        validator_address: Address,
    ) -> Self {
        Self { typ: VoteType::Prevote, height, round, value, validator_address, extension: None }
    }

    pub fn new_precommit(
        height: Height,
        round: Round,
        value: NilOrVal<ValueId>,
        address: Address,
    ) -> Self {
        Self {
            typ: VoteType::Precommit,
            height,
            round,
            value,
            validator_address: address,
            extension: None,
        }
    }

    pub fn to_bytes(&self) -> Bytes {
        Protobuf::to_bytes(self).unwrap()
    }
}

impl malachitebft_core_types::Vote<LoadContext> for Vote {
    fn height(&self) -> Height {
        self.height
    }

    fn round(&self) -> Round {
        self.round
    }

    fn value(&self) -> &NilOrVal<ValueId> {
        &self.value
    }

    fn take_value(self) -> NilOrVal<ValueId> {
        self.value
    }

    fn vote_type(&self) -> VoteType {
        self.typ
    }

    fn validator_address(&self) -> &Address {
        &self.validator_address
    }

    fn extension(&self) -> Option<&SignedExtension<LoadContext>> {
        self.extension.as_ref()
    }

    fn take_extension(&mut self) -> Option<SignedExtension<LoadContext>> {
        self.extension.take()
    }

    fn extend(self, extension: SignedExtension<LoadContext>) -> Self {
        Self { extension: Some(extension), ..self }
    }
}

impl Protobuf for Vote {
    type Proto = crate::proto::Vote;

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn from_proto(proto: Self::Proto) -> Result<Self, ProtoError> {
        Ok(Self {
            typ: decode_votetype(proto.vote_type()),
            height: Height::from_proto(proto.height)?,
            round: Round::new(proto.round),
            value: match proto.value {
                Some(value) => NilOrVal::Val(ValueId::from_proto(value)?),
                None => NilOrVal::Nil,
            },
            validator_address: Address::from_proto(
                proto
                    .validator_address
                    .ok_or_else(|| ProtoError::missing_field::<Self::Proto>("validator_address"))?,
            )?,
            extension: Default::default(),
        })
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn to_proto(&self) -> Result<Self::Proto, ProtoError> {
        Ok(Self::Proto {
            vote_type: encode_votetype(self.typ).into(),
            height: self.height.to_proto()?,
            round: self.round.as_u32().expect("round should not be nil"),
            value: match &self.value {
                NilOrVal::Nil => None,
                NilOrVal::Val(v) => Some(v.to_proto()?),
            },
            validator_address: Some(self.validator_address.to_proto()?),
        })
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn encode_votetype(vote_type: VoteType) -> proto::VoteType {
    match vote_type {
        VoteType::Prevote => proto::VoteType::Prevote,
        VoteType::Precommit => proto::VoteType::Precommit,
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn decode_votetype(vote_type: proto::VoteType) -> VoteType {
    match vote_type {
        proto::VoteType::Prevote => VoteType::Prevote,
        proto::VoteType::Precommit => VoteType::Precommit,
    }
}
