use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_router, ServiceExt, transport::stdio,
};
use serde::Deserialize;
use mailparse::MailHeaderMap;

// ---------- config ----------

#[derive(Clone, Debug)]
struct Config {
    imap_host: String,
    imap_port: u16,
    imap_user: String,
    imap_pass: String,
    smtp_host: String,
    smtp_port: u16,
    smtp_user: String,
    smtp_pass: String,
    smtp_from: String,
}

impl Config {
    fn from_env() -> anyhow::Result<Self> {
        dotenvy::dotenv().ok();
        let imap_user =
            std::env::var("IMAP_USER").map_err(|_| anyhow::anyhow!("IMAP_USER not set"))?;
        let imap_pass =
            std::env::var("IMAP_PASS").map_err(|_| anyhow::anyhow!("IMAP_PASS not set"))?;
        let smtp_user = std::env::var("SMTP_USER").unwrap_or_else(|_| imap_user.clone());
        let smtp_pass = std::env::var("SMTP_PASS").unwrap_or_else(|_| imap_pass.clone());
        Ok(Self {
            imap_host: std::env::var("IMAP_HOST").unwrap_or("imap.gmail.com".into()),
            imap_port: std::env::var("IMAP_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(993),
            imap_user: imap_user.clone(),
            imap_pass,
            smtp_host: std::env::var("SMTP_HOST").unwrap_or("smtp.gmail.com".into()),
            smtp_port: std::env::var("SMTP_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(587),
            smtp_from: std::env::var("SMTP_FROM").unwrap_or_else(|_| imap_user.clone()),
            smtp_user,
            smtp_pass,
        })
    }
}

fn err(msg: impl Into<String>) -> McpError {
    McpError::internal_error(msg.into(), None)
}

fn ok_text(text: String) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

// ---------- IMAP helpers (blocking, run in spawn_blocking) ----------

#[derive(serde::Serialize)]
struct EmailSummary {
    uid: u32,
    from: String,
    subject: String,
    date: String,
}

fn imap_session(cfg: &Config) -> anyhow::Result<imap::Session<native_tls::TlsStream<std::net::TcpStream>>> {
    let tls = native_tls::TlsConnector::builder().build()?;
    let client = imap::connect((cfg.imap_host.as_str(), cfg.imap_port), cfg.imap_host.as_str(), &tls)?;
    client
        .login(cfg.imap_user.as_str(), cfg.imap_pass.as_str())
        .map_err(|(e, _)| anyhow::anyhow!("IMAP login failed: {e}"))
}

fn parse_summary(uid: u32, raw: &[u8]) -> EmailSummary {
    match mailparse::parse_mail(raw) {
        Ok(parsed) => {
            let headers = &parsed.headers;
            let get = |k: &str| headers.get_first_value(k).unwrap_or_default();
            let subject = get("Subject");
            let from = get("From");
            let date = get("Date");
            EmailSummary { uid, from, subject, date }
        }
        Err(_) => EmailSummary {
            uid,
            from: String::new(),
            subject: "(unparseable)".into(),
            date: String::new(),
        },
    }
}

fn extract_body(raw: &[u8]) -> (String, String) {
    // returns (headers_text, body_text snippet)
    match mailparse::parse_mail(raw) {
        Ok(parsed) => {
            let headers = parsed
                .headers
                .iter()
                .map(|h| format!("{}: {}", h.get_key(), h.get_value()))
                .collect::<Vec<_>>()
                .join("\n");
            let body = parsed
                .get_body()
                .unwrap_or_else(|_| "(binary/non-text body)".into());
            let snippet: String = body.chars().take(8000).collect();
            (headers, snippet)
        }
        Err(e) => (String::new(), format!("(parse error: {e})")),
    }
}

fn do_list(cfg: Config, folder: String, limit: u32) -> anyhow::Result<String> {
    let mut s = imap_session(&cfg)?;
    s.select(&folder)?;
    let limit = limit.clamp(1, 50) as usize;
    let mut uids: Vec<u32> = s.search("ALL")?.into_iter().collect();
    uids.sort_unstable();
    let total = uids.len();
    let take: Vec<u32> = uids.into_iter().rev().take(limit).collect();
    if take.is_empty() {
        s.logout().ok();
        return Ok(serde_json::json!({"folder": folder, "total": total, "emails": []}).to_string());
    }
    let seq: Vec<String> = take.iter().map(|u| u.to_string()).collect();
    let msgs = s.uid_fetch(seq.join(","), "RFC822")?;
    let mut out = Vec::new();
    for m in msgs.iter() {
        let uid = m.uid.unwrap_or(0);
        if let Some(raw) = m.body() {
            out.push(parse_summary(uid, raw));
        }
    }
    // newest first (fetch may come unordered) — sort by uid desc
    out.sort_by(|a, b| b.uid.cmp(&a.uid));
    s.logout().ok();
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "folder": folder, "total": total, "emails": out
    }))?)
}

fn do_search(cfg: Config, folder: String, query: String, limit: u32) -> anyhow::Result<String> {
    let mut s = imap_session(&cfg)?;
    s.select(&folder)?;
    let limit = limit.clamp(1, 50) as usize;
    // escape quotes in query
    let q = query.replace('"', "'");
    let mut uids: Vec<u32> = s.search(format!("TEXT \"{q}\""))?.into_iter().collect();
    uids.sort_unstable();
    let total = uids.len();
    let take: Vec<u32> = uids.into_iter().rev().take(limit).collect();
    if take.is_empty() {
        s.logout().ok();
        return Ok(serde_json::json!({"folder": folder, "query": query, "total": total, "emails": []}).to_string());
    }
    let seq: Vec<String> = take.iter().map(|u| u.to_string()).collect();
    let msgs = s.uid_fetch(seq.join(","), "RFC822")?;
    let mut out = Vec::new();
    for m in msgs.iter() {
        let uid = m.uid.unwrap_or(0);
        if let Some(raw) = m.body() {
            out.push(parse_summary(uid, raw));
        }
    }
    out.sort_by(|a, b| b.uid.cmp(&a.uid));
    s.logout().ok();
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "folder": folder, "query": query, "total": total, "emails": out
    }))?)
}

fn do_read(cfg: Config, folder: String, uid: u32) -> anyhow::Result<String> {
    let mut s = imap_session(&cfg)?;
    s.select(&folder)?;
    let msgs = s.uid_fetch(uid.to_string(), "RFC822")?;
    let m = msgs
        .iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("message UID {uid} not found in {folder}"))?;
    let raw = m.body().ok_or_else(|| anyhow::anyhow!("empty body"))?.to_vec();
    let real_uid = m.uid.unwrap_or(uid);
    s.logout().ok();
    let (headers, body) = extract_body(&raw);
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "folder": folder, "uid": real_uid, "headers": headers, "body": body
    }))?)
}

fn do_fetch_thread_headers(cfg: Config, folder: String, uid: u32) -> anyhow::Result<(String, String, String)> {
    // returns (message_id, from, subject)
    let mut s = imap_session(&cfg)?;
    s.select(&folder)?;
    let msgs = s.uid_fetch(uid.to_string(), "RFC822.HEADER")?;
    let m = msgs
        .iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("message UID {uid} not found"))?;
    let raw = m.header().ok_or_else(|| anyhow::anyhow!("empty header"))?.to_vec();
    s.logout().ok();
    let (headers, _) = mailparse::parse_headers(&raw)?;
    let get = |k: &str| {
        headers
            .iter()
            .find(|h| h.get_key().eq_ignore_ascii_case(k))
            .map(|h| h.get_value())
            .unwrap_or_default()
    };
    Ok((get("Message-ID"), get("From"), get("Subject")))
}

// ---------- SMTP helper (async) ----------

async fn smtp_send(
    cfg: &Config,
    to: &str,
    subject: &str,
    body: &str,
    in_reply_to: Option<&str>,
) -> anyhow::Result<String> {
    use lettre::{
        message::header::ContentType, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    };

    let mut builder = Message::builder()
        .from(cfg.smtp_from.parse()?)
        .to(to.parse()?)
        .subject(subject);
    if let Some(id) = in_reply_to {
        builder = builder.in_reply_to(id.parse().map_err(|_| anyhow::anyhow!("bad Message-ID"))?);
    }
    let email = builder.header(ContentType::TEXT_PLAIN).body(body.to_string())?;

    let mailer: AsyncSmtpTransport<Tokio1Executor> = if cfg.smtp_port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.smtp_host)?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.smtp_host)?
    }
    .port(cfg.smtp_port)
    .credentials(lettre::transport::smtp::authentication::Credentials::new(
        cfg.smtp_user.clone(),
        cfg.smtp_pass.clone(),
    ))
    .build();

    mailer.send(email).await?;
    Ok(format!("sent to {to}"))
}

// ---------- MCP tool params ----------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ListParams {
    /// Mailbox folder, e.g. INBOX. Defaults to INBOX.
    #[serde(default)]
    folder: Option<String>,
    /// Max messages to return (1-50, default 10, newest first).
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchParams {
    /// Free-text query matched against the message (IMAP TEXT search).
    query: String,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadParams {
    /// IMAP UID of the message (from list/search output).
    uid: u32,
    #[serde(default)]
    folder: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SendParams {
    /// Recipient address, e.g. alice@example.com
    to: String,
    subject: String,
    /// Plain-text body
    body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReplyParams {
    /// UID of the message to reply to
    uid: u32,
    /// Plain-text reply body
    body: String,
    #[serde(default)]
    folder: Option<String>,
}

// ---------- MCP server ----------

#[derive(Clone)]
struct MailMcp {
    cfg: Config,
}

#[tool_router(server_handler)]
impl MailMcp {
    fn new(cfg: Config) -> Self {
        Self { cfg }
    }

    #[tool(description = "List newest emails in a folder (IMAP). Returns uid, from, subject, date.")]
    async fn list_emails(&self, Parameters(p): Parameters<ListParams>) -> Result<CallToolResult, McpError> {
        let cfg = self.cfg.clone();
        let folder = p.folder.unwrap_or_else(|| "INBOX".into());
        let limit = p.limit.unwrap_or(10);
        let out = tokio::task::spawn_blocking(move || do_list(cfg, folder, limit))
            .await
            .map_err(|e| err(e.to_string()))?
            .map_err(|e| err(e.to_string()))?;
        ok_text(out)
    }

    #[tool(description = "Search emails by free text (IMAP TEXT search). Returns uid, from, subject, date.")]
    async fn search_emails(&self, Parameters(p): Parameters<SearchParams>) -> Result<CallToolResult, McpError> {
        if p.query.trim().is_empty() {
            return Err(McpError::invalid_params("query must be non-empty", None));
        }
        let cfg = self.cfg.clone();
        let folder = p.folder.unwrap_or_else(|| "INBOX".into());
        let limit = p.limit.unwrap_or(10);
        let query = p.query.clone();
        let out = tokio::task::spawn_blocking(move || do_search(cfg, folder, query, limit))
            .await
            .map_err(|e| err(e.to_string()))?
            .map_err(|e| err(e.to_string()))?;
        ok_text(out)
    }

    #[tool(description = "Read one full email by UID (IMAP). Returns headers + body text.")]
    async fn read_email(&self, Parameters(p): Parameters<ReadParams>) -> Result<CallToolResult, McpError> {
        let cfg = self.cfg.clone();
        let folder = p.folder.unwrap_or_else(|| "INBOX".into());
        let out = tokio::task::spawn_blocking(move || do_read(cfg, folder, p.uid))
            .await
            .map_err(|e| err(e.to_string()))?
            .map_err(|e| err(e.to_string()))?;
        ok_text(out)
    }

    #[tool(description = "Send a new plain-text email via SMTP.")]
    async fn send_email(&self, Parameters(p): Parameters<SendParams>) -> Result<CallToolResult, McpError> {
        smtp_send(&self.cfg, &p.to, &p.subject, &p.body, None)
            .await
            .map_err(|e| err(e.to_string()))
            .and_then(ok_text)
    }

    #[tool(description = "Reply to an email by UID via SMTP, preserving threading (In-Reply-To).")]
    async fn reply_email(&self, Parameters(p): Parameters<ReplyParams>) -> Result<CallToolResult, McpError> {
        let cfg = self.cfg.clone();
        let folder = p.folder.clone().unwrap_or_else(|| "INBOX".into());
        let (msg_id, from, subject) =
            tokio::task::spawn_blocking(move || do_fetch_thread_headers(cfg, folder, p.uid))
                .await
                .map_err(|e| err(e.to_string()))?
                .map_err(|e| err(e.to_string()))?;
        if from.is_empty() {
            return Err(err("original From address not found"));
        }
        let reply_subject = if subject.to_lowercase().starts_with("re:") {
            subject
        } else {
            format!("Re: {subject}")
        };
        // extract bare address: "Name <a@b.c>" -> a@b.c
        let reply_to = mailparse::addrparse(&from)
            .map(|addrs| {
                addrs
                    .iter()
                    .filter_map(|a| match a {
                        mailparse::MailAddr::Single(s) => Some(s.addr.clone()),
                        mailparse::MailAddr::Group(g) => {
                            g.addrs.first().map(|s| s.addr.clone())
                        }
                    })
                    .next()
                    .unwrap_or(from.clone())
            })
            .unwrap_or(from.clone());
        let body = format!("{}\n\n--- On original message ({}) ---\n", p.body, msg_id);
        smtp_send(
            &self.cfg,
            &reply_to,
            &reply_subject,
            &body,
            if msg_id.is_empty() { None } else { Some(msg_id.as_str()) },
        )
        .await
        .map_err(|e| err(e.to_string()))
        .and_then(ok_text)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cfg = Config::from_env().unwrap_or_else(|e| {
        eprintln!("mail_mcp config: {e} (set IMAP_USER/IMAP_PASS env vars)");
        std::process::exit(1);
    });
    eprintln!("mail_mcp: IMAP {}:{} SMTP {}:{}", cfg.imap_host, cfg.imap_port, cfg.smtp_host, cfg.smtp_port);

    let service = MailMcp::new(cfg).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
