use anyhow::Result;
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::env;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::{error::Error, fs};
use toml;

use crate::client::Position;
use crate::scancode;
use crate::scancode::Linux::{KeyLeftAlt, KeyLeftCtrl, KeyLeftMeta, KeyLeftShift};

pub const DEFAULT_PORT: u16 = 4242;

#[derive(Serialize, Deserialize, Debug)]
pub struct ConfigToml {
    pub role: Option<Role>,
    pub device_name: Option<String>,
    pub clients: Option<Vec<StoredClient>>,
    pub port: Option<u16>,
    pub frontend: Option<String>,
    pub release_bind: Option<Vec<scancode::Linux>>,
    pub left: Option<TomlClient>,
    pub right: Option<TomlClient>,
    pub top: Option<TomlClient>,
    pub bottom: Option<TomlClient>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn peer_save_preserves_options_and_allows_multiple_peers_on_one_side() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "port = 4242\nfrontend = 'gtk'\ncustom_option = 'keep'\n[right]\nhostname = 'legacy'\n",
        )
        .unwrap();
        let clients = (0..2)
            .map(|n| StoredClient {
                display_name: None,
                hostname: Some(format!("peer{n}")),
                ips: vec!["127.0.0.1".parse().unwrap()],
                port: 4242,
                position: Position::Right,
                activate_on_startup: n == 1,
            })
            .collect();
        save_peer_state(&path, 4343, clients).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let document: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(document["custom_option"].as_str(), Some("keep"));
        assert!(document.get("right").is_none());
        let config: ConfigToml = toml::from_str(&text).unwrap();
        assert_eq!(config.port, Some(4343));
        assert_eq!(config.clients.unwrap().len(), 2);
    }
    #[test]
    fn malformed_config_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "not valid [").unwrap();
        assert!(save_peer_state(&path, 4242, vec![]).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "not valid [");
    }
    #[test]
    fn empty_config_and_legacy_config_still_parse() {
        let config: ConfigToml = toml::from_str("").unwrap();
        assert!(config.clients.is_none());
        let config: ConfigToml = toml::from_str("[left]\nhostname = 'peer'\n").unwrap();
        assert_eq!(config.left.unwrap().hostname.as_deref(), Some("peer"));
    }

    #[test]
    fn identity_is_preserved_when_peers_are_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        save_identity(&path, "Office host", Role::Host).unwrap();
        save_peer_state(&path, 4343, vec![]).unwrap();
        let config: ConfigToml = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(config.role, Some(Role::Host));
        assert_eq!(config.device_name.as_deref(), Some("Office host"));
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub struct StoredClient {
    #[serde(default)]
    pub display_name: Option<String>,
    pub hostname: Option<String>,
    pub ips: Vec<IpAddr>,
    pub port: u16,
    pub position: crate::client::Position,
    pub activate_on_startup: bool,
}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Host,
    #[default]
    Client,
}

pub fn computer_name() -> String {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| "This PC".into())
}

pub fn save_identity(path: &Path, name: &str, role: Role) -> Result<()> {
    let mut document: toml::Value = match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(Default::default())
        }
        Err(e) => return Err(e.into()),
    };
    let table = document
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("Config must be a table"))?;
    table.insert("device_name".into(), name.into());
    table.insert("role".into(), toml::Value::try_from(role)?);
    atomic_write(path, toml::to_string_pretty(&document)?.as_bytes())
}

/// Portable distributions opt in with a marker beside the EXE. Explicit --config wins.
pub fn default_config_path() -> Result<PathBuf> {
    #[cfg(windows)]
    if let Some(dir) = env::current_exe()?.parent() {
        if dir.join("portable.flag").exists() {
            return Ok(dir.join("config.toml"));
        }
    }
    #[cfg(windows)]
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join(".config")))
        .ok_or_else(|| anyhow::anyhow!("No Windows configuration folder available"))?;
    #[cfg(unix)]
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
        .ok_or_else(|| anyhow::anyhow!("No configuration folder available"))?;
    Ok(base.join("lan-mouse").join("config.toml"))
}

pub fn save_peer_state(path: &Path, port: u16, clients: Vec<StoredClient>) -> Result<()> {
    let mut document: toml::Value = match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text)?, // Never silently overwrite malformed user settings.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(Default::default())
        }
        Err(e) => return Err(e.into()),
    };
    let table = document
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("Config must be a TOML table"))?;
    table.insert("port".into(), toml::Value::Integer(port.into()));
    table.insert("clients".into(), toml::Value::try_from(clients)?);
    for position in ["left", "right", "top", "bottom"] {
        table.remove(position);
    }
    atomic_write(path, toml::to_string_pretty(&document)?.as_bytes())
}

pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct TomlClient {
    pub display_name: Option<String>,
    pub hostname: Option<String>,
    pub host_name: Option<String>,
    pub ips: Option<Vec<IpAddr>>,
    pub port: Option<u16>,
    pub activate_on_startup: Option<bool>,
}

impl ConfigToml {
    pub fn new(path: &str) -> Result<ConfigToml, Box<dyn Error>> {
        let config = fs::read_to_string(path)?;
        log::info!("using config: \"{path}\"");
        Ok(toml::from_str::<_>(&config)?)
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct CliArgs {
    /// the listen port for lan-mouse
    #[arg(short, long)]
    port: Option<u16>,

    /// the frontend to use [cli | gtk]
    #[arg(short, long)]
    frontend: Option<String>,

    /// non-default config file location
    #[arg(short, long)]
    config: Option<String>,

    /// run only the service as a daemon without the frontend
    #[arg(short, long)]
    daemon: bool,

    /// Start the Windows GUI hidden in the notification area
    #[arg(long)]
    minimized: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Frontend {
    Gtk,
    Cli,
}

#[derive(Debug)]
pub struct Config {
    pub role: Role,
    pub device_name: String,
    pub config_path: PathBuf,
    pub frontend: Frontend,
    pub port: u16,
    pub clients: Vec<(TomlClient, Position)>,
    pub daemon: bool,
    pub release_bind: Vec<scancode::Linux>,
}

pub struct ConfigClient {
    pub display_name: Option<String>,
    pub ips: HashSet<IpAddr>,
    pub hostname: Option<String>,
    pub port: u16,
    pub pos: Position,
    pub active: bool,
}

const DEFAULT_RELEASE_KEYS: [scancode::Linux; 4] =
    [KeyLeftCtrl, KeyLeftShift, KeyLeftMeta, KeyLeftAlt];

impl Config {
    pub fn new() -> Result<Self> {
        let args = CliArgs::parse();
        let config_path = args
            .config
            .map(PathBuf::from)
            .unwrap_or(default_config_path()?);
        // The daemon receives an explicit path so GUI and service always edit the same file.
        env::set_var("LAN_MOUSE_CONFIG", &config_path);

        let config_toml = match ConfigToml::new(&config_path.to_string_lossy()) {
            Err(e) => {
                log::warn!("{}: {e}", config_path.display());
                log::warn!("Continuing without config file ...");
                None
            }
            Ok(c) => Some(c),
        };

        let frontend = match args.frontend {
            None => match &config_toml {
                Some(c) => c.frontend.clone(),
                None => None,
            },
            frontend => frontend,
        };

        let frontend = match frontend {
            #[cfg(feature = "gtk")]
            None => Frontend::Gtk,
            #[cfg(not(feature = "gtk"))]
            None => Frontend::Cli,
            Some(s) => match s.as_str() {
                "cli" => Frontend::Cli,
                "gtk" => Frontend::Gtk,
                _ => Frontend::Cli,
            },
        };

        let port = match args.port {
            Some(port) => port,
            None => match &config_toml {
                Some(c) => c.port.unwrap_or(DEFAULT_PORT),
                None => DEFAULT_PORT,
            },
        };
        anyhow::ensure!(port != 0, "Port must be between 1 and 65535");

        log::debug!("{config_toml:?}");
        let release_bind = config_toml
            .as_ref()
            .and_then(|c| c.release_bind.clone())
            .unwrap_or(Vec::from_iter(DEFAULT_RELEASE_KEYS.iter().cloned()));

        let mut clients: Vec<(TomlClient, Position)> = vec![];
        let role = config_toml
            .as_ref()
            .and_then(|c| c.role)
            .unwrap_or_default();
        let device_name = config_toml
            .as_ref()
            .and_then(|c| c.device_name.clone())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(computer_name);

        if let Some(config_toml) = config_toml {
            for c in config_toml.clients.unwrap_or_default() {
                clients.push((
                    TomlClient {
                        display_name: c.display_name,
                        hostname: c.hostname,
                        host_name: None,
                        ips: Some(c.ips),
                        port: Some(c.port),
                        activate_on_startup: Some(c.activate_on_startup),
                    },
                    c.position,
                ));
            }
            if let Some(c) = config_toml.right {
                clients.push((c, Position::Right))
            }
            if let Some(c) = config_toml.left {
                clients.push((c, Position::Left))
            }
            if let Some(c) = config_toml.top {
                clients.push((c, Position::Top))
            }
            if let Some(c) = config_toml.bottom {
                clients.push((c, Position::Bottom))
            }
        }

        let daemon = args.daemon;

        Ok(Config {
            role,
            device_name,
            config_path,
            daemon,
            frontend,
            clients,
            port,
            release_bind,
        })
    }

    pub fn get_clients(&self) -> Vec<ConfigClient> {
        self.clients
            .iter()
            .map(|(c, pos)| {
                let port = c.port.unwrap_or(DEFAULT_PORT);
                let ips: HashSet<IpAddr> = if let Some(ips) = c.ips.as_ref() {
                    HashSet::from_iter(ips.iter().cloned())
                } else {
                    HashSet::new()
                };
                let hostname = match &c.hostname {
                    Some(h) => Some(h.clone()),
                    None => c.host_name.clone(),
                };
                let active = c.activate_on_startup.unwrap_or(false);
                ConfigClient {
                    display_name: c.display_name.clone(),
                    ips,
                    hostname,
                    port,
                    pos: *pos,
                    active,
                }
            })
            .collect()
    }
}
