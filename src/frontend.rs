use anyhow::{anyhow, Result};
use std::{cmp::min, io::ErrorKind, str, time::Duration};

#[cfg(unix)]
use std::{
    env,
    path::{Path, PathBuf},
};

use tokio::io::ReadHalf;
use tokio::io::{AsyncReadExt, AsyncWriteExt, WriteHalf};

#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(windows)]
use tokio::net::TcpListener;
#[cfg(windows)]
use tokio::net::TcpStream;

use serde::{Deserialize, Serialize};

use crate::{
    client::{Client, ClientHandle, Position},
    config::{Config, Frontend},
};

/// cli frontend
pub mod cli;

/// gtk frontend
#[cfg(feature = "gtk")]
pub mod gtk;

pub fn run_frontend(config: &Config) -> Result<()> {
    match config.frontend {
        #[cfg(feature = "gtk")]
        Frontend::Gtk => {
            anyhow::ensure!(
                gtk::run(),
                "GTK frontend exited with failure; see logs/app.log"
            );
        }
        #[cfg(not(feature = "gtk"))]
        Frontend::Gtk => panic!("gtk frontend requested but feature not enabled!"),
        Frontend::Cli => {
            cli::run()?;
        }
    };
    Ok(())
}

fn exponential_back_off(duration: &mut Duration) -> &Duration {
    let new = duration.saturating_mul(2);
    *duration = min(new, Duration::from_secs(1));
    duration
}

/// wait for the lan-mouse socket to come online
#[cfg(unix)]
pub fn wait_for_service() -> Result<std::os::unix::net::UnixStream> {
    let socket_path = FrontendListener::socket_path()?;
    let mut duration = Duration::from_millis(1);
    loop {
        use std::os::unix::net::UnixStream;
        if let Ok(stream) = UnixStream::connect(&socket_path) {
            break Ok(stream);
        }
        // a signaling mechanism or inotify could be used to
        // improve this
        std::thread::sleep(*exponential_back_off(&mut duration));
    }
}

#[cfg(windows)]
pub fn wait_for_service() -> Result<std::net::TcpStream> {
    let mut duration = Duration::from_millis(1);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        use std::net::TcpStream;
        if let Ok(stream) = TcpStream::connect("127.0.0.1:5252") {
            break Ok(stream);
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!("Background service did not start within 10 seconds. Check logs/service.log and whether UDP port 4242 is already in use."));
        }
        std::thread::sleep(*exponential_back_off(&mut duration));
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
pub enum FrontendEvent {
    SetIdentity(String, crate::config::Role),
    RenamePeer(ClientHandle, String),
    ReleaseInput(),
    Status(),
    ShowWindow(),
    /// add a new client
    AddClient(Option<String>, u16, Position),
    /// activate/deactivate client
    ActivateClient(ClientHandle, bool),
    /// change the listen port (recreate udp listener)
    ChangePort(u16),
    /// remove a client
    DelClient(ClientHandle),
    /// request an enumertaion of all clients
    Enumerate(),
    /// service shutdown
    Shutdown(),
    /// update a client (hostname, port, position)
    UpdateClient(ClientHandle, Option<String>, u16, Position),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FrontendNotify {
    Status(ServiceStatus),
    ShowWindow(),
    NotifyClientActivate(ClientHandle, bool),
    NotifyClientCreate(Client),
    NotifyClientUpdate(Client),
    NotifyClientDelete(ClientHandle),
    /// new port, reason of failure (if failed)
    NotifyPortChange(u16, Option<String>),
    /// Client State, active
    Enumerate(Vec<(Client, bool)>),
    NotifyError(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub computer_name: String,
    pub device_name: String,
    pub role: crate::config::Role,
    pub capture_available: bool,
    pub sharing: bool,
    pub local_addresses: Vec<String>,
    pub peers: Vec<PeerStatus>,
    pub discovered: Vec<DiscoveredDevice>,
    pub discovery_status: String,
    pub port: u16,
    pub configured: usize,
    pub enabled: usize,
    pub packets_received: u64,
    pub packets_sent: u64,
    pub last_received_secs: Option<u64>,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerStatus {
    pub handle: ClientHandle,
    pub display_name: Option<String>,
    pub hostname: Option<String>,
    pub enabled: bool,
    pub responding: bool,
    pub latency_ms: Option<u64>,
    pub addresses: Vec<String>,
    pub last_seen_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    pub name: String,
    pub computer_name: String,
    pub address: String,
    pub port: u16,
    pub role: crate::config::Role,
}

pub struct FrontendListener {
    #[cfg(windows)]
    listener: TcpListener,
    #[cfg(unix)]
    listener: UnixListener,
    #[cfg(unix)]
    socket_path: PathBuf,
    #[cfg(unix)]
    tx_streams: Vec<WriteHalf<UnixStream>>,
    #[cfg(windows)]
    tx_streams: Vec<WriteHalf<TcpStream>>,
}

impl FrontendListener {
    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn socket_path() -> Result<PathBuf> {
        let xdg_runtime_dir = match env::var("XDG_RUNTIME_DIR") {
            Ok(d) => d,
            Err(e) => return Err(anyhow!("could not find XDG_RUNTIME_DIR: {e}")),
        };
        let xdg_runtime_dir = Path::new(xdg_runtime_dir.as_str());
        Ok(xdg_runtime_dir.join("lan-mouse-socket.sock"))
    }

    #[cfg(all(unix, target_os = "macos"))]
    pub fn socket_path() -> Result<PathBuf> {
        let home = match env::var("HOME") {
            Ok(d) => d,
            Err(e) => return Err(anyhow!("could not find HOME: {e}")),
        };
        let home = Path::new(home.as_str());
        let path = home
            .join("Library")
            .join("Caches")
            .join("lan-mouse-socket.sock");
        Ok(path)
    }

    pub async fn new() -> Option<Result<Self>> {
        #[cfg(unix)]
        let (socket_path, listener) = {
            let socket_path = match Self::socket_path() {
                Ok(path) => path,
                Err(e) => return Some(Err(e)),
            };

            log::debug!("remove socket: {:?}", socket_path);
            if socket_path.exists() {
                // try to connect to see if some other instance
                // of lan-mouse is already running
                match UnixStream::connect(&socket_path).await {
                    // connected -> lan-mouse is already running
                    Ok(_) => return None,
                    // lan-mouse is not running but a socket was left behind
                    Err(e) => {
                        log::debug!("{socket_path:?}: {e} - removing left behind socket");
                        let _ = std::fs::remove_file(&socket_path);
                    }
                }
            }
            let listener = match UnixListener::bind(&socket_path) {
                Ok(ls) => ls,
                // some other lan-mouse instance has bound the socket in the meantime
                Err(e) if e.kind() == ErrorKind::AddrInUse => return None,
                Err(e) => return Some(Err(anyhow!("failed to bind lan-mouse-socket: {e}"))),
            };
            (socket_path, listener)
        };

        #[cfg(windows)]
        let listener = match TcpListener::bind("127.0.0.1:5252").await {
            Ok(ls) => ls,
            // some other lan-mouse instance has bound the socket in the meantime
            Err(e) if e.kind() == ErrorKind::AddrInUse => return None,
            Err(e) => return Some(Err(anyhow!("failed to bind lan-mouse-socket: {e}"))),
        };

        let adapter = Self {
            listener,
            #[cfg(unix)]
            socket_path,
            tx_streams: vec![],
        };

        Some(Ok(adapter))
    }

    #[cfg(unix)]
    pub async fn accept(&mut self) -> Result<ReadHalf<UnixStream>> {
        let stream = self.listener.accept().await?.0;
        let (rx, tx) = tokio::io::split(stream);
        self.tx_streams.push(tx);
        Ok(rx)
    }

    #[cfg(windows)]
    pub async fn accept(&mut self) -> Result<ReadHalf<TcpStream>> {
        let stream = self.listener.accept().await?.0;
        let (rx, tx) = tokio::io::split(stream);
        self.tx_streams.push(tx);
        Ok(rx)
    }

    pub(crate) async fn notify_all(&mut self, notify: FrontendNotify) -> Result<()> {
        // encode event
        let json = serde_json::to_string(&notify).unwrap();
        let payload = json.as_bytes();
        let len = payload.len().to_be_bytes();
        log::debug!("json: {json}, len: {}", payload.len());

        let mut keep = vec![];
        // TODO do simultaneously
        for tx in self.tx_streams.iter_mut() {
            // write len + payload
            if tx.write_all(&len).await.is_err() {
                keep.push(false);
                continue;
            }
            if tx.write_all(payload).await.is_err() {
                keep.push(false);
                continue;
            }
            keep.push(true);
        }

        // could not find a better solution because async
        let mut keep = keep.into_iter();
        self.tx_streams.retain(|_| keep.next().unwrap());
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for FrontendListener {
    fn drop(&mut self) {
        log::debug!("remove socket: {:?}", self.socket_path);
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(unix)]
pub async fn read_event(stream: &mut ReadHalf<UnixStream>) -> Result<FrontendEvent> {
    let len = stream.read_u64().await?;
    anyhow::ensure!(len <= 65536, "Frontend frame exceeds 64 KiB");
    let mut buf = vec![0; len as usize];
    stream.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

#[cfg(windows)]
pub async fn read_event(stream: &mut ReadHalf<TcpStream>) -> Result<FrontendEvent> {
    let len = stream.read_u64().await?;
    anyhow::ensure!(len <= 65536, "Frontend frame exceeds 64 KiB");
    let mut buf = vec![0; len as usize];
    stream.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}
