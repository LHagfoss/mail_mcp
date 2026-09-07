## Hand-off: mail MCP server continuation

**Project:** `/Users/lagos/code/mail_mcp`
**Status:** Fresh Rust binary with 7 MCP tools live over stdio
**Git:** https://github.com/LHagfoss/mail_mcp (pushed to main)

### What's already built
- **7 tools** exposed via `rmcp 3.2` + `#[tool_router(server_handler)]`:
  - `list_emails(folder?, limit?)` — newest-first email summaries (uid/from/subject/date)
  - `search_emails(query, folder?, limit?)` — IMAP `TEXT` search
  - `read_email(uid, folder?)` — full headers + 8k body text
  - `send_email(to, subject, body, attachments?)` — SMTP (STARTTLS on 587, implicit TLS on 465), optional file attachments via `mime_guess`
  - `reply_email(uid, body, folder?)` — preserves threading via `In-Reply-To`
  - `list_attachments(uid, folder?)` — `[{index, filename, mime, size}]`
  - `download_attachment(uid, index, folder?, out_dir?)` — saves to `out_dir` (defaults `attachments`), returns saved `path`
- **Config:** env vars `IMAP_HOST/PORT/USER/PASS`, `SMTP_HOST/PORT/USER/PASS/FROM`; `.env.example` included
- **Dependencies:** `rmcp 3.2`, `imap 2.4`, `lettre 0.11`, `mailparse 0.16`, `mime_guess 2`, `tokio 1`, `serde`, `schemars`, `anyhow`, `dotenvy`, `tracing`
- **Build:** `cargo build` passes (dev mode, 6s). `tools/list` verified over stdio.
- **Architecture:** blocking IMAP done in `spawn_blocking`; async SMTP via `lettre`. All tools return `CallToolResult` + `ContentBlock::text`. Errors via `McpError::invalid_params` or `internal_error`.

### What's missing / next priorities
1. **Live-test against real IMAP/SMTP** — set env vars, run `tools/list` → `tools/call read_email`, `tools/call send_email`. Most likely breakers: SMTP 587 STARTTLS vs 465 implicit TLS, one.com TEXT search support, `TEXT` vs `ALL` fetch. I'd run this in my own terminal.
2. **Read-quality polish** — HTML→text body conversion, attachment size caps so agent context stays bounded.
3. **Send safety** — require explicit `MAIL_SMTP_WRITE_ENABLED=true` before any `send_email`/`reply_email` can fire (currently no gate; sending just works if creds valid).
4. **Rich triage** — mark-read, move/delete folders, flag management, pagination beyond 50.

### Running from scratch (your terminal)
```bash
# 1. Set creds (never commit these!)
export IMAP_HOST=imap.one.com IMAP_PORT=993 IMAP_USER='you@one.com' IMAP_PASS='your-password'
export SMTP_HOST=send.one.com SMTP_PORT=587 SMTP_USER="$IMAP_USER" SMTP_PASS="$IMAP_PASS" SMTP_FROM="$IMAP_USER"

# 2. Build
cargo build  # or: cargo build --release for a single binary

# 3. MCP client config (Claude Code / Desktop etc.)
{
  "mcpServers": {
    "mail": {
      "command": "/Users/lagos/code/mail_mcp/target/debug/mail_mcp",
      "env": { "IMAP_USER": "you@one.com", "IMAP_PASS": "your-password", "SMTP_HOST": "send.one.com" }
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
2. Fix whatever `tools/call` returns errors (SMTP port/TLS, IMAP TEXT support).
3. Decide on read-quality: add `html` field to `read_email`, or strip tags, or add attachment size cap.
4. Decide on send gating: add `MAIL_SMTP_WRITE_ENABLED` env check in `main()`.
5. Add folder/move/delete if needed for agent triage.

The binary is a single `target/release/mail_mcp` — no Node/Python/runtime deps. Can be dropped into any MCP client config as a stdio transport server.