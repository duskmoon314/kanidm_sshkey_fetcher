use std::path::PathBuf;

use clap::Parser;
use kanidm_client::{ClientError, KanidmClient, KanidmClientBuilder};
use serde::{Deserialize, Serialize};
use tracing::{debug, error};

const SSH_CONFIG_DIR: &str = "~/.ssh";

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

    /// Whether to modify the authorized_keys file
    ///
    /// If true, the program will try to update ~/.ssh/authorized_keys
    #[arg(short, long, default_value_t = false)]
    #[serde(default)]
    modify: bool,
}

impl Cli {
    pub fn or(&mut self, other: &Cli) {
        self.debug = self.debug || other.debug;
        self.addr = self.addr.clone().or(other.addr.clone());
        self.ca_path = self.ca_path.clone().or(other.ca_path.clone());
        self.account_ids.extend(other.account_ids.clone());
        self.modify = self.modify || other.modify;
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

pub fn modify_authorized_keys(keys: Vec<String>) -> Result<(), ()> {
    debug!("Modifying authorized_keys file started");

    let ssh_config_dir = PathBuf::from(shellexpand::tilde(SSH_CONFIG_DIR).into_owned());
    if !ssh_config_dir.exists() {
        debug!("Creating ssh config directory");

        std::fs::create_dir(&ssh_config_dir)
            .map_err(|e| error!("Failed to create ssh config directory -- {:?}", e))?;
    }

    let authorized_keys_file = ssh_config_dir.join("authorized_keys");

    let mut authorized_keys =
        std::fs::read_to_string(&authorized_keys_file).unwrap_or_else(|_| String::new());

    // Find `# Managed Keys by kanidm_sshkey_fetcher` and `# End of Managed Keys by kanidm_sshkey_fetcher`
    const MANAGED_KEYS_START: &str = "# Managed Keys by kanidm_sshkey_fetcher";
    const MANAGED_KEYS_END: &str = "# End of Managed Keys by kanidm_sshkey_fetcher";
    let start_index = authorized_keys
        .find(MANAGED_KEYS_START)
        .unwrap_or(authorized_keys.len());
    let end_index = authorized_keys
        .find(MANAGED_KEYS_END)
        .unwrap_or(authorized_keys.len());

    // Prepare the new content
    let mut new_content = String::new();
    for key in keys {
        new_content.push_str(&format!("{}\n", key));
    }

    // Replace the managed keys section if it exists
    if start_index < end_index {
        let start_index = start_index + MANAGED_KEYS_START.len() + 2; // +2 for the newline
        new_content.push('\n'); // Add a newline between the content and the end marker
        authorized_keys.replace_range(start_index..end_index, &new_content);
    } else {
        // If the section doesn't exist, append the new content
        authorized_keys.push_str(&format!(
            "\n{}\n\n{}\n{}\n",
            MANAGED_KEYS_START, new_content, MANAGED_KEYS_END
        ));
    }

    // Write the updated content back to the file
    std::fs::write(&authorized_keys_file, authorized_keys)
        .map_err(|e| error!("Failed to write to authorized_keys file -- {:?}", e))?;

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), ()> {
    let mut args = Cli::parse();

    if let Some(config_path) = &args.config_path {
        let config_content = std::fs::read_to_string(config_path)
            .map_err(|e| eprintln!("Failed to read config file -- {:?}", e))?;

        let args_file: Cli = toml::from_str(&config_content)
            .map_err(|e| eprintln!("Failed to parse config file -- {:?}", e))?;

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

    let mut keys = Vec::new();

    for id in &args.account_ids {
        match client.idm_account_get_ssh_pubkeys(id.as_str()).await {
            Ok(pkeys) => {
                keys.extend(pkeys.clone());
                pkeys.iter().for_each(|pkey| println!("{}", pkey))
            }
            // Err(e) => error!("Failed to get ssh pubkeys for account {} -- {:?}", id, e),
            Err(_e) => {}
        }
    }

    // Modify the authorized_keys file if requested
    if args.modify {
        modify_authorized_keys(keys)?;
    }

    Ok(())
}
