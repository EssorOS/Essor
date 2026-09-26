//! Helpers for tests that exercise the bundled Node sync server.

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for a spawned server to start accepting connections.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// Kills the spawned server when the test ends, even on panic.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Server {
    fn spawn(port: u16, data_dir: &Path) -> Self {
        let child = Command::new("node")
            .arg("dist/server.js")
            .current_dir(manifest_dir().join("server"))
            .env("PORT", port.to_string())
            .env("DATA_DIR", data_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to start node server");
        Self(child)
    }
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The tests need Node and a built server; skip quietly when either is absent.
fn server_available() -> bool {
    Command::new("node")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
        && manifest_dir().join("server/dist/server.js").exists()
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// A running sync server with a private data directory. Dropping it kills the
/// server (if still running) and removes the directory.
pub(crate) struct TestServer {
    child: Option<Server>,
    data_dir: PathBuf,
    port: u16,
}

impl TestServer {
    /// Start a server, or return `None` after printing a skip note when Node or
    /// the built server is unavailable.
    pub(crate) fn start(name: &str) -> Option<Self> {
        if !server_available() {
            let hint = "run `npm install && npm run build` in server/ first";
            if std::env::var_os("CI").is_some() {
                panic!("sync server unavailable: {hint}");
            }
            eprintln!("skipping: {hint}");
            return None;
        }
        let mut server = Self {
            child: None,
            data_dir: temp_dir(&format!("server-{name}")),
            port: free_port(),
        };
        server.spawn();
        Some(server)
    }

    /// The `ws://` base URL to point clients at.
    pub(crate) fn url(&self) -> String {
        format!("ws://127.0.0.1:{}", self.port)
    }

    /// Stop the server, keeping the data directory intact for [`Self::restart`].
    pub(crate) fn stop(&mut self) {
        self.child = None;
    }

    /// Start a fresh server on the same port and data directory.
    pub(crate) fn restart(&mut self) {
        self.spawn();
    }

    /// Stop the server, erase its data directory, and start a fresh instance on
    /// the same port. Simulates a server whose persistence was lost.
    pub(crate) fn wipe(&mut self) {
        self.child = None;
        let _ = std::fs::remove_dir_all(&self.data_dir);
        self.spawn();
    }

    fn spawn(&mut self) {
        self.child = Some(Server::spawn(self.port, &self.data_dir));
        assert!(
            wait_for_port(self.port),
            "server did not start on port {}",
            self.port
        );
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.child = None;
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

/// A fresh, empty temp directory named for `name`.
pub(crate) fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("essor-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn wait_for_port(port: u16) -> bool {
    wait_until(
        || TcpStream::connect(("127.0.0.1", port)).is_ok(),
        START_TIMEOUT,
    )
}

pub(crate) fn wait_until(condition: impl Fn() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}
