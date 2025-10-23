use async_trait::async_trait;
use bytes::Bytes;
use malachitebft_core_types::{
    CommitCertificate, CommitSignature, NilOrVal, SignedMessage, VotingPower,
};
use malachitebft_signing::{Error as SigningError, SigningProvider, VerificationResult};
pub use malachitebft_signing_ed25519::*;

use crate::{
    context::LoadContext, proposal::Proposal, proposal_part::ProposalPart,
    validator_set::Validator, vote::Vote,
};

pub trait Hashable {
    type Output;
    fn hash(&self) -> Self::Output;
}

impl Hashable for PublicKey {
    type Output = [u8; 32];

    fn hash(&self) -> [u8; 32] {
        use sha3::{Digest, Keccak256};
        let mut hasher = Keccak256::new();
        hasher.update(self.as_bytes());
        hasher.finalize().into()
    }
}

#[derive(Debug)]
pub struct Ed25519Provider {
    private_key: PrivateKey,
}

impl Ed25519Provider {
    pub fn new(private_key: PrivateKey) -> Self {
        Self { private_key }
    }

    pub fn private_key(&self) -> &PrivateKey {
        &self.private_key
    }

    pub fn sign(&self, data: &[u8]) -> Signature {
        self.private_key.sign(data)
    }

    pub fn verify(&self, data: &[u8], signature: &Signature, public_key: &PublicKey) -> bool {
        public_key.verify(data, signature).is_ok()
    }
}

#[async_trait]
impl SigningProvider<LoadContext> for Ed25519Provider {
    #[cfg_attr(coverage_nightly, coverage(off))]
    async fn sign_vote(&self, vote: Vote) -> Result<SignedMessage<LoadContext, Vote>, SigningError> {
        let signature = self.sign(&vote.to_bytes());
        Ok(SignedMessage::new(vote, signature))
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    async fn verify_signed_vote(
        &self,
        vote: &Vote,
        signature: &Signature,
        public_key: &PublicKey,
    ) -> Result<VerificationResult, SigningError> {
        let is_valid = public_key.verify(&vote.to_bytes(), signature).is_ok();
        Ok(VerificationResult::from_bool(is_valid))
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    async fn sign_proposal(&self, proposal: Proposal) -> Result<SignedMessage<LoadContext, Proposal>, SigningError> {
        let signature = self.private_key.sign(&proposal.to_bytes());
        Ok(SignedMessage::new(proposal, signature))
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    async fn verify_signed_proposal(
        &self,
        proposal: &Proposal,
        signature: &Signature,
        public_key: &PublicKey,
    ) -> Result<VerificationResult, SigningError> {
        let is_valid = public_key.verify(&proposal.to_bytes(), signature).is_ok();
        Ok(VerificationResult::from_bool(is_valid))
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    async fn sign_proposal_part(&self, proposal_part: ProposalPart) -> Result<SignedMessage<LoadContext, ProposalPart>, SigningError> {
        let signature = self.private_key.sign(&proposal_part.to_sign_bytes());
        Ok(SignedMessage::new(proposal_part, signature))
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    async fn verify_signed_proposal_part(
        &self,
        proposal_part: &ProposalPart,
        signature: &Signature,
        public_key: &PublicKey,
    ) -> Result<VerificationResult, SigningError> {
        let is_valid = public_key.verify(&proposal_part.to_sign_bytes(), signature).is_ok();
        Ok(VerificationResult::from_bool(is_valid))
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    async fn sign_vote_extension(&self, _extension: Bytes) -> Result<SignedMessage<LoadContext, Bytes>, SigningError> {
        unimplemented!("Vote extensions not yet supported")
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    async fn verify_signed_vote_extension(
        &self,
        _extension: &Bytes,
        _signature: &Signature,
        _public_key: &PublicKey,
    ) -> Result<VerificationResult, SigningError> {
        unimplemented!("Vote extensions not yet supported")
    }
}

// Extension method for commit signature verification (removed from trait)
impl Ed25519Provider {
    /// Verify a commit signature against a certificate
    ///
    /// Note: This was removed from the SigningProvider trait in latest malachite.
    /// Kept as an extension method for backward compatibility.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub async fn verify_commit_signature(
        &self,
        certificate: &CommitCertificate<LoadContext>,
        commit_sig: &CommitSignature<LoadContext>,
        validator: &Validator,
    ) -> Result<VotingPower, String> {
        use malachitebft_core_types::Validator;

        // Reconstruct the vote that was signed
        let vote = Vote::new_precommit(
            certificate.height,
            certificate.round,
            NilOrVal::Val(certificate.value_id),
            *validator.address(),
        );

        // Verify signature
        let result = self.verify_signed_vote(&vote, &commit_sig.signature, validator.public_key())
            .await
            .map_err(|e| format!("Signature verification error: {:?}", e))?;

        if result.is_invalid() {
            return Err(format!("Invalid commit signature from validator {}", validator.address()));
        }

        Ok(validator.voting_power())
    }
}
