use mailparse::MailHeaderMap;
use rmcp::{
    handler::server::wrapper::Parameters, model::CallToolResult, schemars, tool, tool_router,
    transport::stdio, ErrorData as McpError, ServiceExt,
};
use serde::Deserialize;

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
    smtp_write_enabled: bool,
    max_attachment_bytes: usize,
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
        let smtp_write_enabled = std::env::var("MAIL_SMTP_WRITE_ENABLED")
            .ok()
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false);
        let max_attachment_bytes = std::env::var("MAIL_MAX_ATTACHMENT_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(25 * 1024 * 1024);
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
            smtp_write_enabled,
            max_attachment_bytes,
        })
    }
}

fn err(msg: impl Into<String>) -> McpError {
    McpError::internal_error(msg.into(), None)
}

fn ok_json(text: String) -> Result<CallToolResult, McpError> {
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| err(format!("tool produced invalid JSON: {e}")))?;
    Ok(CallToolResult::structured(value))
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({
        "error": message.into(),
        "retryable": false,
    }))
}

async fn finish_blocking_json<F>(task: F) -> Result<CallToolResult, McpError>
where
    F: FnOnce() -> anyhow::Result<String> + Send + 'static,
{
    let result = tokio::task::spawn_blocking(task)
        .await
        .map_err(|e| err(e.to_string()))?;
    match result {
        Ok(text) => ok_json(text),
        Err(e) => Ok(tool_error(e.to_string())),
    }
}

// ---------- IMAP helpers (blocking, run in spawn_blocking) ----------

#[derive(serde::Serialize, schemars::JsonSchema)]
struct EmailSummary {
    uid: u32,
    from: String,
    subject: String,
    date: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct EmailListResponse {
    folder: String,
    total: usize,
    emails: Vec<EmailSummary>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct SearchResponse {
    folder: String,
    query: String,
    total: usize,
    emails: Vec<EmailSummary>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct MailboxSummary {
    name: String,
    delimiter: Option<String>,
    selectable: bool,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct MailboxListResponse {
    mailboxes: Vec<MailboxSummary>,
}

fn imap_session(
    cfg: &Config,
) -> anyhow::Result<imap::Session<native_tls::TlsStream<std::net::TcpStream>>> {
    let tls = native_tls::TlsConnector::builder().build()?;
    let client = imap::connect(
        (cfg.imap_host.as_str(), cfg.imap_port),
        cfg.imap_host.as_str(),
        &tls,
    )?;
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
            EmailSummary {
                uid,
                from,
                subject,
                date,
            }
        }
        Err(_) => EmailSummary {
            uid,
            from: String::new(),
            subject: "(unparseable)".into(),
            date: String::new(),
        },
    }
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema, Default)]
struct HeaderSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    in_reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    references: Option<String>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct ParsedEmail {
    headers: HeaderSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_headers: Option<String>,
    body: String,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct ReadEmailResponse {
    folder: String,
    uid: u32,
    #[serde(flatten)]
    email: ParsedEmail,
}

fn optional_header(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn extract_email(raw: &[u8], include_raw_headers: bool) -> ParsedEmail {
    // Return common headers as fields and a readable body snippet. Raw decoded
    // headers are opt-in because mailing-list and authentication headers can
    // be very large and are rarely useful during normal triage.
    match mailparse::parse_mail(raw) {
        Ok(parsed) => {
            let get = |key: &str| {
                optional_header(parsed.headers.get_first_value(key).unwrap_or_default())
            };
            let headers = HeaderSummary {
                from: get("From"),
                to: get("To"),
                cc: get("Cc"),
                reply_to: get("Reply-To"),
                subject: get("Subject"),
                date: get("Date"),
                message_id: get("Message-ID"),
                in_reply_to: get("In-Reply-To"),
                references: get("References"),
            };
            let raw_headers = include_raw_headers.then(|| {
                parsed
                    .headers
                    .iter()
                    .map(|h| format!("{}: {}", h.get_key(), h.get_value()))
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            let mut plain = None;
            let mut html = None;
            collect_body_parts(&parsed, &mut plain, &mut html);
            let body = plain
                .or_else(|| html.map(|value| html_to_text(&value)))
                .unwrap_or_else(|| "(binary/non-text body)".into());
            let body = normalize_text(&body);
            let snippet: String = body.chars().take(8000).collect();
            ParsedEmail {
                headers,
                raw_headers,
                body: snippet,
            }
        }
        Err(e) => ParsedEmail {
            headers: HeaderSummary::default(),
            raw_headers: None,
            body: format!("(parse error: {e})"),
        },
    }
}

fn collect_body_parts(
    part: &mailparse::ParsedMail<'_>,
    plain: &mut Option<String>,
    html: &mut Option<String>,
) {
    if part.ctype.mimetype.starts_with("multipart/") {
        for sub in &part.subparts {
            collect_body_parts(sub, plain, html);
        }
        return;
    }

    let disposition = part.get_content_disposition();
    let has_filename =
        disposition.params.contains_key("filename") || part.ctype.params.contains_key("name");
    if matches!(
        disposition.disposition,
        mailparse::DispositionType::Attachment
    ) || has_filename
    {
        return;
    }

    if part.ctype.mimetype.eq_ignore_ascii_case("text/plain") && plain.is_none() {
        *plain = part.get_body().ok();
    } else if part.ctype.mimetype.eq_ignore_ascii_case("text/html") && html.is_none() {
        *html = part.get_body().ok();
    }
}

fn html_to_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    let mut tag = String::new();
    for ch in html.chars() {
        if ch == '<' {
            in_tag = true;
            tag.clear();
        } else if ch == '>' && in_tag {
            let tag_name = tag.trim_start_matches('/').trim().to_ascii_lowercase();
            if tag_name.starts_with("br")
                || tag_name.starts_with("p")
                || tag_name.starts_with("div")
                || tag_name.starts_with("li")
                || tag_name.starts_with("tr")
                || tag_name.starts_with("h1")
                || tag_name.starts_with("h2")
                || tag_name.starts_with("h3")
            {
                text.push('\n');
            }
            in_tag = false;
        } else if in_tag {
            tag.push(ch);
        } else {
            text.push(ch);
        }
    }

    decode_html_entities(&text)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn decode_html_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
}

// ---------- attachments ----------

#[derive(serde::Serialize, schemars::JsonSchema)]
struct AttachmentInfo {
    index: u32,
    filename: String,
    mime: String,
    size: usize,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct AttachmentListResponse {
    folder: String,
    uid: u32,
    attachments: Vec<AttachmentInfo>,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct DownloadAttachmentResponse {
    folder: String,
    uid: u32,
    filename: String,
    mime: String,
    size: usize,
    path: String,
}

#[derive(serde::Serialize, schemars::JsonSchema)]
struct SendResponse {
    status: String,
    message: String,
}

fn walk_attachments(part: &mailparse::ParsedMail<'_>, out: &mut Vec<AttachmentInfo>) {
    if part.ctype.mimetype.starts_with("multipart/") {
        for sub in &part.subparts {
            walk_attachments(sub, out);
        }
        return;
    }
    let disp = part.get_content_disposition();
    let filename = disp
        .params
        .get("filename")
        .or_else(|| part.ctype.params.get("name"))
        .cloned()
        .unwrap_or_default();
    let is_attachment =
        matches!(disp.disposition, mailparse::DispositionType::Attachment) || !filename.is_empty();
    if is_attachment {
        let size = part.get_body_raw().map(|b| b.len()).unwrap_or(0);
        out.push(AttachmentInfo {
            index: out.len() as u32,
            filename: if filename.is_empty() {
                format!("attachment-{}.bin", out.len())
            } else {
                filename
            },
            mime: part.ctype.mimetype.clone(),
            size,
        });
    }
    for sub in &part.subparts {
        walk_attachments(sub, out);
    }
}

fn collect_attachments(raw: &[u8]) -> anyhow::Result<Vec<AttachmentInfo>> {
    let parsed = mailparse::parse_mail(raw)?;
    let mut out = Vec::new();
    walk_attachments(&parsed, &mut out);
    Ok(out)
}

fn sanitize_filename(name: &str) -> String {
    let base = std::path::Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("attachment.bin");
    let clean: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let clean = clean.trim().to_string();
    if clean.is_empty() {
        "attachment.bin".into()
    } else {
        clean
    }
}

fn do_list_attachments(cfg: Config, folder: String, uid: u32) -> anyhow::Result<String> {
    let mut s = imap_session(&cfg)?;
    s.select(&folder)?;
    let msgs = s.uid_fetch(uid.to_string(), "RFC822")?;
    let m = msgs
        .iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("message UID {uid} not found in {folder}"))?;
    let raw = m
        .body()
        .ok_or_else(|| anyhow::anyhow!("empty body"))?
        .to_vec();
    s.logout().ok();
    let list = collect_attachments(&raw)?;
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "folder": folder, "uid": uid, "attachments": list
    }))?)
}

fn do_download_attachment(
    cfg: Config,
    folder: String,
    uid: u32,
    index: u32,
    out_dir: String,
) -> anyhow::Result<String> {
    let mut s = imap_session(&cfg)?;
    s.select(&folder)?;
    let msgs = s.uid_fetch(uid.to_string(), "RFC822")?;
    let m = msgs
        .iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("message UID {uid} not found in {folder}"))?;
    let raw = m
        .body()
        .ok_or_else(|| anyhow::anyhow!("empty body"))?
        .to_vec();
    s.logout().ok();

    let parsed = mailparse::parse_mail(&raw)?;
    let mut found: Vec<(AttachmentInfo, Vec<u8>)> = Vec::new();
    fn walk(
        part: &mailparse::ParsedMail<'_>,
        found: &mut Vec<(AttachmentInfo, Vec<u8>)>,
        max_attachment_bytes: usize,
    ) -> anyhow::Result<()> {
        if part.ctype.mimetype.starts_with("multipart/") {
            for sub in &part.subparts {
                walk(sub, found, max_attachment_bytes)?;
            }
            return Ok(());
        }
        let disp = part.get_content_disposition();
        let filename = disp
            .params
            .get("filename")
            .or_else(|| part.ctype.params.get("name"))
            .cloned()
            .unwrap_or_default();
        let is_attachment = matches!(disp.disposition, mailparse::DispositionType::Attachment)
            || !filename.is_empty();
        if is_attachment {
            let body = part.get_body_raw()?;
            if body.len() > max_attachment_bytes {
                return Err(anyhow::anyhow!(
                    "attachment index {} is {} bytes, exceeding the {} byte limit",
                    found.len(),
                    body.len(),
                    max_attachment_bytes
                ));
            }
            let bytes = body.to_vec();
            let info = AttachmentInfo {
                index: found.len() as u32,
                filename: if filename.is_empty() {
                    format!("attachment-{}.bin", found.len())
                } else {
                    filename
                },
                mime: part.ctype.mimetype.clone(),
                size: bytes.len(),
            };
            found.push((info, bytes));
        }
        for sub in &part.subparts {
            walk(sub, found, max_attachment_bytes)?;
        }
        Ok(())
    }
    walk(&parsed, &mut found, cfg.max_attachment_bytes)?;

    let (info, bytes) = found
        .into_iter()
        .find(|(i, _)| i.index == index)
        .ok_or_else(|| anyhow::anyhow!("attachment index {index} not found"))?;

    std::fs::create_dir_all(&out_dir)?;
    let safe = sanitize_filename(&info.filename);
    let path = std::path::Path::new(&out_dir).join(format!("uid{uid}-{index}-{safe}"));
    std::fs::write(&path, &bytes)?;
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "folder": folder, "uid": uid,
        "filename": info.filename, "mime": info.mime, "size": info.size,
        "path": path.to_string_lossy(),
    }))?)
}

fn do_list_mailboxes(cfg: Config) -> anyhow::Result<String> {
    let mut s = imap_session(&cfg)?;
    let names = s.list(None, Some("*"))?;
    let mut mailboxes = names
        .iter()
        .map(|mailbox| MailboxSummary {
            name: mailbox.name().to_string(),
            delimiter: mailbox.delimiter().map(str::to_string),
            selectable: !mailbox
                .attributes()
                .iter()
                .any(|attribute| matches!(attribute, imap::types::NameAttribute::NoSelect)),
        })
        .collect::<Vec<_>>();
    mailboxes.sort_by(|a, b| a.name.cmp(&b.name));
    s.logout().ok();
    Ok(serde_json::to_string_pretty(&MailboxListResponse {
        mailboxes,
    })?)
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
    let msgs = s.uid_fetch(seq.join(","), "RFC822.HEADER")?;
    let mut out = Vec::new();
    for m in msgs.iter() {
        let uid = m.uid.unwrap_or(0);
        if let Some(header) = m.header() {
            out.push(parse_summary(uid, header));
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
        return Ok(
            serde_json::json!({"folder": folder, "query": query, "total": total, "emails": []})
                .to_string(),
        );
    }
    let seq: Vec<String> = take.iter().map(|u| u.to_string()).collect();
    let msgs = s.uid_fetch(seq.join(","), "RFC822.HEADER")?;
    let mut out = Vec::new();
    for m in msgs.iter() {
        let uid = m.uid.unwrap_or(0);
        if let Some(header) = m.header() {
            out.push(parse_summary(uid, header));
        }
    }
    out.sort_by(|a, b| b.uid.cmp(&a.uid));
    s.logout().ok();
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "folder": folder, "query": query, "total": total, "emails": out
    }))?)
}

fn do_read(
    cfg: Config,
    folder: String,
    uid: u32,
    include_raw_headers: bool,
) -> anyhow::Result<String> {
    let mut s = imap_session(&cfg)?;
    s.select(&folder)?;
    let msgs = s.uid_fetch(uid.to_string(), "RFC822")?;
    let m = msgs
        .iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("message UID {uid} not found in {folder}"))?;
    let raw = m
        .body()
        .ok_or_else(|| anyhow::anyhow!("empty body"))?
        .to_vec();
    let real_uid = m.uid.unwrap_or(uid);
    s.logout().ok();
    let email = extract_email(&raw, include_raw_headers);
    Ok(serde_json::to_string_pretty(&ReadEmailResponse {
        folder,
        uid: real_uid,
        email,
    })?)
}

fn do_fetch_thread_headers(
    cfg: Config,
    folder: String,
    uid: u32,
) -> anyhow::Result<(String, String, String)> {
    // returns (message_id, from, subject)
    let mut s = imap_session(&cfg)?;
    s.select(&folder)?;
    let msgs = s.uid_fetch(uid.to_string(), "RFC822.HEADER")?;
    let m = msgs
        .iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("message UID {uid} not found"))?;
    let raw = m
        .header()
        .ok_or_else(|| anyhow::anyhow!("empty header"))?
        .to_vec();
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
    attachments: &[String],
    max_attachment_bytes: usize,
) -> anyhow::Result<String> {
    use lettre::{
        message::{
            header::{ContentDisposition, ContentType},
            MultiPart, SinglePart,
        },
        AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    };

    let mut builder = Message::builder()
        .from(cfg.smtp_from.parse()?)
        .to(to.parse()?)
        .subject(subject);
    if let Some(id) = in_reply_to {
        builder = builder.in_reply_to(id.parse().map_err(|_| anyhow::anyhow!("bad Message-ID"))?);
    }
    let email = if attachments.is_empty() {
        builder
            .header(ContentType::TEXT_PLAIN)
            .body(body.to_string())?
    } else {
        let mut mixed = MultiPart::mixed().singlepart(SinglePart::plain(body.to_string()));
        for path in attachments {
            let file_size = tokio::fs::metadata(path)
                .await
                .map_err(|e| anyhow::anyhow!("cannot inspect attachment {path}: {e}"))?
                .len();
            if file_size > max_attachment_bytes as u64 {
                return Err(anyhow::anyhow!(
                    "attachment {path} is {file_size} bytes, exceeding the {} byte limit",
                    max_attachment_bytes
                ));
            }
            let bytes = tokio::fs::read(path)
                .await
                .map_err(|e| anyhow::anyhow!("cannot read attachment {path}: {e}"))?;
            if bytes.len() > max_attachment_bytes {
                return Err(anyhow::anyhow!(
                    "attachment {path} exceeds the {} byte limit",
                    max_attachment_bytes
                ));
            }
            let filename = std::path::Path::new(path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("attachment.bin")
                .to_string();
            let mime_str = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            let ctype: ContentType = mime_str.parse().unwrap_or(ContentType::TEXT_PLAIN);
            mixed = mixed.singlepart(
                SinglePart::builder()
                    .header(ctype)
                    .header(ContentDisposition::attachment(&filename))
                    .body(bytes),
            );
        }
        builder.multipart(mixed)?
    };

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
#[serde(deny_unknown_fields)]
struct MailboxParams {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListParams {
    /// Exact IMAP mailbox name from list_mailboxes output. Defaults to INBOX.
    #[serde(default)]
    folder: Option<String>,
    /// Max messages to return (1-50, default 10, newest first).
    #[serde(default)]
    #[schemars(range(min = 1, max = 50))]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchParams {
    /// Literal free-text query matched by IMAP TEXT. This is not Gmail syntax; do not use from:me or subject:foo.
    #[schemars(length(min = 1))]
    query: String,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 50))]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadParams {
    /// IMAP UID of the message (from list/search output).
    uid: u32,
    #[serde(default)]
    folder: Option<String>,
    /// Include all decoded headers, including verbose routing/authentication headers.
    #[serde(default)]
    include_raw_headers: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendParams {
    /// Recipient address, e.g. alice@example.com
    to: String,
    subject: String,
    /// Plain-text body
    body: String,
    /// Optional attachments: list of local file paths to attach
    #[serde(default)]
    attachments: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReplyParams {
    /// UID of the message to reply to
    uid: u32,
    /// Plain-text reply body
    body: String,
    #[serde(default)]
    folder: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListAttachmentsParams {
    /// IMAP UID of the message (from list/search output).
    uid: u32,
    #[serde(default)]
    folder: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct DownloadAttachmentParams {
    /// IMAP UID of the message.
    uid: u32,
    /// Attachment index from list_attachments output.
    index: u32,
    #[serde(default)]
    folder: Option<String>,
    /// Directory to save the file into (created if missing, default "attachments").
    #[serde(default)]
    out_dir: Option<String>,
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

    #[tool(
        description = "List selectable and non-selectable IMAP mailboxes. Use the exact returned name as the folder argument for other tools.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<MailboxListResponse>(),
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn list_mailboxes(
        &self,
        Parameters(_p): Parameters<MailboxParams>,
    ) -> Result<CallToolResult, McpError> {
        finish_blocking_json({
            let cfg = self.cfg.clone();
            move || do_list_mailboxes(cfg)
        })
        .await
    }

    #[tool(
        description = "List newest emails in a folder (IMAP). The folder must be an exact name from list_mailboxes; defaults to INBOX. Returns uid, from, subject, date.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<EmailListResponse>(),
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn list_emails(
        &self,
        Parameters(p): Parameters<ListParams>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = self.cfg.clone();
        let folder = p.folder.unwrap_or_else(|| "INBOX".into());
        let limit = p.limit.unwrap_or(10);
        finish_blocking_json(move || do_list(cfg, folder, limit)).await
    }

    #[tool(
        description = "Search emails using literal IMAP TEXT in an exact folder name from list_mailboxes. This is not Gmail query syntax (do not use from:me or subject:foo). Returns uid, from, subject, date.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<SearchResponse>(),
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn search_emails(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        if p.query.trim().is_empty() {
            return Err(McpError::invalid_params("query must be non-empty", None));
        }
        let cfg = self.cfg.clone();
        let folder = p.folder.unwrap_or_else(|| "INBOX".into());
        let limit = p.limit.unwrap_or(10);
        let query = p.query.clone();
        finish_blocking_json(move || do_search(cfg, folder, query, limit)).await
    }

    #[tool(
        description = "Read one email by UID (IMAP) from an exact folder name from list_mailboxes. Returns compact structured headers and readable body text. Set include_raw_headers=true to include all decoded headers.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<ReadEmailResponse>(),
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn read_email(
        &self,
        Parameters(p): Parameters<ReadParams>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = self.cfg.clone();
        let folder = p.folder.unwrap_or_else(|| "INBOX".into());
        let include_raw_headers = p.include_raw_headers;
        finish_blocking_json(move || do_read(cfg, folder, p.uid, include_raw_headers)).await
    }

    #[tool(
        description = "Send a new plain-text email via SMTP. Requires MAIL_SMTP_WRITE_ENABLED=true. Optional attachments are local file paths.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<SendResponse>(),
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = true)
    )]
    async fn send_email(
        &self,
        Parameters(p): Parameters<SendParams>,
    ) -> Result<CallToolResult, McpError> {
        if !self.cfg.smtp_write_enabled {
            return Ok(tool_error(
                "outbound mail is disabled; set MAIL_SMTP_WRITE_ENABLED=true to enable sending",
            ));
        }
        let atts = p.attachments.unwrap_or_default();
        match smtp_send(
            &self.cfg,
            &p.to,
            &p.subject,
            &p.body,
            None,
            &atts,
            self.cfg.max_attachment_bytes,
        )
        .await
        {
            Ok(message) => ok_json(
                serde_json::to_string(&SendResponse {
                    status: "sent".into(),
                    message,
                })
                .map_err(|e| err(e.to_string()))?,
            ),
            Err(e) => Ok(tool_error(e.to_string())),
        }
    }

    #[tool(
        description = "Reply to an email by UID via SMTP, preserving threading (In-Reply-To). Requires MAIL_SMTP_WRITE_ENABLED=true.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<SendResponse>(),
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = true)
    )]
    async fn reply_email(
        &self,
        Parameters(p): Parameters<ReplyParams>,
    ) -> Result<CallToolResult, McpError> {
        if !self.cfg.smtp_write_enabled {
            return Ok(tool_error(
                "outbound mail is disabled; set MAIL_SMTP_WRITE_ENABLED=true to enable replies",
            ));
        }
        let cfg = self.cfg.clone();
        let folder = p.folder.clone().unwrap_or_else(|| "INBOX".into());
        let (msg_id, from, subject) =
            match tokio::task::spawn_blocking(move || do_fetch_thread_headers(cfg, folder, p.uid))
                .await
                .map_err(|e| err(e.to_string()))?
            {
                Ok(headers) => headers,
                Err(e) => return Ok(tool_error(e.to_string())),
            };
        if from.is_empty() {
            return Ok(tool_error("original From address not found"));
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
                        mailparse::MailAddr::Group(g) => g.addrs.first().map(|s| s.addr.clone()),
                    })
                    .next()
                    .unwrap_or(from.clone())
            })
            .unwrap_or(from.clone());
        let body = format!("{}\n\n--- On original message ({}) ---\n", p.body, msg_id);
        match smtp_send(
            &self.cfg,
            &reply_to,
            &reply_subject,
            &body,
            if msg_id.is_empty() {
                None
            } else {
                Some(msg_id.as_str())
            },
            &[],
            self.cfg.max_attachment_bytes,
        )
        .await
        {
            Ok(message) => ok_json(
                serde_json::to_string(&SendResponse {
                    status: "sent".into(),
                    message,
                })
                .map_err(|e| err(e.to_string()))?,
            ),
            Err(e) => Ok(tool_error(e.to_string())),
        }
    }

    #[tool(
        description = "List attachments of an email by UID (IMAP). The folder must be an exact name from list_mailboxes. Returns index, filename, mime, size.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<AttachmentListResponse>(),
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn list_attachments(
        &self,
        Parameters(p): Parameters<ListAttachmentsParams>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = self.cfg.clone();
        let folder = p.folder.unwrap_or_else(|| "INBOX".into());
        finish_blocking_json(move || do_list_attachments(cfg, folder, p.uid)).await
    }

    #[tool(
        description = "Download one attachment by UID + index to disk. The folder must be an exact name from list_mailboxes. Returns the saved path.",
        output_schema = rmcp::handler::server::tool::schema_for_output::<DownloadAttachmentResponse>(),
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = true)
    )]
    async fn download_attachment(
        &self,
        Parameters(p): Parameters<DownloadAttachmentParams>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = self.cfg.clone();
        let folder = p.folder.unwrap_or_else(|| "INBOX".into());
        let out_dir = p.out_dir.unwrap_or_else(|| "attachments".into());
        finish_blocking_json(move || do_download_attachment(cfg, folder, p.uid, p.index, out_dir))
            .await
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
    eprintln!(
        "mail_mcp: IMAP {}:{} SMTP {}:{} (outbound mail: {}, attachment limit: {} bytes)",
        cfg.imap_host,
        cfg.imap_port,
        cfg.smtp_host,
        cfg.smtp_port,
        if cfg.smtp_write_enabled {
            "enabled"
        } else {
            "disabled"
        },
        cfg.max_attachment_bytes
    );

    let service = MailMcp::new(cfg).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_body_becomes_readable_text() {
        assert_eq!(
            html_to_text("<h1>Hello</h1><p>One &amp; two<br>three</p>"),
            "Hello\nOne & two\nthree"
        );
    }

    #[test]
    fn read_body_prefers_plain_text_part() {
        let raw = b"Content-Type: multipart/alternative; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nplain body\r\n--b\r\nContent-Type: text/html\r\n\r\n<p>html body</p>\r\n--b--\r\n";
        let email = extract_email(raw, false);
        assert_eq!(email.body, "plain body");
    }

    #[test]
    fn read_email_returns_compact_headers_by_default() {
        let raw = b"From: Alice <alice@example.com>\r\nTo: Bob <bob@example.com>\r\nSubject: Hello\r\nDate: Tue, 1 Sep 2026 12:00:00 +0000\r\nDKIM-Signature: very-long-auth-header\r\n\r\nHello\r\nworld";
        let email = extract_email(raw, false);
        assert_eq!(email.headers.subject.as_deref(), Some("Hello"));
        assert_eq!(
            email.headers.from.as_deref(),
            Some("Alice <alice@example.com>")
        );
        assert_eq!(email.body, "Hello\nworld");
        assert!(email.raw_headers.is_none());
    }

    #[test]
    fn read_email_can_include_raw_headers_on_request() {
        let raw = b"Subject: Hello\r\nDKIM-Signature: auth-header\r\n\r\nbody";
        let email = extract_email(raw, true);
        assert!(email
            .raw_headers
            .as_deref()
            .is_some_and(|headers| headers.contains("DKIM-Signature: auth-header")));
    }

    #[test]
    fn tool_params_reject_unknown_fields() {
        let parsed: Result<ReadParams, _> = serde_json::from_value(serde_json::json!({
            "uid": 873,
            "unexpected": true
        }));
        assert!(parsed.is_err());
    }

    #[test]
    fn json_results_include_structured_content() {
        let result = ok_json(r#"{"folder":"INBOX","total":0,"emails":[]}"#.into())
            .expect("valid JSON should become a tool result");
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({"folder":"INBOX","total":0,"emails":[]}))
        );
    }

    #[test]
    fn filenames_cannot_escape_output_directory() {
        assert_eq!(sanitize_filename("../../secret.txt"), "secret.txt");
        assert_eq!(sanitize_filename("invoice:2026.pdf"), "invoice_2026.pdf");
    }
}
