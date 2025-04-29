use std::path::PathBuf;

use clap::Parser;
use kanidm_client::{ClientError, KanidmClient, KanidmClientBuilder};
use serde::{Deserialize, Serialize};
use tracing::error;

#[derive(Debug, Parser, Serialize, Deserialize)]
#[command(version, about)]
pub struct Cli {
    #[arg(short, long)]
    #[serde(default)]
    debug: bool,

    /// The address of the kanidm server to connect to
    #[arg(short = 'H', long = "url")]
    addr: Option<String>,

    /// The certificate file to use
    #[arg(short = 'C', long = "ca", value_parser)]
    ca_path: Option<PathBuf>,

    /// The configuration file to use
    #[arg(short = 'c', long = "config", value_parser)]
    config_path: Option<PathBuf>,

    /// The account ids to fetch, space separated
    #[serde(default)]
    account_ids: Vec<String>,
}

impl Cli {
    pub fn or(&mut self, other: &Cli) {
        self.debug = self.debug || other.debug;
        self.addr = self.addr.clone().or(other.addr.clone());
        self.ca_path = self.ca_path.clone().or(other.ca_path.clone());
        self.account_ids.extend(other.account_ids.clone());
    }
}

pub fn build_configured_client(args: &Cli) -> Result<KanidmClient, ()> {
    let client_builder = {
        use kanidm_proto::constants::{
            DEFAULT_CLIENT_CONFIG_PATH, DEFAULT_CLIENT_CONFIG_PATH_HOME,
        };
        use tracing::debug;

        let config_path = shellexpand::tilde(DEFAULT_CLIENT_CONFIG_PATH_HOME).into_owned();

        debug!("Attempting to use config {}", DEFAULT_CLIENT_CONFIG_PATH);
        KanidmClientBuilder::new()
            .read_options_from_optional_config(DEFAULT_CLIENT_CONFIG_PATH)
            .and_then(|cb| {
                debug!("Attempting to use config {}", config_path);
                cb.read_options_from_optional_config(config_path)
            })
            .map_err(|e| {
                error!("Failed to parse config (if present) -- {:?}", e);
            })
    }?;

    let client_builder = match &args.addr {
        Some(addr) => client_builder.address(addr.to_string()),
        None => client_builder,
    };

    let ca_path = args.ca_path.as_ref().and_then(|p| p.to_str());
    let client_builder = match ca_path {
        Some(ca_path) => client_builder
            .add_root_certificate_filepath(ca_path)
            .map_err(|e| {
                error!("Failed to add ca certificate -- {:?}", e);
            })?,
        None => client_builder,
    };

    client_builder.build().map_err(|e| {
        error!("Failed to build client -- {:?}", e);
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), ()> {
    let mut args = Cli::parse();

    if let Some(config_path) = &args.config_path {
        let config_content = std::fs::read_to_string(config_path)
            .map_err(|e| error!("Failed to read config file -- {:?}", e))?;

        let args_file: Cli = toml::from_str(&config_content)
            .map_err(|e| error!("Failed to parse config file -- {:?}", e))?;

        args.or(&args_file);
    }

    if args.debug {
        unsafe {
            std::env::set_var("RUST_LOG", "kanidm=debug,kanidm_client=debug");
        }
    }
    tracing_subscriber::fmt::init();

    let client = build_configured_client(&args)?;

    let r = client.auth_anonymous().await;
    if let Err(e) = r {
        match e {
            ClientError::Transport(e) => {
                error!("failed to connect to kanidm server: {}", e.to_string())
            }
            _ => error!("Error during authentication phase: {:?}", e),
        }
    }

    for id in &args.account_ids {
        match client.idm_account_get_ssh_pubkeys(id.as_str()).await {
            Ok(pkeys) => pkeys.iter().for_each(|pkey| println!("{}", pkey)),
            // Err(e) => error!("Failed to get ssh pubkeys for account {} -- {:?}", id, e),
            Err(_e) => {}
        }
    }

    Ok(())
}
