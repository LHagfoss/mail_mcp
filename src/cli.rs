//! Subcommand surface for `mail_mcp`: server mode plus account management
//! (`accounts add/list/remove/default/test`). Secrets are written to the macOS
//! Keychain and referenced from `config.toml` via `*_pass_cmd`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::{self, AccountConfig};

#[derive(Debug, Parser)]
#[command(
    name = "mail_mcp",
    version,
    about = "Mail MCP server (stdio) and account CLI"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the MCP server over stdio (default when no subcommand is given).
    Serve(ServeArgs),
    /// Manage mail accounts stored in the config file.
    Accounts(AccountsArgs),
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Account name to serve; defaults to `default_account` in the config.
    #[arg(long)]
    pub account: Option<String>,
}

#[derive(Debug, Args)]
pub struct AccountsArgs {
    #[command(subcommand)]
    pub command: AccountsCommand,
}

#[derive(Debug, Subcommand)]
pub enum AccountsCommand {
    /// List configured accounts.
    List,
    /// Add or update an account (prompts for secrets, stores them in Keychain).
    Add(AddArgs),
    /// Remove an account from the config file.
    Remove {
        /// Account name to remove.
        name: String,
    },
    /// Set the default account.
    Default {
        /// Account name to make default.
        name: String,
    },
    /// Test IMAP login and SMTP reachability without sending mail.
    Test {
        /// Account to test; defaults to the default account.
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Account name, e.g. `personal` or `work`.
    pub name: String,
    #[arg(long, default_value = "imap.gmail.com")]
    pub imap_host: String,
    #[arg(long, default_value_t = 993)]
    pub imap_port: u16,
    /// IMAP/login username (usually the mailbox address).
    #[arg(long)]
    pub imap_user: String,
    #[arg(long, default_value = "smtp.gmail.com")]
    pub smtp_host: String,
    #[arg(long, default_value_t = 587)]
    pub smtp_port: u16,
    /// SMTP username; defaults to the IMAP user.
    #[arg(long)]
    pub smtp_user: Option<String>,
    /// `From:` address. Use an alias like `hello@example.com` to send as it.
    #[arg(long)]
    pub smtp_from: Option<String>,
    /// Enable outbound mail (send_email/reply_email) for this account.
    #[arg(long, default_value_t = false)]
    pub smtp_write_enabled: bool,
    /// IMAP folder for sent copies; provider default when omitted.
    #[arg(long)]
    pub sent_folder: Option<String>,
    /// Use one password for both IMAP and SMTP (default: prompt once for each).
    #[arg(long, default_value_t = true)]
    pub shared_password: bool,
    /// Make this the default account.
    #[arg(long, default_value_t = false)]
    pub make_default: bool,
}

fn config_or_default_path() -> PathBuf {
    config::config_path().unwrap_or_else(|| PathBuf::from("mail_mcp.toml"))
}

impl Cli {
    pub fn dispatch(self) -> anyhow::Result<Option<String>> {
        match self.command {
            None => Ok(None), // no subcommand -> serve with default account
            Some(Command::Serve(args)) => {
                if let Some(account) = args.account {
                    std::env::set_var("MAIL_MCP_ACCOUNT", account);
                }
                Ok(None)
            }
            Some(Command::Accounts(args)) => run_accounts(args.command),
        }
    }
}

fn run_accounts(command: AccountsCommand) -> anyhow::Result<Option<String>> {
    let path = config_or_default_path();
    match command {
        AccountsCommand::List => {
            let mut settings = config::Settings::load()?.unwrap_or_default();
            if settings.accounts.is_empty() {
                // Describe the env-var fallback without a config file.
                if let Ok(env_settings) = config::settings_from_env() {
                    settings = env_settings;
                }
            }
            let mut names: Vec<&String> = settings.accounts.keys().collect();
            names.sort();
            if names.is_empty() {
                return Ok(Some("no accounts configured".into()));
            }
            let default = settings.default_account.clone().unwrap_or_default();
            let mut lines = Vec::new();
            for name in names {
                let marker = if *name == default { " (default)" } else { "" };
                let acct = &settings.accounts[name];
                let user = acct.imap_user.clone().unwrap_or_default();
                let from = acct.smtp_from.clone().unwrap_or_else(|| user.clone());
                lines.push(format!("{name}{marker}\n  user: {user}\n  from: {from}"));
            }
            Ok(Some(lines.join("\n")))
        }
        AccountsCommand::Add(args) => {
            let mut settings = config::Settings::load()?.unwrap_or_default();
            let smtp_user = args
                .smtp_user
                .clone()
                .unwrap_or_else(|| args.imap_user.clone());
            let smtp_from = args
                .smtp_from
                .clone()
                .unwrap_or_else(|| args.imap_user.clone());
            let imap_secret = prompt_secret(&format!(
                "IMAP app password for {} <{}>: ",
                args.imap_user, args.name
            ))?;
            store_keychain(&format!("{}-imap", args.name), &imap_secret)?;
            let smtp_pass_cmd = if args.shared_password {
                Some(keychain_cmd(&format!("{}-imap", args.name)))
                // reuse the same entry for SMTP
            } else {
                let smtp_secret = prompt_secret(&format!(
                    "SMTP app password for {} <{}>: ",
                    smtp_user, args.name
                ))?;
                store_keychain(&format!("{}-smtp", args.name), &smtp_secret)?;
                Some(keychain_cmd(&format!("{}-smtp", args.name)))
            };
            let account = AccountConfig {
                imap_host: Some(args.imap_host),
                imap_port: Some(args.imap_port),
                imap_user: Some(args.imap_user),
                imap_pass: None,
                imap_pass_cmd: Some(keychain_cmd(&format!("{}-imap", args.name))),
                smtp_host: Some(args.smtp_host),
                smtp_port: Some(args.smtp_port),
                smtp_user: Some(smtp_user),
                smtp_pass: None,
                smtp_pass_cmd,
                smtp_from: Some(smtp_from),
                smtp_write_enabled: Some(args.smtp_write_enabled),
                max_attachment_bytes: None,
                sent_folder: args.sent_folder,
            };
            settings.accounts.insert(args.name.clone(), account);
            if args.make_default || settings.default_account.is_none() {
                settings.default_account = Some(args.name.clone());
            }
            settings.save(&path)?;
            Ok(Some(format!(
                "account '{}' saved to {}",
                args.name,
                path.display()
            )))
        }
        AccountsCommand::Remove { name } => {
            let mut settings = config::Settings::load()?
                .ok_or_else(|| anyhow::anyhow!("no config file at {}", path.display()))?;
            if settings.accounts.remove(&name).is_none() {
                anyhow::bail!("account '{name}' not found");
            }
            if settings.default_account.as_deref() == Some(name.as_str()) {
                settings.default_account = None;
            }
            settings.save(&path)?;
            Ok(Some(format!("account '{name}' removed")))
        }
        AccountsCommand::Default { name } => {
            let mut settings = config::Settings::load()?
                .ok_or_else(|| anyhow::anyhow!("no config file at {}", path.display()))?;
            if !settings.accounts.contains_key(&name) {
                anyhow::bail!("account '{name}' not found");
            }
            settings.default_account = Some(name.clone());
            settings.save(&path)?;
            Ok(Some(format!("default account set to '{name}'")))
        }
        AccountsCommand::Test { account } => {
            let settings = config::load_settings()?;
            let resolved = settings.resolve(account.as_deref())?;
            test_account(&resolved)
        }
    }
}

fn test_account(acct: &crate::config::ResolvedAccount) -> anyhow::Result<Option<String>> {
    use native_tls::TlsConnector;
    let tls = TlsConnector::builder().build()?;
    let client = imap::connect(
        (acct.imap_host.as_str(), acct.imap_port),
        acct.imap_host.as_str(),
        &tls,
    )?;
    let mut session = client
        .login(acct.imap_user.as_str(), acct.imap_pass.as_str())
        .map_err(|(e, _)| anyhow::anyhow!("IMAP login failed: {e}"))?;
    session.logout().ok();
    Ok(Some(format!(
        "IMAP OK: {}:{} as {}\nSMTP target: {}:{} (from {})",
        acct.imap_host,
        acct.imap_port,
        acct.imap_user,
        acct.smtp_host,
        acct.smtp_port,
        acct.smtp_from
    )))
}

fn prompt_secret(prompt: &str) -> anyhow::Result<String> {
    use std::io::{BufRead, Write};
    eprint!("{prompt}");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let value = line.trim().to_string();
    if value.is_empty() {
        anyhow::bail!("empty secret");
    }
    Ok(value)
}

/// Store a secret in the macOS Keychain (no-op off macOS).
fn store_keychain(account: &str, secret: &str) -> anyhow::Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let status = std::process::Command::new("security")
        .args([
            "add-generic-password",
            "-U",
            "-s",
            "mail_mcp",
            "-a",
            account,
            "-w",
            secret,
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("failed to store Keychain entry for {account}");
    }
    Ok(())
}

fn keychain_cmd(account: &str) -> String {
    format!("security find-generic-password -s mail_mcp -a {account} -w")
}
