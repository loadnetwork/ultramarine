use std::{collections::BTreeMap, str::FromStr};

use alloy_genesis::{ChainConfig, Genesis, GenesisAccount};
use alloy_primitives::{Address, U256};
use alloy_signer_local::{MnemonicBuilder, PrivateKeySigner, coins_bip39::English};
use chrono::NaiveDate;
use clap::Parser;
use color_eyre::eyre::Result;

/// Test mnemonics for wallet generation
const TEST_MNEMONICS: [&str; 3] = [
    "test test test test test test test test test test test junk",
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
    "zero zero zero zero zero zero zero zero zero zero zero zoo",
];

#[derive(Parser, Debug, Clone, PartialEq)]
pub struct GenesisCmd {
    /// Output file path for genesis configuration
    #[clap(long, default_value = "./assets/genesis.json")]
    output: String,
    /// Chain ID for the genesis configuration
    #[clap(long, default_value = "1")]
    chain_id: u64,
}

impl GenesisCmd {
    pub async fn run(&self) -> Result<()> {
        generate_genesis(&self.output, self.chain_id)
    }
}

/// Create a signer from a mnemonic.
pub(crate) fn make_signer(mnemonic: &str) -> PrivateKeySigner {
    MnemonicBuilder::<English>::default().phrase(mnemonic).build().expect("Failed to create wallet")
}

pub(crate) fn make_signers() -> Vec<PrivateKeySigner> {
    TEST_MNEMONICS.iter().map(|&mnemonic| make_signer(mnemonic)).collect()
}

pub(crate) fn generate_genesis(genesis_file: &str, chain_id: u64) -> Result<()> {
    // Create signers and get their addresses
    let signers = make_signers();
    let signer_addresses: Vec<Address> = signers.iter().map(|signer| signer.address()).collect();

    println!("Using signer addresses:");
    for (i, addr) in signer_addresses.iter().enumerate() {
        println!("  Signer {}: {}", i, addr);
    }

    // Create genesis configuration with pre-funded accounts
    let mut alloc = BTreeMap::new();
    for addr in &signer_addresses {
        alloc.insert(
            *addr,
            GenesisAccount {
                balance: U256::from_str("15000000000000000000000").unwrap(), // 15000 ETH
                ..Default::default()
            },
        );
    }

    // The Ethereum Cancun-Deneb (Dencun) upgrade was activated on the mainnet
    // on March 13, 2024, at epoch 269,568.
    let date = NaiveDate::from_ymd_opt(2024, 3, 14).unwrap();
    let datetime = date.and_hms_opt(0, 0, 0).unwrap();
    let valid_cancun_timestamp = datetime.and_utc().timestamp() as u64;

    // Create genesis configuration
    let genesis = Genesis {
        config: ChainConfig {
            chain_id,
            homestead_block: Some(0),
            eip150_block: Some(0),
            eip155_block: Some(0),
            eip158_block: Some(0),
            byzantium_block: Some(0),
            constantinople_block: Some(0),
            petersburg_block: Some(0),
            istanbul_block: Some(0),
            berlin_block: Some(0),
            london_block: Some(0),
            shanghai_time: Some(0),
            cancun_time: Some(0),
            terminal_total_difficulty: Some(U256::ZERO),
            terminal_total_difficulty_passed: true,
            ..Default::default()
        },
        alloc,
        ..Default::default()
    }
    .with_gas_limit(30_000_000)
    .with_timestamp(valid_cancun_timestamp);

    // Create parent directories if they don't exist
    if let Some(parent) = std::path::Path::new(genesis_file).parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Write genesis to file
    let genesis_json = serde_json::to_string_pretty(&genesis)?;
    std::fs::write(genesis_file, genesis_json)?;
    println!("\n✅ Genesis configuration written to {}", genesis_file);

    Ok(())
}
