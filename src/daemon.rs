//! The background daemon: a Unix-socket control server around a [`Manager`], plus the
//! client helper used by the CLI. Unix only.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;
use tokio::time::timeout;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::manager::Manager;
use crate::protocol::{AddSource, Command, Response};
use crate::vlog;

const MAX_REQUEST: u64 = 64 * 1024;

/// `$WISPLET_HOME`, else `$XDG_DATA_HOME/wisplet`, else `~/.local/share/wisplet`.
pub fn state_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("WISPLET_HOME") {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(p).join("wisplet");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/share/wisplet")
}

pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("daemon.sock")
}

/// Unix socket paths are limited by `sun_path` (104 bytes on macOS, 108 on Linux).
const MAX_SOCKET_PATH: usize = 100;

/// Fail early, with an actionable message, when the socket path cannot be bound.
pub fn check_socket_path(socket: &Path) -> io::Result<()> {
    let len = socket.as_os_str().len();
    if len > MAX_SOCKET_PATH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "socket path is {len} bytes but Unix sockets allow at most ~{MAX_SOCKET_PATH}: {}\n\
                 Set WISPLET_HOME to a shorter directory.",
                socket.display()
            ),
        ));
    }
    Ok(())
}

/// Default download directory: `~/Downloads` when it exists, else the current directory.
pub fn default_download_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let d = PathBuf::from(home).join("Downloads");
        if d.is_dir() {
            return d;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

async fn bind(socket: &Path) -> io::Result<UnixListener> {
    if socket.exists() {
        if UnixStream::connect(socket).await.is_ok() {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, "a daemon is already running"));
        }
        std::fs::remove_file(socket)?; // stale socket from a crashed daemon
    }
    let listener = UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Run the daemon until `shutdown` is notified (or SIGINT/SIGTERM when `handle_signals`).
/// Must be called inside a `LocalSet`.
pub async fn run(
    cfg: Config,
    state_dir: &Path,
    default_dir: &Path,
    handle_signals: bool,
    shutdown: Rc<Notify>,
) -> io::Result<()> {
    let socket = socket_path(state_dir);
    check_socket_path(&socket)?;
    std::fs::create_dir_all(state_dir)?;
    std::fs::set_permissions(state_dir, std::fs::Permissions::from_mode(0o700))?;
    let listener = bind(&socket).await?;
    let manager = Manager::open(Rc::new(cfg), state_dir, default_dir)?;
    vlog!("daemon listening on {}", socket.display());
    let watcher = tokio::task::spawn_local(manager.clone().watch_loop());

    let signal = async {
        if handle_signals {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = signal(SignalKind::terminate()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = async {
                    match term.as_mut() {
                        Some(t) => { t.recv().await; }
                        None => std::future::pending::<()>().await,
                    }
                } => {}
            }
        } else {
            std::future::pending::<()>().await
        }
    };
    tokio::pin!(signal);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let (m, s) = (manager.clone(), shutdown.clone());
                tokio::task::spawn_local(async move {
                    if let Err(e) = handle_connection(stream, &m, &s).await {
                        vlog!("control connection: {e}");
                    }
                });
            }
            _ = shutdown.notified() => break,
            _ = &mut signal => break,
        }
    }

    watcher.abort();
    manager.shutdown();
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

async fn handle_connection(stream: UnixStream, manager: &Rc<Manager>, shutdown: &Notify) -> io::Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut raw = Vec::new();
    let mut reader = BufReader::new(rd.take(MAX_REQUEST));
    let n = timeout(Duration::from_secs(5), reader.read_until(b'\n', &mut raw))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "request timed out"))??;
    if n == 0 {
        return Ok(());
    }
    let line = String::from_utf8_lossy(&raw);
    let response = match Command::parse(line.trim()) {
        Ok(cmd) => execute(manager, cmd, shutdown),
        Err(e) => Response::Err(e),
    };
    // A client that never reads must not pin this task (and its socket) forever.
    timeout(Duration::from_secs(5), async {
        wr.write_all(response.encode().as_bytes()).await?;
        wr.shutdown().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "client not reading response"))?
}

fn execute(m: &Rc<Manager>, cmd: Command, shutdown: &Notify) -> Response {
    let fail = |e: Error| Response::Err(e.to_string());
    match cmd {
        Command::Ping => Response::Ok(vec!["pong".into()]),
        Command::List => Response::Ok(m.rows().iter().map(|r| r.encode()).collect()),
        Command::Shutdown => {
            shutdown.notify_one();
            Response::Ok(vec![])
        }
        Command::Add { source, dir, move_to } => {
            let dir = dir.map(PathBuf::from);
            let res = match source {
                AddSource::File(p) => m.add_file(Path::new(&p), dir.as_deref(), move_to),
                AddSource::Magnet(uri) => m.add_magnet(&uri, dir.as_deref(), move_to),
            };
            res.map(|id| Response::Ok(vec![id.to_string()])).unwrap_or_else(fail)
        }
        Command::Settings => {
            let show = |d: Option<PathBuf>| d.map(|d| d.display().to_string()).unwrap_or_default();
            Response::Ok(vec![
                format!("download_dir\t{}", m.download_dir().display()),
                format!("watch_dir\t{}", show(m.watch_dir())),
                format!("move_dir\t{}", show(m.move_dir())),
            ])
        }
        Command::SetWatchDir(d) => {
            m.set_watch_dir(d.as_deref().map(Path::new)).map(|_| Response::Ok(vec![])).unwrap_or_else(fail)
        }
        Command::SetMoveDir(d) => {
            m.set_move_dir(d.as_deref().map(Path::new)).map(|_| Response::Ok(vec![])).unwrap_or_else(fail)
        }
        Command::SetMove { id, spec } => m.set_torrent_move(id, spec).map(|_| Response::Ok(vec![])).unwrap_or_else(fail),
        Command::SetDownloadDir(d) => {
            m.set_download_dir(Path::new(&d)).map(|_| Response::Ok(vec![])).unwrap_or_else(fail)
        }
        Command::Info(id) => m.info(id).map(Response::Ok).unwrap_or_else(fail),
        Command::Pause(t) => m.pause(&t).map(|n| Response::Ok(vec![n.to_string()])).unwrap_or_else(fail),
        Command::Resume(t) => m.resume(&t).map(|n| Response::Ok(vec![n.to_string()])).unwrap_or_else(fail),
        Command::Remove { id, purge } => m.remove(id, purge).map(|_| Response::Ok(vec![])).unwrap_or_else(fail),
    }
}

/// Send one command to the daemon listening on `socket`.
pub async fn request(socket: &Path, cmd: &Command) -> Result<Vec<String>> {
    let mut stream = timeout(Duration::from_secs(2), UnixStream::connect(socket))
        .await
        .map_err(|_| Error::Timeout("connecting to daemon"))??;
    stream.write_all(format!("{}\n", cmd.encode()).as_bytes()).await?;
    let mut text = String::new();
    timeout(Duration::from_secs(30), stream.read_to_string(&mut text))
        .await
        .map_err(|_| Error::Timeout("daemon response"))??;
    match Response::parse(&text).map_err(Error::Failed)? {
        Response::Ok(lines) => Ok(lines),
        Response::Err(msg) => Err(Error::Failed(msg)),
    }
}

/// `true` if a daemon answers on `socket`.
pub async fn is_running(socket: &Path) -> bool {
    request(socket, &Command::Ping).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_honours_override() {
        // Run in a single test to avoid env races between parallel tests.
        std::env::set_var("WISPLET_HOME", "/tmp/td-test-home");
        assert_eq!(state_dir(), PathBuf::from("/tmp/td-test-home"));
        std::env::remove_var("WISPLET_HOME");
        std::env::set_var("XDG_DATA_HOME", "/tmp/xdg");
        assert_eq!(state_dir(), PathBuf::from("/tmp/xdg/wisplet"));
        std::env::remove_var("XDG_DATA_HOME");
        assert!(state_dir().ends_with(".local/share/wisplet"));
    }

    #[test]
    fn rejects_socket_paths_that_are_too_long() {
        let ok = socket_path(Path::new("/tmp/short"));
        assert!(check_socket_path(&ok).is_ok());
        let long = socket_path(&PathBuf::from(format!("/tmp/{}", "x".repeat(120))));
        let err = check_socket_path(&long).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("WISPLET_HOME"));
        // boundary: exactly at the limit is accepted, one over is not
        let at_limit = PathBuf::from(format!("/{}", "a".repeat(MAX_SOCKET_PATH - 1)));
        assert!(check_socket_path(&at_limit).is_ok());
        let over = PathBuf::from(format!("/{}", "a".repeat(MAX_SOCKET_PATH)));
        assert!(check_socket_path(&over).is_err());
    }

    #[test]
    fn socket_lives_in_state_dir() {
        assert_eq!(socket_path(Path::new("/a/b")), PathBuf::from("/a/b/daemon.sock"));
    }
}
