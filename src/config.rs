//! Multi-account configuration: on-disk `config.toml`, secret resolution
//! (inline value, shell command, or macOS Keychain), and a legacy env-var
//! fallback so existing single-account setups keep working unchanged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// One mail account as written in `config.toml`. Every field is optional so a
/// file can be sparse; [`Settings::resolve`] fills defaults and produces a
/// fully-populated [`ResolvedAccount`].
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AccountConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_user: Option<String>,
    /// Inline secret (discouraged; prefer `imap_pass_cmd`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_pass: Option<String>,
    /// Shell command whose stdout is the secret, e.g.
    /// `security find-generic-password -s mail_mcp -a personal-imap -w`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_pass_cmd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smtp_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smtp_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smtp_user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smtp_pass: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smtp_pass_cmd: Option<String>,
    /// `From:` header. Set to an alias such as `hello@example.com` to send as it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smtp_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smtp_write_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attachment_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent_folder: Option<String>,
}

/// A fully-resolved account ready for use by IMAP/SMTP helpers.
#[derive(Debug, Clone)]
pub struct ResolvedAccount {
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_user: String,
    pub imap_pass: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_user: String,
    pub smtp_pass: String,
    pub smtp_from: String,
    pub smtp_write_enabled: bool,
    pub max_attachment_bytes: usize,
    pub sent_folder: String,
}

pub fn default_sent_folder(imap_host: &str) -> String {
    if imap_host.eq_ignore_ascii_case("imap.one.com") {
        "INBOX.Sent".into()
    } else {
        "Sent".into()
    }
}

/// Top-level on-disk configuration.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Settings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_account: Option<String>,
    #[serde(default)]
    pub accounts: HashMap<String, AccountConfig>,
}

impl Settings {
    /// Load configuration: `MAIL_MCP_CONFIG` path if set, else
    /// `~/.config/mail_mcp/config.toml`. Returns `None` when no file exists,
    /// signalling the caller to fall back to environment variables.
    pub fn load() -> anyhow::Result<Option<Settings>> {
        let Some(path) = config_path() else {
            return Ok(None);
        };
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
        let settings: Settings = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("invalid config {}: {e}", path.display()))?;
        Ok(Some(settings))
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Resolve a named account (or the default) into a usable config.
    pub fn resolve(&self, name: Option<&str>) -> anyhow::Result<ResolvedAccount> {
        let key = name
            .map(|s| s.to_string())
            .or_else(|| self.default_account.clone())
            .ok_or_else(|| anyhow::anyhow!("no account given and no default_account set"))?;
        let acct = self
            .accounts
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("unknown account '{key}'"))?;
        acct.resolve(&key)
    }
}

impl AccountConfig {
    /// Produce a fully-populated account, resolving secrets and applying
    /// provider-neutral defaults.
    pub fn resolve(&self, name: &str) -> anyhow::Result<ResolvedAccount> {
        let imap_host = self
            .imap_host
            .clone()
            .unwrap_or_else(|| "imap.gmail.com".into());
        let imap_user = self
            .imap_user
            .clone()
            .ok_or_else(|| anyhow::anyhow!("account '{name}': imap_user is required"))?;
        let imap_pass = resolve_secret(
            self.imap_pass.as_deref(),
            self.imap_pass_cmd.as_deref(),
            &format!("{name}-imap"),
        )?;
        let smtp_user = self.smtp_user.clone().unwrap_or_else(|| imap_user.clone());
        let smtp_pass = match (&self.smtp_pass, &self.smtp_pass_cmd) {
            (None, None) => imap_pass.clone(),
            _ => resolve_secret(
                self.smtp_pass.as_deref(),
                self.smtp_pass_cmd.as_deref(),
                &format!("{name}-smtp"),
            )?,
        };
        let sent_folder = self
            .sent_folder
            .clone()
            .unwrap_or_else(|| default_sent_folder(&imap_host));
        Ok(ResolvedAccount {
            imap_host,
            imap_port: self.imap_port.unwrap_or(993),
            imap_user: imap_user.clone(),
            imap_pass,
            smtp_host: self
                .smtp_host
                .clone()
                .unwrap_or_else(|| "smtp.gmail.com".into()),
            smtp_port: self.smtp_port.unwrap_or(587),
            smtp_user,
            smtp_pass,
            smtp_from: self.smtp_from.clone().unwrap_or_else(|| imap_user.clone()),
            smtp_write_enabled: self.smtp_write_enabled.unwrap_or(false),
            max_attachment_bytes: self.max_attachment_bytes.unwrap_or(25 * 1024 * 1024),
            sent_folder,
        })
    }
}

/// Resolve a secret from an inline value or a shell command, trimming the
/// trailing newline commands commonly append.
fn resolve_secret(
    inline: Option<&str>,
    cmd: Option<&str>,
    keychain_account: &str,
) -> anyhow::Result<String> {
    if let Some(value) = inline {
        if !value.is_empty() {
            return Ok(value.to_string());
        }
    }
    if let Some(command) = cmd {
        return run_secret_cmd(command);
    }
    // No explicit secret: fall back to macOS Keychain lookup by convention.
    if cfg!(target_os = "macos") {
        if let Ok(value) = run_secret_cmd(&format!(
            "security find-generic-password -s mail_mcp -a {keychain_account} -w"
        )) {
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }
    anyhow::bail!("no secret available (set *_pass, *_pass_cmd, or a Keychain entry)")
}

fn run_secret_cmd(command: &str) -> anyhow::Result<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .map_err(|e| anyhow::anyhow!("secret command failed to start: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "secret command exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Path to the config file, honouring `MAIL_MCP_CONFIG` then XDG/HOME.
pub fn config_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("MAIL_MCP_CONFIG") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("mail_mcp").join("config.toml"))
}

/// Legacy single-account loader: builds a `Settings` from the original flat
/// environment variables so existing deployments keep working untouched.
pub fn settings_from_env() -> anyhow::Result<Settings> {
    let imap_user = std::env::var("IMAP_USER").map_err(|_| anyhow::anyhow!("IMAP_USER not set"))?;
    let imap_pass = std::env::var("IMAP_PASS").map_err(|_| anyhow::anyhow!("IMAP_PASS not set"))?;
    let parse_bool = |v: String| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    };
    let account = AccountConfig {
        imap_host: std::env::var("IMAP_HOST").ok(),
        imap_port: std::env::var("IMAP_PORT").ok().and_then(|v| v.parse().ok()),
        imap_user: Some(imap_user.clone()),
        imap_pass: Some(imap_pass.clone()),
        imap_pass_cmd: None,
        smtp_host: std::env::var("SMTP_HOST").ok(),
        smtp_port: std::env::var("SMTP_PORT").ok().and_then(|v| v.parse().ok()),
        smtp_user: std::env::var("SMTP_USER").ok(),
        smtp_pass: std::env::var("SMTP_PASS").ok(),
        smtp_pass_cmd: None,
        smtp_from: std::env::var("SMTP_FROM").ok(),
        smtp_write_enabled: std::env::var("MAIL_SMTP_WRITE_ENABLED")
            .ok()
            .map(parse_bool),
        max_attachment_bytes: std::env::var("MAIL_MAX_ATTACHMENT_BYTES")
            .ok()
            .and_then(|v| v.parse().ok()),
        sent_folder: std::env::var("MAIL_SENT_FOLDER").ok(),
    };
    let mut accounts = HashMap::new();
    accounts.insert("default".to_string(), account);
    Ok(Settings {
        default_account: Some("default".into()),
        accounts,
    })
}

/// Process-wide cache of resolved accounts, keyed by account name.
static RESOLVE_CACHE: std::sync::OnceLock<Mutex<HashMap<String, Arc<ResolvedAccount>>>> =
    std::sync::OnceLock::new();

/// Resolve a named account (or the default), reusing an entry resolved earlier
/// in this process so per-call selection does not re-run secret commands on
/// every tool invocation. A failed resolution is never cached.
pub fn resolve_cached(
    settings: &Settings,
    name: Option<&str>,
) -> anyhow::Result<Arc<ResolvedAccount>> {
    let key = name
        .map(str::to_string)
        .or_else(|| settings.default_account.clone())
        .ok_or_else(|| anyhow::anyhow!("no account given and no default_account set"))?;
    let cache = RESOLVE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(found) = cache
        .lock()
        .expect("account cache poisoned")
        .get(&key)
        .cloned()
    {
        return Ok(found);
    }
    let resolved = Arc::new(settings.resolve(Some(&key))?);
    cache
        .lock()
        .expect("account cache poisoned")
        .insert(key, resolved.clone());
    Ok(resolved)
}

/// Load settings from file, falling back to env vars when no file exists.
pub fn load_settings() -> anyhow::Result<Settings> {
    match Settings::load()? {
        Some(settings) => Ok(settings),
        None => settings_from_env(),
    }
}
