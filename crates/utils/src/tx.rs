use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_network::TxSigner;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_signer_local::PrivateKeySigner;
use color_eyre::eyre::Result;

pub(crate) fn make_eip1559_tx(nonce: u64, to: Address) -> TxEip1559 {
    TxEip1559 {
        chain_id: 1,
        nonce,
        max_priority_fee_per_gas: 1_000_000_000, // 1 gwei
        max_fee_per_gas: 20_000_000_000,         // 20 gwei
        gas_limit: 21_000,
        to: TxKind::Call(to),
        value: U256::from(100_000_000_000_000_u128), // 0.0001 ETH
        input: Bytes::default(),
        access_list: Default::default(),
    }
}

pub(crate) async fn make_signed_eip1559_tx(
    signer: &PrivateKeySigner,
    nonce: u64,
    to: Address,
) -> Result<TxEnvelope> {
    let mut tx = make_eip1559_tx(nonce, to);

    let signature = signer.sign_transaction(&mut tx).await?;
    Ok(tx.into_signed(signature).into())
}

#[cfg(test)]
mod tests {
    use alloy_network::eip2718::Encodable2718;
    use alloy_primitives::Signature;

    use super::*;

    #[tokio::test]
    async fn test_encode_decode_signed_eip1559_tx() {
        let tx = make_eip1559_tx(0, Address::ZERO);
        let signature = Signature::test_signature();
        let signed_tx: TxEnvelope = tx.into_signed(signature).into();
        let tx_bytes = signed_tx.encoded_2718();

        // Verify we can encode the transaction
        assert!(!tx_bytes.is_empty());
    }
}
