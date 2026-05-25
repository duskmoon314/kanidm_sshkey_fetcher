use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

// use humantime::Duration;

use anyhow::Result;
use clap::Parser;
use kanidm_client::{ClientError, KanidmClient, KanidmClientBuilder};
use serde::{Deserialize, Serialize};
use tracing::{debug, error};

const SSH_CONFIG_DIR: &str = "~/.ssh";
const DEFAULT_LOCK_FILE: &str = "/tmp/kanidm_sshkey_fetcher.lock";
const MANAGED_KEYS_START: &str = "# Managed Keys by kanidm_sshkey_fetcher";
const MANAGED_KEYS_END: &str = "# End of Managed Keys by kanidm_sshkey_fetcher";
const LAST_MODIFIED_PREFIX: &str = "# last_modified: ";

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

    /// Path to write log output to (instead of stderr)
    #[arg(long = "log-file", value_parser)]
    log_file: Option<PathBuf>,

    /// How long before re-fetching keys from the server (e.g., "5m", "1h")
    ///
    /// Only effective when --modify is also set
    #[arg(long = "cache-ttl", value_parser = humantime::parse_duration)]
    #[serde(default, with = "humantime_serde")]
    cache_ttl: Option<Duration>,

    /// Path to the lock file for preventing concurrent updates
    #[arg(long = "lock-file", value_parser)]
    lock_file: Option<PathBuf>,
}

impl Cli {
    pub fn or(&mut self, other: &Cli) {
        self.debug = self.debug || other.debug;
        self.addr = self.addr.clone().or(other.addr.clone());
        self.ca_path = self.ca_path.clone().or(other.ca_path.clone());
        self.account_ids.extend(other.account_ids.clone());
        self.modify = self.modify || other.modify;
        self.log_file = self.log_file.clone().or(other.log_file.clone());
        self.cache_ttl = self.cache_ttl.clone().or(other.cache_ttl.clone());
        self.lock_file = self.lock_file.clone().or(other.lock_file.clone());
    }
}

pub fn init_tracing(args: &Cli) {
    if args.debug {
        unsafe {
            std::env::set_var("RUST_LOG", "kanidm=debug,kanidm_client=debug");
        }
    }

    match &args.log_file {
        Some(path) => {
            let expanded = shellexpand::tilde(&path.to_string_lossy()).into_owned();
            let log_path = PathBuf::from(&expanded);

            let file_appender = tracing_appender::rolling::never(
                log_path.parent().unwrap_or(Path::new("/var/log")),
                log_path
                    .file_name()
                    .unwrap_or(OsStr::new("kanidm_sshkey_fetcher.toml")),
            );

            let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
            tracing_subscriber::fmt()
                .with_writer(non_blocking)
                .with_ansi(false)
                .init();
        }

        None => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .init();
        }
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

fn try_acquire_lock(lock_path: &Path) -> bool {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path)
    {
        Ok(mut f) => {
            use std::io::Write;
            let _ = write!(f, "{}", std::process::id());
            true
        }
        Err(_) => {
            // Lock exists — check if holder is still alive
            if let Ok(content) = std::fs::read_to_string(lock_path)
                && let Ok(pid) = content.trim().parse::<u32>()
            {
                let proc_path = PathBuf::from(format!("/proc/{}", pid));
                if !proc_path.exists() {
                    // Process is dead — remove stale lock and retry
                    let _ = std::fs::remove_file(lock_path);
                    return try_acquire_lock(lock_path);
                }
            }
            false
        }
    }
}

pub fn read_authorized_keys() -> Result<(PathBuf, String)> {
    let ssh_config_dir = PathBuf::from(shellexpand::tilde(SSH_CONFIG_DIR).into_owned());
    let authorized_keys_file = ssh_config_dir.join("authorized_keys");

    if !ssh_config_dir.exists() {
        Ok((authorized_keys_file, String::new()))
    } else {
        let content = std::fs::read_to_string(&authorized_keys_file)?;
        Ok((authorized_keys_file, content))
    }
}

pub async fn fetch_keys(client: &KanidmClient, account_ids: &Vec<String>) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for id in account_ids {
        match client.idm_account_get_ssh_pubkeys(id.as_str()).await {
            Ok(pkeys) => {
                keys.extend(pkeys.clone());
                pkeys.iter().for_each(|pkey| println!("{}", pkey))
            }
            Err(_e) => {}
        }
    }
    Ok(keys)
}

pub fn modify_authorized_keys(keys: Vec<String>) -> Result<()> {
    debug!("Modifying authorized_keys file started");

    let (authorized_keys_file, mut content) = read_authorized_keys()?;

    if !authorized_keys_file.exists() {
        debug!(
            "authorized_keys file does not exist, creating new one at {:?}",
            authorized_keys_file
        );
        std::fs::create_dir_all(
            authorized_keys_file
                .parent()
                .expect("authorized_keys_file has no parent"),
        )?;
        std::fs::write(&authorized_keys_file, "")?;
    }

    let start_index = content.find(MANAGED_KEYS_START).unwrap_or(content.len());
    let end_index = content.find(MANAGED_KEYS_END).unwrap_or(content.len());

    // Prepare timestamp
    let now_millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);

    // Build the managed section content: timestamp + keys
    let mut new_content = format!("{}{}\n", LAST_MODIFIED_PREFIX, now_millis);
    for key in keys {
        new_content.push_str(&format!("{}\n", key));
    }

    // Replace the managed keys section if it exists
    if start_index < end_index {
        let start_index = start_index + MANAGED_KEYS_START.len() + 2; // +2 for the newline
        new_content.push('\n'); // Add a newline between the content and the end marker
        content.replace_range(start_index..end_index, &new_content);
    } else {
        // If the section doesn't exist, append the new content
        content.push_str(&format!(
            "\n{}\n{}\n{}\n",
            MANAGED_KEYS_START, new_content, MANAGED_KEYS_END
        ));
    }

    // Write the updated content back to the file
    std::fs::write(&authorized_keys_file, content)?;

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = Cli::parse();

    if let Some(config_path) = &args.config_path {
        let config_content = std::fs::read_to_string(config_path)?;
        let args_file: Cli = toml::from_str(&config_content)?;

        args.or(&args_file);
    }

    init_tracing(&args);

    let client = build_configured_client(&args)
        .map_err(|_| anyhow::anyhow!("Kanidm Client build failed"))?;

    // Cache path: only when both --modify and --cache-ttl are set
    if args.modify
        && let Some(duration) = &args.cache_ttl
    {
        // Read the authorized_keys file and check if the cache is still valid
        let (_authorized_keys_path, content) = read_authorized_keys()?;
        if let Some(last_modified) = content.find(LAST_MODIFIED_PREFIX).and_then(|idx| {
            content[idx + LAST_MODIFIED_PREFIX.len()..]
                .lines()
                .next()
                .and_then(|line| line.trim().parse::<u64>().ok())
        }) {
            let last_modified = std::time::UNIX_EPOCH + Duration::from_millis(last_modified);
            let now = std::time::SystemTime::now();
            let elapsed = now.duration_since(last_modified)?;
            if elapsed < *duration {
                // Cache is valid, print cached keys and exit
                println!("{}", content);
                return Ok(());
            } else {
                // Cache is stale, update
                let r = client.auth_anonymous().await;
                if let Err(e) = r {
                    match e {
                        ClientError::Transport(e) => {
                            error!("failed to connect to kanidm server: {}", e.to_string())
                        }
                        _ => error!("Error during authentication phase: {:?}", e),
                    }
                }
                let keys = fetch_keys(&client, &args.account_ids).await?;

                // Print keys
                for key in &keys {
                    println!("{}", key);
                }

                // Acquire lock before modifying the file
                let lock_path = args
                    .log_file
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(DEFAULT_LOCK_FILE));

                if try_acquire_lock(&lock_path) {
                    modify_authorized_keys(keys)?;

                    // Release lock after modification
                    std::fs::remove_file(&lock_path)?;
                }
            }
        }
    }

    // Normal path — no cache
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
