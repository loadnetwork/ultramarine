#![allow(missing_docs)]
//! Example application using channels

use color_eyre::eyre::{Result, eyre};
use malachitebft_app_channel::app::Node;
// use node::App;
use tracing::{info, trace};
use ultramarine_cli::{
    args::{Args, Commands},
    cmd::{
        distributed_testnet::DistributedTestnetCmd, init::InitCmd, start::StartCmd,
        testnet::TestnetCmd,
    },
    config, logging, runtime,
};
use ultramarine_node::node::App;
use ultramarine_types::height::Height;

/// Main entry point for the application
///
/// This function:
/// - Parses command-line arguments
/// - Loads configuration from file
/// - Initializes logging system
/// - Sets up error handling
/// - Creates and runs the application node
fn main() -> Result<()> {
    color_eyre::install()?;

    // Also forward panics to tracing so they show up alongside node logs.
    // This supplements color-eyre's pretty panic output on stderr.
    install_tracing_panic_hook();

    // Load command-line arguments and possible configuration file.
    let args = Args::new();

    // Override logging configuration (if exists) with optional command-line parameters.
    let mut logging = config::LoggingConfig::default();
    if let Some(log_level) = args.log_level {
        logging.log_level = log_level;
    }
    if let Some(log_format) = args.log_format {
        logging.log_format = log_format;
    }

    // This is a drop guard responsible for flushing any remaining logs when the program terminates.
    // It must be assigned to a binding that is not _, as _ will result in the guard being dropped
    // immediately.
    let _guard = logging::init(logging.log_level, logging.log_format);

    trace!("Command-line parameters: {args:?}");

    // Parse the input command.
    match &args.command {
        Commands::Start(cmd) => start(&args, cmd, logging),
        Commands::Init(cmd) => init(&args, cmd, logging),
        Commands::Testnet(cmd) => testnet(&args, cmd, logging),
        Commands::DistributedTestnet(cmd) => distributed_testnet(&args, cmd, logging),
    }
}

fn install_tracing_panic_hook() {
    use std::panic;

    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());

        let msg: &str = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.as_str()
        } else {
            "panic"
        };

        // Capture backtrace if enabled; force capture provides something even without env var.
        let bt = std::backtrace::Backtrace::force_capture();
        tracing::error!(target = "panic", %location, message = %msg, backtrace = %format!("{bt}"), "panic occurred");

        // Preserve existing behavior (color-eyre pretty report to stderr).
        default_hook(info);
    }));
}

fn start(args: &Args, cmd: &StartCmd, logging: config::LoggingConfig) -> Result<()> {
    // Load configuration file if it exists. Some commands do not require a configuration file.
    let config_file = args
        .get_config_file_path()
        .map_err(|error| eyre!("Failed to get configuration file path: {error}"))?;

    let mut config = config::load_config(&config_file, None)
        .map_err(|error| eyre!("Failed to load configuration file: {error}"))?;

    config.logging = logging;

    let rt = runtime::build_runtime(config.runtime)?;

    info!(
        file = %args.get_config_file_path().unwrap_or_default().display(),
        "Loaded configuration",
    );

    trace!(?config, "Configuration");

    // Setup the application
    let app = App {
        config,
        home_dir: args.get_home_dir()?,
        genesis_file: args.get_genesis_file_path()?,
        private_key_file: args.get_priv_validator_key_file_path()?,
        start_height: cmd.start_height.map(Height::new),
        engine_http_url: cmd.engine_http_url.clone(),
        engine_ipc_path: cmd.engine_ipc_path.clone(),
        eth1_rpc_url: cmd.eth1_rpc_url.clone(),
        jwt_path: cmd.jwt_path.clone(),
    };

    // Start the node
    rt.block_on(app.run()).map_err(|error| eyre!("Failed to run the application node: {error}"))
}

fn init(args: &Args, cmd: &InitCmd, logging: config::LoggingConfig) -> Result<()> {
    // Setup the application
    let app = App {
        config: Default::default(), // There is not existing configuration yet
        home_dir: args.get_home_dir()?,
        genesis_file: args.get_genesis_file_path()?,
        private_key_file: args.get_priv_validator_key_file_path()?,
        start_height: Some(Height::new(1)), // We always start at height 1
        engine_http_url: None,
        engine_ipc_path: None,
        eth1_rpc_url: None,
        jwt_path: None,
    };

    cmd.run(
        &app,
        &args.get_config_file_path()?,
        &args.get_genesis_file_path()?,
        &args.get_priv_validator_key_file_path()?,
        logging,
    )
    .map_err(|error| eyre!("Failed to run init command {error:?}"))
}

fn testnet(args: &Args, cmd: &TestnetCmd, logging: config::LoggingConfig) -> Result<()> {
    // Setup the application
    let app = App {
        config: Default::default(), // There is not existing configuration yet
        home_dir: args.get_home_dir()?,
        genesis_file: args.get_genesis_file_path()?,
        private_key_file: args.get_priv_validator_key_file_path()?,
        start_height: Some(Height::new(1)), // We always start at height 1
        engine_http_url: None,
        engine_ipc_path: None,
        eth1_rpc_url: None,
        jwt_path: None,
    };

    cmd.run(&app, &args.get_home_dir()?, logging)
        .map_err(|error| eyre!("Failed to run testnet command {:?}", error))
}

fn distributed_testnet(
    args: &Args,
    cmd: &DistributedTestnetCmd,
    logging: config::LoggingConfig,
) -> Result<()> {
    // Setup the application
    let app = App {
        config: Default::default(), // There is not existing configuration yet
        home_dir: args.get_home_dir()?,
        genesis_file: args.get_genesis_file_path()?,
        private_key_file: args.get_priv_validator_key_file_path()?,
        start_height: Some(Height::new(1)), // We always start at height 1
        engine_http_url: None,
        engine_ipc_path: None,
        eth1_rpc_url: None,
        jwt_path: None,
    };

    cmd.run(&app, &args.get_home_dir()?, logging)
        .map_err(|error| eyre!("Failed to run distributed testnet command {:?}", error))
}
