//! Logging in to HTTP MCP servers with OAuth, and the stored login later runs
//! send as a bearer token.

use std::fmt;
use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use rune_context::mcp::ServerConfig;
use rune_context::mcp::client::Authorizer;
use rune_context::mcp::oauth::{
    self, AuthorizationMetadata, Discovery, LoopbackAuthorization, OAuthTokenStore,
};
use rune_core::error::{ErrorCode, Result, RuneError};
use rune_core::paths::{self, Paths};
use serde::{Deserialize, Serialize};

/// How long the browser has to finish a login.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// Longest login record read back.
const MAX_LOGIN_BYTES: u64 = 256 * 1024;

/// What a login keeps besides the tokens: where they were issued and to whom.
#[derive(Serialize, Deserialize)]
struct Login {
    client_id: String,
    discovery: Discovery,
}

/// The directory holding login records, and token files where no system store is used.
#[must_use]
pub fn directory(paths: &Paths) -> Utf8PathBuf {
    paths.state_root.join("mcp-oauth")
}

/// The token store of this platform: the keychain on macOS, private files elsewhere.
#[must_use]
pub fn token_store(paths: &Paths) -> OAuthTokenStore {
    #[cfg(target_os = "macos")]
    {
        OAuthTokenStore::with_system_store(
            directory(paths),
            rune_net::auth::lookup_in_system_store,
            rune_net::auth::store_in_system_store,
            rune_net::auth::remove_from_system_store,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        OAuthTokenStore::new(directory(paths))
    }
}

fn login_path(directory: &Utf8Path, name: &str) -> Utf8PathBuf {
    directory.join(format!("{name}.login.json"))
}

/// One server to log in to.
pub struct Target<'a> {
    pub name: &'a str,
    pub config: &'a ServerConfig,
    pub client_id: Option<&'a str>,
    pub scope: Option<&'a str>,
}

/// Discovers the server's authorization server, registers a client when none
/// is configured, and completes a loopback login, storing its tokens.
pub fn login(
    target: &Target<'_>,
    directory: &Utf8Path,
    store: &OAuthTokenStore,
    out: &mut dyn Write,
    open_browser: bool,
) -> Result<()> {
    let name = target.name;
    let discovery = oauth::discover(target.config)?;
    let metadata = discovery.metadata(target.config)?;
    let client_id = match target.client_id {
        Some(id) => id.to_owned(),
        None if metadata.registration_endpoint().is_some() => oauth::register_client(&metadata)?,
        None => {
            return Err(RuneError::new(
                ErrorCode::InvalidConfiguration,
                format!(
                    "MCP server {name} needs oauth_client_id: it does not support dynamic client registration"
                ),
            ));
        }
    };
    let mut flow = LoopbackAuthorization::start(
        &metadata,
        &client_id,
        target.scope.unwrap_or_default(),
        LOGIN_TIMEOUT,
    )?;
    writeln!(
        out,
        "open this address to log in to {name}:\n{}",
        flow.authorization_url()
    )?;
    out.flush()?;
    if open_browser {
        open(flow.authorization_url());
    }
    flow.complete(store)?;
    paths::create_dir_private(directory)?;
    let record = serde_json::to_string(&Login {
        client_id,
        discovery,
    })?;
    paths::write_private_atomic(&login_path(directory, name), &record)?;
    writeln!(out, "logged in to {name}")?;
    Ok(())
}

/// Removes a server's login and its tokens; false when there was none.
pub fn logout(
    name: &str,
    config: &ServerConfig,
    directory: &Utf8Path,
    store: &OAuthTokenStore,
) -> Result<bool> {
    let path = login_path(directory, name);
    let Some(login) = read_login(&path)? else {
        return Ok(false);
    };
    if let Ok(metadata) = login.discovery.metadata(config) {
        store.remove(&metadata, &login.client_id)?;
    }
    std::fs::remove_file(&path)?;
    Ok(true)
}

fn read_login(path: &Utf8Path) -> Result<Option<Login>> {
    let Some(text) = paths::read_private(path, MAX_LOGIN_BYTES)? else {
        return Ok(None);
    };
    serde_json::from_str(&text).map(Some).map_err(|_| {
        RuneError::new(
            ErrorCode::InvalidConfiguration,
            format!("the MCP login record {path} is not valid"),
        )
    })
}

/// Opens an address in the default browser, ignoring failure.
fn open(url: &str) {
    use std::process::{Command, Stdio};
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = Command::new("xdg-open");
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/c", "start", ""]);
        command
    };
    let _ = command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// A stored login, as the bearer token source of one HTTP server.
pub struct StoredLogin {
    metadata: AuthorizationMetadata,
    client_id: String,
    store: OAuthTokenStore,
    access: Mutex<Option<String>>,
}

impl fmt::Debug for StoredLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredLogin")
            .field("issuer", &self.metadata.issuer())
            .finish_non_exhaustive()
    }
}

impl StoredLogin {
    /// The login of `name`, when one was stored and still matches its configuration.
    #[must_use]
    pub fn load(
        name: &str,
        config: &ServerConfig,
        directory: &Utf8Path,
        store: OAuthTokenStore,
    ) -> Option<Self> {
        let login = read_login(&login_path(directory, name)).ok()??;
        let metadata = login.discovery.metadata(config).ok()?;
        Some(Self {
            metadata,
            client_id: login.client_id,
            store,
            access: Mutex::new(None),
        })
    }
}

impl Authorizer for StoredLogin {
    fn token(&self) -> Option<String> {
        let mut access = self.access.lock().ok()?;
        if access.is_none() {
            *access = self
                .store
                .load(&self.metadata, &self.client_id)
                .ok()
                .flatten()
                .map(|tokens| tokens.access_token().to_owned());
        }
        access.clone()
    }

    fn refresh(&self) -> bool {
        let Ok(Some(tokens)) = self.store.load(&self.metadata, &self.client_id) else {
            return false;
        };
        if tokens.refresh_token().is_none() {
            return false;
        }
        let Ok(refreshed) = tokens.refresh(&self.metadata, &self.client_id) else {
            // A rejected refresh token cannot recover: the user logs in again.
            let _ = self.store.remove(&self.metadata, &self.client_id);
            if let Ok(mut access) = self.access.lock() {
                *access = None;
            }
            return false;
        };
        if self
            .store
            .save(&self.metadata, &self.client_id, &refreshed)
            .is_err()
        {
            return false;
        }
        if let Ok(mut access) = self.access.lock() {
            *access = Some(refreshed.access_token().to_owned());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::net::TcpStream;
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::sync::mpsc;

    /// The fixture server; dropping it closes its standard input, which stops it.
    struct Fixture {
        child: Child,
        port: u16,
    }

    impl Fixture {
        fn start() -> Self {
            let mut child = Command::new("python3")
                .args(["-c", include_str!("../tests/mcp_oauth_server.py"), "oauth"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("python3 runs the OAuth fixture");
            let mut line = String::new();
            BufReader::new(child.stdout.take().expect("stdout"))
                .read_line(&mut line)
                .expect("port");
            let port = line.trim().parse().expect("port number");
            Self { child, port }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}/mcp", self.port)
        }

        fn request(&self, method: &str, path: &str, body: &str) -> String {
            raw(self.port, method, path, body)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            drop(self.child.stdin.take());
            let _ = self.child.wait();
        }
    }

    /// One HTTP/1.1 request on a fresh connection; the whole response.
    fn raw(port: u16, method: &str, target: &str, body: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .expect("send");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    /// Lines written by the login, as they are written.
    struct Lines(mpsc::Sender<String>, String);

    impl Write for Lines {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.1.push_str(&String::from_utf8_lossy(bytes));
            while let Some(end) = self.1.find('\n') {
                let line: String = self.1.drain(..=end).collect();
                let _ = self.0.send(line.trim_end().to_owned());
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn stats(fixture: &Fixture) -> serde_json::Value {
        let response = fixture.request("GET", "/stats", "");
        let body = response.split("\r\n\r\n").nth(1).expect("body");
        serde_json::from_str(body).expect("stats")
    }

    #[test]
    fn oauth_login_stores_and_refreshes_tokens() {
        let fixture = Fixture::start();
        let dir = tempfile::tempdir().expect("tempdir");
        let directory = Utf8PathBuf::from_path_buf(dir.path().join("mcp-oauth")).expect("utf8");
        let config = ServerConfig::parse(&serde_json::json!({
            "name": "remote",
            "transport": {"transport": "http", "url": fixture.url()},
        }))
        .expect("config");

        let (sender, lines) = mpsc::channel();
        let login = {
            let config = config.clone();
            let directory = directory.clone();
            std::thread::spawn(move || {
                let target = Target {
                    name: "remote",
                    config: &config,
                    client_id: None,
                    scope: Some("tools"),
                };
                let store = OAuthTokenStore::new(directory.clone());
                login(
                    &target,
                    &directory,
                    &store,
                    &mut Lines(sender, String::new()),
                    false,
                )
            })
        };
        let timeout = Duration::from_secs(30);
        assert_eq!(
            lines.recv_timeout(timeout).expect("prompt"),
            "open this address to log in to remote:"
        );
        let address = lines.recv_timeout(timeout).expect("address");
        let path = address
            .strip_prefix(&format!("http://127.0.0.1:{}", fixture.port))
            .expect("the authorization endpoint");
        // Follow the address the way a browser would: one GET, then the redirect.
        let redirect = fixture.request("GET", path, "");
        let location = redirect
            .lines()
            .find_map(|line| line.strip_prefix("Location: "))
            .expect("redirect to the callback")
            .trim()
            .to_owned();
        let callback = location
            .strip_prefix("http://127.0.0.1:")
            .expect("loopback");
        let (port, target) = callback.split_at(callback.find('/').expect("path"));
        let reply = raw(port.parse().expect("port"), "GET", target, "");
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        login.join().expect("login thread").expect("login");
        assert_eq!(
            lines.recv_timeout(timeout).expect("done"),
            "logged in to remote"
        );
        assert_eq!(stats(&fixture)["registrations"], 1);

        let stored = StoredLogin::load(
            "remote",
            &config,
            &directory,
            OAuthTokenStore::new(directory.clone()),
        )
        .expect("a stored login");
        assert_eq!(stored.token().as_deref(), Some("access-1"));
        // Every token and login file is the user's alone.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut files = 0;
            for entry in std::fs::read_dir(&directory).expect("token directory") {
                let entry = entry.expect("entry");
                let mode = entry.metadata().expect("metadata").permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{}", entry.path().display());
                files += 1;
            }
            assert!(files > 0);
            let mode = std::fs::metadata(&directory)
                .expect("directory")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }

        // Requests carry the stored token; a later 401 costs exactly one refresh.
        let client = rune_context::mcp::Client::new(&rune_core::budget::BudgetSet::new());
        client.authorize_with("remote", Arc::new(stored));
        let report = client
            .connect_all(std::slice::from_ref(&config))
            .expect("connect");
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        fixture.request("POST", "/expire", "");
        let outcome = client
            .call("mcp_remote_echo", &serde_json::json!({}))
            .expect("call after refresh");
        assert_eq!(outcome.text, "echo from access-2");
        assert_eq!(stats(&fixture)["refreshes"], 1);
        let reloaded = StoredLogin::load(
            "remote",
            &config,
            &directory,
            OAuthTokenStore::new(directory.clone()),
        )
        .expect("still logged in");
        assert_eq!(reloaded.token().as_deref(), Some("access-2"));

        // A refused refresh leaves the server needing a login, without tokens.
        fixture.request("POST", "/expire", "reject");
        let error = client
            .call("mcp_remote_echo", &serde_json::json!({}))
            .expect_err("refresh refused");
        assert!(error.message().contains("needs login"), "{error:?}");
        let store = OAuthTokenStore::new(directory.clone());
        let metadata = read_login(&login_path(&directory, "remote"))
            .expect("record")
            .expect("present")
            .discovery
            .metadata(&config)
            .expect("metadata");
        assert!(
            store
                .load(&metadata, "registered-client")
                .expect("load")
                .is_none()
        );
        client.shutdown();

        assert!(logout("remote", &config, &directory, &store).expect("logout"));
        assert!(!logout("remote", &config, &directory, &store).expect("logout again"));
    }
}
