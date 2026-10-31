## Hand-off: mail MCP server continuation

**Project:** `/Users/lagos/code/mail_mcp`
**Status:** Rust binary with 12 MCP tools live over stdio; baseline release build verified
**Git:** https://github.com/LHagfoss/mail_mcp (pushed to main)

### What's already built
- **12 tools** exposed via `rmcp 3.2` + `#[tool_router(server_handler)]`:
  - `list_folders` — list server folders and selectable state
  - `list_emails(folder?, limit?, offset?)` — newest-first summaries including unread, flagged, attachment-presence, and byte size
  - `search_emails(query?, from?, subject?, since?, before?, unread?, has_attachments?, folder?, limit?, offset?)` — structured IMAP search and paginated summaries; attachment scans capped at 2,000 messages
  - `read_email(uid, folder?)` — headers and up to 8k body characters; uses `BODY.PEEK[]`, so it does not mark mail read
  - `set_email_flags(uid, confirm_change, unread?, flagged?, folder?)` — update one message's read/flagged state; explicit confirmation and mailbox-write opt-in required
  - `move_email(uid, destination, confirm_move, folder?)` — move one message, requiring explicit confirmation, IMAP MOVE support, and mailbox-write opt-in
  - `trash_email(uid, confirm_move, folder?)` — move one message to configured Trash (not permanently expunged); explicit confirmation and mailbox-write opt-in required
  - `send_email(to, subject, body, cc?, attachments?, confirm_send)` — SMTP send; requires explicit per-call confirmation and outbound opt-in
  - `reply_email(uid, body, cc?, confirm_send)` — preserves `In-Reply-To` threading; also requires confirmation and outbound opt-in
  - `list_attachments(uid, folder?)` — `[{index, filename, mime, size}]`
  - `read_attachment_text(uid, index, folder?, max_output_chars?)` — bounded plain-text/HTML/DOCX/XLSX extraction; PDF extraction is disabled until decompression can be safely bounded
  - `download_attachment(uid, index, folder?, out_dir?)` — writes under attachment root and refuses overwrite
- **Config:** env vars `IMAP_HOST/PORT/USER/PASS`, `SMTP_HOST/PORT/USER/PASS/FROM`; `.env.example` included
- **Safety/config:** SMTP send and IMAP mutation are separately disabled by default (`MAIL_SMTP_WRITE_ENABLED`, `MAIL_IMAP_WRITE_ENABLED`) and sending additionally requires `confirm_send=true`. Attachment access is restricted to `MAIL_ATTACHMENT_ROOT` (default `attachments`); per-file, aggregate outgoing, and message limits are configurable with `MAIL_MAX_ATTACHMENT_BYTES`, `MAIL_MAX_TOTAL_ATTACHMENT_BYTES`, and `MAIL_MAX_EMAIL_BYTES`. `MAIL_SENT_FOLDER` and `MAIL_TRASH_FOLDER` are provider-specific; verify actual names before enabling Trash.
- **Dependencies:** `rmcp 3.2`, `imap 2.4` / `imap-proto 0.10`, `lettre 0.11`, `mailparse 0.16`, `mime_guess 2`, `pdf-extract 0.12`, `quick-xml 0.38`, `zip 2`, `tokio 1`, `serde`, `schemars`, `anyhow`, `dotenvy`, `tracing`
- **Verification:** Latest local test run passed 21 tests. Run `cargo fmt --all -- --check`, `cargo test --locked`, and `cargo build --release --locked` for this branch before merging. Live IMAP/SMTP behavior has not been verified; validate provider folders/capabilities without enabling writes by default.
- **Architecture:** blocking IMAP done in `spawn_blocking`; async SMTP via `lettre`. All tools return `CallToolResult` + `ContentBlock::text`. Errors via `McpError::invalid_params` or `internal_error`.
- **CC:** both outbound tools accept an optional array, e.g. `"cc": ["alice@example.com", "Bob <bob@example.com>"]`. Each entry is one mailbox; all CC recipients are included in the visible headers and SMTP delivery envelope. Omitted, null, or empty CC lists send without CC. Replies include only explicitly supplied CC recipients; they do not automatically copy the original message's recipients.
- **Read/download quality:** list/search fetch headers only; `read_email` prefers text/plain and converts HTML-only messages to readable text; attachment downloads sanitize filenames and enforce a size cap.

### Remaining verification / next priorities
1. **Live-test against real IMAP/SMTP** — set real credentials and verify folder names, supported MOVE capability, structured search, PDF/DOCX/XLSX extraction, and SMTP behavior. Do not enable sending or mailbox mutations for tests without explicit approval.
2. **Optional future work** — OAuth2 provider authentication, richer date semantics, incremental IMAP IDLE updates, and extracting scanned PDFs via an explicitly configured OCR service. The built-in PDF extractor does not OCR image-only PDFs.
3. **Provider setup** — confirm the actual Trash folder name and set `trash_folder` / `MAIL_TRASH_FOLDER`; `trash_email` requires the IMAP MOVE extension and never falls back to mailbox-wide EXPUNGE.

### Running from scratch (your terminal)
```bash
# 1. Set creds (never commit these!)
export IMAP_HOST=imap.one.com IMAP_PORT=993 IMAP_USER='you@one.com' IMAP_PASS='your-password'
export SMTP_HOST=send.one.com SMTP_PORT=587 SMTP_USER="$IMAP_USER" SMTP_PASS="$IMAP_PASS" SMTP_FROM="$IMAP_USER"
# Keep false for read-only testing; set true only when testing send/reply.
export MAIL_SMTP_WRITE_ENABLED=false
# Optional; defaults to 25 MiB.
export MAIL_MAX_ATTACHMENT_BYTES=26214400
# Optional; one.com defaults to INBOX.Sent.
export MAIL_SENT_FOLDER=INBOX.Sent

# 2. Build
cargo build  # or: cargo build --release for a single binary

# 3. MCP client config (Claude Code / Desktop etc.)
# Downloads and outgoing attachments are confined to MAIL_ATTACHMENT_ROOT (default ./attachments).
# IMAP mutations require MAIL_IMAP_WRITE_ENABLED=true; outbound mail requires
# MAIL_SMTP_WRITE_ENABLED=true plus confirm_send=true on the call.
# Set MAIL_TRASH_FOLDER to the provider's real Trash mailbox name if needed.
{
  "mcpServers": {
    "mail": {
      "command": "/Users/lagos/code/mail_mcp/target/debug/mail_mcp",
      "env": { "IMAP_USER": "you@one.com", "IMAP_PASS": "your-password", "SMTP_HOST": "send.one.com", "MAIL_SMTP_WRITE_ENABLED": "false" }
    }
  }
}

# 4. Quick test
# list 2 newest:
printf '%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}' \
 '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_emails","arguments":{"limit":2}}}' \
| /path/mail_mcp/target/debug/mail_mcp 2>/dev/null | tail -1 | python3 -m json.tool
```

### To continue from here
If someone else takes this, they should:
1. Run the live test in their own terminal (steps above).
2. Fix any provider-specific errors from `tools/call` (SMTP port/TLS or IMAP `TEXT` support).
3. Add folder/move/delete if needed for agent triage.

The binary is a single `target/release/mail_mcp` — no Node/Python/runtime deps. Can be dropped into any MCP client config as a stdio transport server.
