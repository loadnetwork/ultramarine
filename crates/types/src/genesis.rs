use serde::{Deserialize, Serialize};

use crate::validator_set::ValidatorSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Genesis {
    pub validator_set: ValidatorSet,
}
