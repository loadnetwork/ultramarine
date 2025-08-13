use bytes::Bytes;
use malachitebft_core_types::{Context, NilOrVal, Round, ValidatorSet as _};

use crate::{
    address::*, height::*, proposal::*, proposal_part::*, signing::*, validator_set::*, value::*,
    vote::*,
};

#[derive(Copy, Clone, Debug, Default)]
pub struct LoadContext;

impl LoadContext {
    pub fn new() -> Self {
        Self
    }
}

// TODO: implement with concrete types
impl Context for LoadContext {
    type Address = Address;
    type ProposalPart = ProposalPart;
    type Height = Height;
    type Proposal = Proposal;
    type ValidatorSet = ValidatorSet;
    type Validator = Validator;
    type Value = Value;
    type Vote = Vote;
    type SigningScheme = Ed25519;
    type Extension = Bytes;

    fn select_proposer<'a>(
        &self,
        validator_set: &'a Self::ValidatorSet,
        height: Self::Height,
        round: Round,
    ) -> &'a Self::Validator {
        assert!(validator_set.count() > 0);
        assert!(round != Round::Nil && round.as_i64() >= 0);

        let proposer_index = {
            let height = height.as_u64() as usize;
            let round = round.as_i64() as usize;

            (height - 1 + round) % validator_set.count()
        };

        validator_set.get_by_index(proposer_index).expect("proposer_index is valid")
    }

    fn new_proposal(
        height: Height,
        round: Round,
        value: Value,
        pol_round: Round,
        address: Address,
    ) -> Proposal {
        Proposal::new(height, round, value, pol_round, address)
    }

    fn new_prevote(
        height: Height,
        round: Round,
        value_id: NilOrVal<ValueId>,
        address: Address,
    ) -> Vote {
        Vote::new_prevote(height, round, value_id, address)
    }

    fn new_precommit(
        height: Height,
        round: Round,
        value_id: NilOrVal<ValueId>,
        address: Address,
    ) -> Vote {
        Vote::new_precommit(height, round, value_id, address)
    }
}
