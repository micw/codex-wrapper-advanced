//! Where the server listens and who gets in.
//!
//! Two deployment pictures, and they demand opposite things:
//!
//! * **Local** (companion process of an application): unix socket. The file
//!   permissions *are* the access control — whoever can open the socket runs
//!   under the same uid and could read `auth.json` directly anyway. A token
//!   protects nothing there and is not required.
//! * **Network** (own server): TCP. Here the key is the *only* barrier, so it
//!   must be stable, plural and individually revocable — supplied by the
//!   operator, not rolled by the process.
//!
//! A token minted at startup satisfies neither: superfluous locally, too weak on
//! the network (ephemeral, exactly one, not revocable). There is none here.

use std::path::PathBuf;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use http::HeaderMap;

/// Where the server listens.
#[derive(Debug, Clone)]
pub enum Listen {
    /// `unix:/path/to/socket`
    Unix(PathBuf),
    /// `127.0.0.1:8080`, `0.0.0.0:8080`
    Tcp(std::net::SocketAddr),
}

impl std::str::FromStr for Listen {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        if let Some(path) = value.strip_prefix("unix:") {
            if path.is_empty() {
                bail!("unix: needs a path, e.g. unix:/run/codex/sock");
            }
            return Ok(Self::Unix(PathBuf::from(path)));
        }
        // A bare port number for convenience — but on loopback, never 0.0.0.0.
        if let Ok(port) = value.parse::<u16>() {
            return Ok(Self::Tcp(std::net::SocketAddr::from((
                std::net::Ipv4Addr::LOCALHOST,
                port,
            ))));
        }
        value
            .parse::<std::net::SocketAddr>()
            .map(Self::Tcp)
            .with_context(|| {
                format!(
                    "cannot parse listen address {value:?}; expected \
                     unix:/path, 127.0.0.1:8080 or a port number"
                )
            })
    }
}

impl std::fmt::Display for Listen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "unix:{}", path.display()),
            Self::Tcp(addr) => write!(f, "http://{addr}"),
        }
    }
}

/// A named API key.
///
/// The name is there for attribution in the log. The subscription quota is the
/// shared resource, and a total across all clients does not say who spent it.
#[derive(Debug, Clone)]
pub struct ApiKey {
    pub name: String,
    secret: String,
}

/// The configured keys. Empty means no authentication.
#[derive(Debug, Clone, Default)]
pub struct ApiKeys(Vec<ApiKey>);

impl ApiKeys {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Reads keys from a file: one line `name:secret`.
    ///
    /// Blank lines and `#` comments are skipped. Deliberately a file rather than
    /// an environment variable: file permissions are inspectable, and a
    /// process's environment is readable more widely than one tends to assume.
    pub fn from_file(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading API keys: {}", path.display()))?;

        let mut keys = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((name, secret)) = line.split_once(':') else {
                bail!("{}:{}: expected `name:secret`", path.display(), index + 1);
            };
            let (name, secret) = (name.trim(), secret.trim());
            if name.is_empty() || secret.is_empty() {
                bail!(
                    "{}:{}: name and secret must not be empty",
                    path.display(),
                    index + 1
                );
            }
            // Short secrets are the most common way to fool oneself here.
            if secret.len() < 16 {
                bail!(
                    "{}:{}: secret for {name:?} is too short ({} characters, minimum 16)",
                    path.display(),
                    index + 1,
                    secret.len()
                );
            }
            keys.push(ApiKey {
                name: name.to_string(),
                secret: secret.to_string(),
            });
        }
        Ok(Self(keys))
    }

    /// Checks `Authorization: Bearer …` and returns the key's name. `None` means
    /// rejected.
    ///
    /// With an empty key list every caller is accepted as `local` — that case is
    /// only permitted on a unix socket, which [`validate`] enforces at startup.
    pub fn authenticate(&self, headers: &HeaderMap) -> Option<String> {
        if self.0.is_empty() {
            return Some("local".to_string());
        }
        let presented = headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))?;

        // Walk all keys without breaking early: otherwise the runtime reveals how
        // many keys exist and which one matched.
        let mut matched = None;
        for key in &self.0 {
            if constant_time_eq(presented, &key.secret) {
                matched = Some(key.name.clone());
            }
        }
        matched
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Validates the combination of transport and keys before anything listens.
///
/// **Fail closed.** A TCP listener without keys would be open access to somebody
/// else's subscription quota — that must not be the default, and must not be
/// reachable by accident either. Conversely, keys on a unix socket are allowed
/// but pointless; that only earns a hint.
pub fn validate(listen: &Listen, keys: &ApiKeys) -> Result<()> {
    match listen {
        Listen::Tcp(addr) if keys.is_empty() => {
            bail!(
                "TCP listener on {addr} without API keys. Anyone reaching the port \
                 could spend the ChatGPT quota.\n\
                 Either pass --api-keys <file> or switch to a unix socket \
                 (--listen unix:/path), where file permissions are the access \
                 control."
            );
        }
        Listen::Tcp(addr) if !addr.ip().is_loopback() => {
            eprintln!(
                "Note: bound to {}, so reachable from outside this machine. \
                 {} API key(s) active. Sharing access to this endpoint may violate \
                 the provider's Terms of Service.",
                addr.ip(),
                keys.len()
            );
        }
        Listen::Unix(_) if !keys.is_empty() => {
            eprintln!(
                "Note: API keys on a unix socket have no effect — whoever may open \
                 the socket already has file access. The socket's permissions are \
                 the access control."
            );
        }
        _ => {}
    }
    Ok(())
}
