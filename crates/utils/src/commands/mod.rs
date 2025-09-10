pub mod genesis;
pub mod spam;

use clap::Subcommand;
use genesis::GenesisCmd;
use spam::SpamCmd;

#[derive(Subcommand)]
pub enum Commands {
    /// Generate genesis configuration file
    Genesis(GenesisCmd),
    /// Spam the network with transactions.
    #[command(arg_required_else_help = true)]
    Spam(SpamCmd),
}
