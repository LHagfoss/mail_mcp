use mailparse::MailHeaderMap;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::Deserialize;
use serde_json::Value;

// ---------- config ----------

mod cli;
mod config;

use cli::Cli;
use config::{ResolvedAccount as Config, Settings};

/// Convert the outcome of a tool into an MCP result.
///
/// Operational failures (IMAP/SMTP errors, unknown accounts, missing messages)
/// are returned as `isError: true` tool results so the model can read the
/// message and recover. Protocol errors are reserved for invalid parameters.
fn tool_result(result: anyhow::Result<String>) -> Result<CallToolResult, McpError> {
    Ok(match result {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("error: {e:#}"))]),
    })
}

/// Run blocking IMAP work off the async runtime and flatten join errors.
async fn blocking<F>(f: F) -> anyhow::Result<String>
where
    F: FnOnce() -> anyhow::Result<String> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow::anyhow!("worker task failed: {e}"))?
}

// ---------- IMAP helpers (blocking, run in spawn_blocking) ----------

#[derive(serde::Serialize)]
struct EmailSummary {
    uid: u32,
    from: String,
    subject: String,
    date: String,
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

fn append_sent_copy(cfg: &Config, raw: &[u8]) -> anyhow::Result<()> {
    let mut session = imap_session(cfg)?;
    let result = session.append(&cfg.sent_folder, raw);
    session.logout().ok();
    result.map_err(Into::into)
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

fn extract_body(raw: &[u8]) -> (String, String) {
    // Returns (headers_text, body_text snippet), preferring text/plain and
    // falling back to a readable approximation of text/html.
    match mailparse::parse_mail(raw) {
        Ok(parsed) => {
            let headers = parsed
                .headers
                .iter()
                .map(|h| format!("{}: {}", h.get_key(), h.get_value()))
                .collect::<Vec<_>>()
                .join("\n");
            let mut plain = None;
            let mut html = None;
            collect_body_parts(&parsed, &mut plain, &mut html);
            let body = plain
                .or_else(|| html.map(|value| html_to_text(&value)))
                .unwrap_or_else(|| "(binary/non-text body)".into());
            let snippet: String = body.chars().take(8000).collect();
            (headers, snippet)
        }
        Err(e) => (String::new(), format!("(parse error: {e})")),
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

#[derive(serde::Serialize)]
struct AttachmentInfo {
    index: u32,
    filename: String,
    mime: String,
    size: usize,
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

/// Newest-first page of `uids`: skip `offset` newest messages, then take
/// `limit` (clamped to 1-50). Returns the page and the offset of the next page.
fn page_uids(mut uids: Vec<u32>, offset: u32, limit: u32) -> (Vec<u32>, Option<usize>) {
    let limit = limit.clamp(1, 50) as usize;
    let offset = offset as usize;
    uids.sort_unstable_by(|a, b| b.cmp(a));
    let total = uids.len();
    let page: Vec<u32> = uids.into_iter().skip(offset).take(limit).collect();
    let end = offset + page.len();
    (page, (end < total).then_some(end))
}

fn fetch_summaries(
    s: &mut imap::Session<native_tls::TlsStream<std::net::TcpStream>>,
    uids: &[u32],
) -> anyhow::Result<Vec<EmailSummary>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let seq: Vec<String> = uids.iter().map(|u| u.to_string()).collect();
    let msgs = s.uid_fetch(seq.join(","), "RFC822.HEADER")?;
    let mut out: Vec<EmailSummary> = msgs
        .iter()
        .filter_map(|m| m.header().map(|h| parse_summary(m.uid.unwrap_or(0), h)))
        .collect();
    // newest first (fetch may come unordered) — sort by uid desc
    out.sort_by_key(|e| std::cmp::Reverse(e.uid));
    Ok(out)
}

fn do_list(cfg: Config, folder: String, limit: u32, offset: u32) -> anyhow::Result<String> {
    let mut s = imap_session(&cfg)?;
    s.select(&folder)?;
    let uids: Vec<u32> = s.uid_search("ALL")?.into_iter().collect();
    let total = uids.len();
    let (page, next_offset) = page_uids(uids, offset, limit);
    let out = fetch_summaries(&mut s, &page);
    s.logout().ok();
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "folder": folder, "total": total, "offset": offset,
        "next_offset": next_offset, "emails": out?
    }))?)
}

fn do_search(
    cfg: Config,
    folder: String,
    query: String,
    limit: u32,
    offset: u32,
) -> anyhow::Result<String> {
    let mut s = imap_session(&cfg)?;
    s.select(&folder)?;
    // escape quotes in query
    let q = query.replace('"', "'");
    // Search across headers (FROM, TO, SUBJECT) as well as BODY / TEXT for broad compatibility across IMAP servers.
    let imap_query = format!(
        "OR (OR FROM \"{q}\" TO \"{q}\") (OR SUBJECT \"{q}\" (OR BODY \"{q}\" TEXT \"{q}\"))"
    );
    let uids: Vec<u32> = s.uid_search(&imap_query)?.into_iter().collect();
    let total = uids.len();
    let (page, next_offset) = page_uids(uids, offset, limit);
    let out = fetch_summaries(&mut s, &page);
    s.logout().ok();
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "folder": folder, "query": query, "total": total, "offset": offset,
        "next_offset": next_offset, "emails": out?
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
    let raw = m
        .body()
        .ok_or_else(|| anyhow::anyhow!("empty body"))?
        .to_vec();
    let real_uid = m.uid.unwrap_or(uid);
    s.logout().ok();
    let (headers, body) = extract_body(&raw);
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "folder": folder, "uid": real_uid, "headers": headers, "body": body
    }))?)
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

fn add_cc_recipients(
    mut builder: lettre::message::MessageBuilder,
    cc: &[String],
) -> anyhow::Result<lettre::message::MessageBuilder> {
    for recipient in cc {
        let mailbox = recipient
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid CC recipient {recipient:?}: {e}"))?;
        builder = builder.cc(mailbox);
    }
    Ok(builder)
}

async fn smtp_send(
    cfg: &Config,
    to: &str,
    subject: &str,
    body: &str,
    in_reply_to: Option<&str>,
    attachments: &[String],
    cc: &[String],
) -> anyhow::Result<String> {
    use lettre::{
        message::{
            header::{ContentDisposition, ContentType},
            MultiPart, SinglePart,
        },
        AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    };
    let max_attachment_bytes = cfg.max_attachment_bytes;

    let mut builder = Message::builder()
        .from(cfg.smtp_from.parse()?)
        .to(to.parse()?)
        .subject(subject);
    builder = add_cc_recipients(builder, cc)?;
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

    let raw = email.formatted();
    mailer.send(email).await?;

    // SMTP delivery and IMAP mailbox storage are separate operations. Save a
    // copy only after SMTP accepts the message, and report a sync warning
    // without turning a successfully sent message into a retryable failure.
    let append_cfg = cfg.clone();
    let append_result = tokio::task::spawn_blocking(move || append_sent_copy(&append_cfg, &raw))
        .await
        .map_err(|e| anyhow::anyhow!("Sent-folder sync task failed: {e}"))?;
    match append_result {
        Ok(()) => Ok(format!("sent to {to} and saved to {}", cfg.sent_folder)),
        Err(e) => Ok(format!(
            "sent to {to}; warning: could not save a copy to {}: {e}",
            cfg.sent_folder
        )),
    }
}

// ---------- MCP tool params ----------

/// Compact generated input schemas.
///
/// schemars renders `Option<T>` as `{"type": ["T", "null"], "default": null}`.
/// Some MCP clients only handle a scalar `type`, and expanding the union into
/// `anyOf` duplicated every description per branch. Optional fields are simply
/// omitted from `required`, so the nullable marker and null default carry no
/// information for callers; `null` is still accepted at runtime by serde.
/// Integer `format` annotations (e.g. `uint32`) are non-standard noise and are
/// dropped too (`minimum` is kept).
fn portable_schema(schema: &mut schemars::Schema) {
    schemars::transform::transform_subschemas(&mut portable_schema, schema);

    let Some(object) = schema.as_object_mut() else {
        return;
    };
    object.remove("$schema");
    if object.get("default").is_some_and(Value::is_null) {
        object.remove("default");
    }
    if let Some(types) = object.get("type").and_then(Value::as_array).cloned() {
        let non_null: Vec<Value> = types.into_iter().filter(|t| t != "null").collect();
        let ty = match non_null.len() {
            1 => non_null.into_iter().next().unwrap_or(Value::Null),
            _ => Value::Array(non_null),
        };
        object.insert("type".into(), ty);
    }
    if object.get("type").is_some_and(|t| t == "integer") {
        object.remove("format");
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(transform = portable_schema)]
struct ListParams {
    /// IMAP folder (default INBOX).
    #[serde(default)]
    folder: Option<String>,
    /// Max messages, 1-50 (default 10). Newest first.
    #[serde(default)]
    limit: Option<u32>,
    /// Skip this many newest messages; use next_offset from a previous call to page.
    #[serde(default)]
    offset: Option<u32>,
    /// Account name (default: configured default account).
    #[serde(default)]
    account: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(transform = portable_schema)]
struct SearchParams {
    /// Text matched (substring, case-insensitive) against From, To, Subject and body.
    query: String,
    /// IMAP folder (default INBOX).
    #[serde(default)]
    folder: Option<String>,
    /// Max messages, 1-50 (default 10). Newest first.
    #[serde(default)]
    limit: Option<u32>,
    /// Skip this many newest matches; use next_offset from a previous call to page.
    #[serde(default)]
    offset: Option<u32>,
    /// Account name (default: configured default account).
    #[serde(default)]
    account: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(transform = portable_schema)]
struct ReadParams {
    /// Message UID from list_emails/search_emails.
    uid: u32,
    /// IMAP folder the UID belongs to (default INBOX).
    #[serde(default)]
    folder: Option<String>,
    /// Account name (default: configured default account).
    #[serde(default)]
    account: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(transform = portable_schema)]
struct SendParams {
    /// Recipient address, e.g. alice@example.com or "Alice <alice@example.com>".
    to: String,
    /// CC recipients, one per entry, e.g. bob@example.com or "Bob <bob@example.com>".
    #[serde(default)]
    cc: Option<Vec<String>>,
    /// Subject line.
    subject: String,
    /// Plain-text body.
    body: String,
    /// Local file paths to attach.
    #[serde(default)]
    attachments: Option<Vec<String>>,
    /// Account to send from (default: configured default account).
    #[serde(default)]
    account: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(transform = portable_schema)]
struct ReplyParams {
    /// UID of the message to reply to.
    uid: u32,
    /// Plain-text reply body.
    body: String,
    /// CC recipients, one per entry, e.g. bob@example.com or "Bob <bob@example.com>".
    #[serde(default)]
    cc: Option<Vec<String>>,
    /// IMAP folder the UID belongs to (default INBOX).
    #[serde(default)]
    folder: Option<String>,
    /// Account to reply from (default: configured default account).
    #[serde(default)]
    account: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(transform = portable_schema)]
struct ListAttachmentsParams {
    /// Message UID from list_emails/search_emails.
    uid: u32,
    /// IMAP folder the UID belongs to (default INBOX).
    #[serde(default)]
    folder: Option<String>,
    /// Account name (default: configured default account).
    #[serde(default)]
    account: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[schemars(transform = portable_schema)]
struct DownloadAttachmentParams {
    /// Message UID.
    uid: u32,
    /// Attachment index from list_attachments.
    index: u32,
    /// IMAP folder the UID belongs to (default INBOX).
    #[serde(default)]
    folder: Option<String>,
    /// Directory to save into, created if missing (default "attachments").
    #[serde(default)]
    out_dir: Option<String>,
    /// Account name (default: configured default account).
    #[serde(default)]
    account: Option<String>,
}

// ---------- MCP server ----------

const INSTRUCTIONS: &str = "Email over IMAP/SMTP. Find messages with list_emails or \
search_emails (newest first; page with offset/next_offset), then use the returned uid with \
read_email, list_attachments or download_attachment. UIDs are per folder: pass the same folder. \
send_email and reply_email deliver real mail and cannot be undone; confirm recipients and \
content with the user first. Use account to pick a non-default mailbox.";

#[derive(Clone)]
struct MailMcp {
    settings: Settings,
}

#[tool_router]
impl MailMcp {
    fn new(settings: Settings) -> Self {
        Self { settings }
    }

    /// Resolve the account for a tool call: the per-call `account` argument when
    /// given, otherwise the configured default. Results are cached per process.
    fn account(&self, name: Option<&str>) -> anyhow::Result<Config> {
        config::resolve_cached(&self.settings, name).map(|a| a.as_ref().clone())
    }

    #[tool(
        description = "List newest emails in a folder. Returns total, next_offset and emails [{uid, from, subject, date}].",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn list_emails(
        &self,
        Parameters(p): Parameters<ListParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(
            async {
                let cfg = self.account(p.account.as_deref())?;
                let folder = p.folder.unwrap_or_else(|| "INBOX".into());
                let (limit, offset) = (p.limit.unwrap_or(10), p.offset.unwrap_or(0));
                blocking(move || do_list(cfg, folder, limit, offset)).await
            }
            .await,
        )
    }

    #[tool(
        description = "Search emails by text in From, To, Subject and body. Returns total, next_offset and emails [{uid, from, subject, date}], newest first.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn search_emails(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        if p.query.trim().is_empty() {
            return Err(McpError::invalid_params("query must be non-empty", None));
        }
        tool_result(
            async {
                let cfg = self.account(p.account.as_deref())?;
                let folder = p.folder.unwrap_or_else(|| "INBOX".into());
                let (limit, offset) = (p.limit.unwrap_or(10), p.offset.unwrap_or(0));
                let query = p.query;
                blocking(move || do_search(cfg, folder, query, limit, offset)).await
            }
            .await,
        )
    }

    #[tool(
        description = "Read one email by UID: all headers plus body text (plain text preferred, HTML converted; first 8000 chars).",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn read_email(
        &self,
        Parameters(p): Parameters<ReadParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(
            async {
                let cfg = self.account(p.account.as_deref())?;
                let folder = p.folder.unwrap_or_else(|| "INBOX".into());
                blocking(move || do_read(cfg, folder, p.uid)).await
            }
            .await,
        )
    }

    #[tool(
        description = "Send a new plain-text email via SMTP (optional cc and attachments) and save a copy to Sent. Irreversible. Requires MAIL_SMTP_WRITE_ENABLED=true.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn send_email(
        &self,
        Parameters(p): Parameters<SendParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(
            async {
                let cfg = self.account(p.account.as_deref())?;
                if !cfg.smtp_write_enabled {
                    anyhow::bail!(
                        "outbound mail is disabled; set MAIL_SMTP_WRITE_ENABLED=true to enable sending"
                    );
                }
                let atts = p.attachments.unwrap_or_default();
                smtp_send(
                    &cfg,
                    &p.to,
                    &p.subject,
                    &p.body,
                    None,
                    &atts,
                    p.cc.as_deref().unwrap_or_default(),
                )
                .await
            }
            .await,
        )
    }

    #[tool(
        description = "Reply to the sender of an email by UID (Re: subject, In-Reply-To threading, optional cc) and save a copy to Sent. Irreversible. Requires MAIL_SMTP_WRITE_ENABLED=true.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn reply_email(
        &self,
        Parameters(p): Parameters<ReplyParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(self.do_reply(p).await)
    }

    async fn do_reply(&self, p: ReplyParams) -> anyhow::Result<String> {
        let account = self.account(p.account.as_deref())?;
        if !account.smtp_write_enabled {
            anyhow::bail!(
                "outbound mail is disabled; set MAIL_SMTP_WRITE_ENABLED=true to enable replies"
            );
        }
        let cfg = account.clone();
        let folder = p.folder.clone().unwrap_or_else(|| "INBOX".into());
        let (msg_id, from, subject) =
            tokio::task::spawn_blocking(move || do_fetch_thread_headers(cfg, folder, p.uid))
                .await
                .map_err(|e| anyhow::anyhow!("worker task failed: {e}"))??;
        if from.is_empty() {
            anyhow::bail!("original From address not found");
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
        smtp_send(
            &account,
            &reply_to,
            &reply_subject,
            &body,
            if msg_id.is_empty() {
                None
            } else {
                Some(msg_id.as_str())
            },
            &[],
            p.cc.as_deref().unwrap_or_default(),
        )
        .await
    }

    #[tool(
        description = "List attachments of an email by UID. Returns [{index, filename, mime, size}].",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn list_attachments(
        &self,
        Parameters(p): Parameters<ListAttachmentsParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(
            async {
                let cfg = self.account(p.account.as_deref())?;
                let folder = p.folder.unwrap_or_else(|| "INBOX".into());
                blocking(move || do_list_attachments(cfg, folder, p.uid)).await
            }
            .await,
        )
    }

    #[tool(
        description = "Save one attachment (UID + index from list_attachments) to a local directory. Returns the saved path.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn download_attachment(
        &self,
        Parameters(p): Parameters<DownloadAttachmentParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(
            async {
                let cfg = self.account(p.account.as_deref())?;
                let folder = p.folder.unwrap_or_else(|| "INBOX".into());
                let out_dir = p.out_dir.unwrap_or_else(|| "attachments".into());
                blocking(move || do_download_attachment(cfg, folder, p.uid, p.index, out_dir)).await
            }
            .await,
        )
    }
}

#[tool_handler]
impl ServerHandler for MailMcp {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new(
            "mail",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(INSTRUCTIONS.to_string())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    // Subcommands (account management) run and exit; no subcommand => serve.
    let cli = <Cli as clap::Parser>::parse();
    if let Some(message) = cli.dispatch().unwrap_or_else(|e| {
        eprintln!("mail_mcp: {e}");
        std::process::exit(1);
    }) {
        println!("{message}");
        return Ok(());
    }

    let settings = config::load_settings().unwrap_or_else(|e| {
        eprintln!("mail_mcp config: {e}");
        std::process::exit(1);
    });
    let cfg = config::resolve_cached(&settings, None).unwrap_or_else(|e| {
        eprintln!("mail_mcp config: {e}");
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

    let service = MailMcp::new(settings).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cc_recipients_appear_in_headers_and_delivery_envelope() {
        let builder = lettre::Message::builder()
            .from("sender@example.com".parse().unwrap())
            .to("alice@example.com".parse().unwrap());
        let email = add_cc_recipients(
            builder,
            &["Bob <bob@example.com>".into(), "carol@example.com".into()],
        )
        .unwrap()
        .body("hello".to_string())
        .unwrap();
        let recipients: Vec<_> = email
            .envelope()
            .to()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            recipients,
            ["alice@example.com", "bob@example.com", "carol@example.com"]
        );
        let raw = email.formatted();
        let parsed = mailparse::parse_mail(&raw).unwrap();
        assert_eq!(
            parsed.headers.get_first_value("Cc").unwrap(),
            "Bob <bob@example.com>, carol@example.com"
        );
    }

    #[test]
    fn invalid_cc_recipient_is_rejected() {
        assert!(add_cc_recipients(lettre::Message::builder(), &["invalid".into()]).is_err());
    }

    #[test]
    fn cc_is_optional_for_existing_send_and_reply_calls() {
        let send: SendParams = serde_json::from_value(serde_json::json!({
            "to": "alice@example.com", "subject": "Hello", "body": "hi"
        }))
        .unwrap();
        let reply: ReplyParams = serde_json::from_value(serde_json::json!({
            "uid": 1, "body": "hi"
        }))
        .unwrap();
        assert!(send.cc.is_none());
        assert!(reply.cc.is_none());
        let email = add_cc_recipients(
            lettre::Message::builder()
                .from("sender@example.com".parse().unwrap())
                .to("alice@example.com".parse().unwrap()),
            &[],
        )
        .unwrap()
        .body("hi".to_string())
        .unwrap();
        assert_eq!(email.envelope().to().len(), 1);
        assert!(!String::from_utf8(email.formatted())
            .unwrap()
            .contains("Cc:"));
    }

    #[test]
    fn outbound_tools_advertise_multiple_cc_recipients() {
        for schema in [
            rmcp::handler::server::common::schema_for_input::<Parameters<SendParams>>().unwrap(),
            rmcp::handler::server::common::schema_for_input::<Parameters<ReplyParams>>().unwrap(),
        ] {
            let cc = &schema["properties"]["cc"];
            assert_eq!(cc["type"], "array");
            assert_eq!(cc["items"]["type"], "string");
            assert!(cc.get("anyOf").is_none());
            assert!(cc.get("description").is_some());
        }
    }

    #[test]
    fn input_schemas_are_compact_and_scalar_typed() {
        let schema = rmcp::handler::server::common::schema_for_input::<Parameters<ListParams>>()
            .expect("list input schema");
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .expect("input schema properties");
        let folder = properties.get("folder").expect("folder property");
        assert_eq!(folder["type"], "string");
        assert!(folder.get("anyOf").is_none());
        assert!(folder.get("default").is_none());
        assert!(folder.get("description").is_some());
        let limit = properties.get("limit").expect("limit property");
        assert_eq!(limit["type"], "integer");
        assert!(limit.get("format").is_none());

        fn assert_no_type_arrays(value: &Value) {
            match value {
                Value::Object(object) => {
                    assert!(!object.get("type").is_some_and(Value::is_array));
                    for value in object.values() {
                        assert_no_type_arrays(value);
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        assert_no_type_arrays(value);
                    }
                }
                _ => {}
            }
        }

        assert_no_type_arrays(&Value::Object(schema.as_ref().clone()));
    }

    #[test]
    fn optional_params_still_accept_explicit_null() {
        let p: ListParams =
            serde_json::from_value(serde_json::json!({"folder": null, "limit": null})).unwrap();
        assert!(p.folder.is_none() && p.limit.is_none() && p.offset.is_none());
    }

    #[test]
    fn pagination_is_newest_first_with_next_offset() {
        let uids = vec![3, 1, 5, 4, 2];
        assert_eq!(page_uids(uids.clone(), 0, 2), (vec![5, 4], Some(2)));
        assert_eq!(page_uids(uids.clone(), 2, 2), (vec![3, 2], Some(4)));
        assert_eq!(page_uids(uids.clone(), 4, 2), (vec![1], None));
        assert_eq!(page_uids(uids, 9, 2), (vec![], None));
    }

    #[test]
    fn failures_become_error_results_not_protocol_errors() {
        let result = tool_result(Err(anyhow::anyhow!("IMAP login failed"))).unwrap();
        assert_eq!(result.is_error, Some(true));
        let ok = tool_result(Ok("fine".into())).unwrap();
        assert_ne!(ok.is_error, Some(true));
    }

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
        let (_, body) = extract_body(raw);
        assert_eq!(body, "plain body");
    }

    #[test]
    fn filenames_cannot_escape_output_directory() {
        assert_eq!(sanitize_filename("../../secret.txt"), "secret.txt");
        assert_eq!(sanitize_filename("invoice:2026.pdf"), "invoice_2026.pdf");
    }

    #[test]
    fn one_com_uses_its_namespaced_sent_folder_by_default() {
        assert_eq!(
            crate::config::default_sent_folder("imap.one.com"),
            "INBOX.Sent"
        );
        assert_eq!(crate::config::default_sent_folder("imap.gmail.com"), "Sent");
    }
}
