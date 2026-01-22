use std::{collections::BTreeMap, str::FromStr};

use alloy_genesis::{ChainConfig, Genesis, GenesisAccount};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_signer_local::{MnemonicBuilder, PrivateKeySigner, coins_bip39::English};
use chrono::NaiveDate;
use color_eyre::eyre::Result;
use ultramarine_types::constants::LOAD_EXECUTION_GAS_LIMIT;

/// Test mnemonics for wallet generation.
///
/// This is intended for dev/testnet genesis generation workflows.
const TEST_MNEMONICS: [&str; 3] = [
    "test test test test test test test test test test test junk",
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
    "zero zero zero zero zero zero zero zero zero zero zero zoo",
];

pub fn make_signer(mnemonic: &str) -> PrivateKeySigner {
    MnemonicBuilder::<English>::default().phrase(mnemonic).build().expect("failed to create wallet")
}

pub fn make_signers() -> Vec<PrivateKeySigner> {
    TEST_MNEMONICS.iter().map(|&mnemonic| make_signer(mnemonic)).collect()
}

pub fn build_dev_genesis(chain_id: u64) -> Result<Genesis> {
    let signers = make_signers();
    let signer_addresses: Vec<Address> = signers.iter().map(|signer| signer.address()).collect();

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

    build_genesis_from_alloc(chain_id, alloc)
}

pub fn build_genesis(chain_id: u64, alloc: BTreeMap<Address, GenesisAccount>) -> Result<Genesis> {
    build_genesis_from_alloc(chain_id, alloc)
}

pub fn build_genesis_from_alloc_strings(
    chain_id: u64,
    alloc: Vec<(String, String)>,
) -> Result<Genesis> {
    let mut map = BTreeMap::new();
    for (address, balance_wei) in alloc {
        let addr = Address::from_str(&address)?;
        let balance = U256::from_str(&balance_wei)?;
        map.insert(addr, GenesisAccount { balance, ..Default::default() });
    }
    build_genesis_from_alloc(chain_id, map)
}

fn build_genesis_from_alloc(
    chain_id: u64,
    alloc: BTreeMap<Address, GenesisAccount>,
) -> Result<Genesis> {
    // The Ethereum Cancun-Deneb (Dencun) upgrade was activated on the mainnet on March 13, 2024.
    // We keep the timestamp reference handy for future policy, but Load activates forks at genesis.
    let date = NaiveDate::from_ymd_opt(2024, 3, 14).unwrap();
    let datetime = date.and_hms_opt(0, 0, 0).unwrap();
    let _valid_cancun_timestamp = datetime.and_utc().timestamp() as u64;

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
            prague_time: Some(0),
            merge_netsplit_block: Some(0),
            terminal_total_difficulty: Some(U256::ZERO),
            terminal_total_difficulty_passed: true,
            ..Default::default()
        },
        alloc,
        ..Default::default()
    }
    .with_gas_limit(LOAD_EXECUTION_GAS_LIMIT)
    .with_timestamp(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards")
            .as_secs(),
    )
    .with_extra_data(Bytes::from_static(b"Load Network Dev"))
    .with_difficulty(U256::ZERO)
    .with_mix_hash(B256::ZERO)
    .with_coinbase(Address::ZERO)
    .with_base_fee(Some(7));

    let mut genesis = genesis;
    genesis.parent_hash = Some(B256::ZERO);
    genesis.number = Some(0);

    Ok(genesis)
}

pub fn write_genesis(path: &std::path::Path, genesis: &Genesis) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut genesis_value = serde_json::to_value(genesis)?;
    if let Some(root) = genesis_value.as_object_mut() {
        root.insert("gasUsed".to_string(), serde_json::Value::String("0x0".to_string()));
        root.insert(
            "parentHash".to_string(),
            serde_json::Value::String(
                "0x0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            ),
        );
    }
    if let Some(config) = genesis_value.get_mut("config").and_then(serde_json::Value::as_object_mut) &&
        matches!(config.get("daoForkSupport"), Some(serde_json::Value::Bool(false)))
    {
        config.remove("daoForkSupport");
    }
    let genesis_json = serde_json::to_string_pretty(&genesis_value)?;
    std::fs::write(path, genesis_json)?;
    Ok(())
}
