## Hand-off: mail MCP server continuation

**Project:** `/Users/lagos/code/mail_mcp`
**Status:** Rust binary with 7 MCP tools live over stdio; release build verified
**Git:** https://github.com/LHagfoss/mail_mcp (pushed to main)

### What's already built
- **7 tools** exposed via `rmcp 3.2` + `#[tool_router(server_handler)]`:
  - `list_emails(folder?, limit?)` — newest-first email summaries (uid/from/subject/date)
  - `search_emails(query, folder?, limit?)` — IMAP `TEXT` search
  - `read_email(uid, folder?)` — full headers + 8k body text
  - `send_email(to, subject, body, attachments?)` — SMTP (STARTTLS on 587, implicit TLS on 465), optional file attachments via `mime_guess`; saves a copy to the configured IMAP Sent folder after SMTP accepts it
  - `reply_email(uid, body, folder?)` — preserves threading via `In-Reply-To`
  - `list_attachments(uid, folder?)` — `[{index, filename, mime, size}]`
  - `download_attachment(uid, index, folder?, out_dir?)` — saves to `out_dir` (defaults `attachments`), returns saved `path`
- **Config:** env vars `IMAP_HOST/PORT/USER/PASS`, `SMTP_HOST/PORT/USER/PASS/FROM`; `.env.example` included
- **Safety/config:** outbound mail is disabled by default; set `MAIL_SMTP_WRITE_ENABLED=true` to enable `send_email` and `reply_email`. Attachment downloads default to a 25 MiB limit via `MAIL_MAX_ATTACHMENT_BYTES`. `MAIL_SENT_FOLDER` overrides the Sent mailbox; one.com defaults to `INBOX.Sent`.
- **Dependencies:** `rmcp 3.2`, `imap 2.4`, `lettre 0.11`, `mailparse 0.16`, `mime_guess 2`, `tokio 1`, `serde`, `schemars`, `anyhow`, `dotenvy`, `tracing`
- **Build:** `cargo build` passes (dev mode, 6s). `tools/list` verified over stdio.
- **Verification:** `cargo fmt -- --check`, `cargo check`, `cargo test` (3 offline tests), `cargo build --release`, and stdio `tools/list` smoke test pass.
- **Architecture:** blocking IMAP done in `spawn_blocking`; async SMTP via `lettre`. All tools return `CallToolResult` + `ContentBlock::text`. Errors via `McpError::invalid_params` or `internal_error`.
- **Read/download quality:** list/search fetch headers only; `read_email` prefers text/plain and converts HTML-only messages to readable text; attachment downloads sanitize filenames and enforce a size cap.

### What's missing / next priorities
1. **Live-test against real IMAP/SMTP** — set real credentials, run `tools/list` → `tools/call read_email`, and set `MAIL_SMTP_WRITE_ENABLED=true` before testing `send_email`. Possible provider-specific issue: SMTP 587 STARTTLS vs 465 implicit TLS, or one.com `TEXT` search behavior.
2. **Rich triage** — mark-read, move/delete folders, flag management, pagination beyond 50.

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
