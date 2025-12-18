use clap::Parser;
use color_eyre::eyre::Result;
use ultramarine_genesis::{build_dev_genesis, write_genesis};

#[derive(Parser, Debug, Clone, PartialEq)]
pub struct GenesisCmd {
    /// Output file path for genesis configuration
    #[clap(long, default_value = "./assets/genesis.json")]
    output: String,
    /// Chain ID for the genesis configuration
    #[clap(long, default_value = "16383")]
    chain_id: u64,
}

impl GenesisCmd {
    pub async fn run(&self) -> Result<()> {
        let genesis = build_dev_genesis(self.chain_id)?;
        write_genesis(std::path::Path::new(&self.output), &genesis)?;
        println!("\n✅ Genesis configuration written to {}", self.output);
        Ok(())
    }
}
