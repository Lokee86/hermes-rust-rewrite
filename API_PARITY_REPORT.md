# Hermes gateway API parity report

This report compares Mercury with stock Hermes Agent v0.23.3 for existing Desktop, Webapp, TUI, and headless clients. The source baseline is Mercury `1ac307f5427405a6ae571f188d6a7ea5c6bec4d0` and stock Hermes `caafe5389ae585ef77387a962aee05b1bcf17029`. See [the full method inventory](FEATURE_PARITY.md#gateway-contract-inventory) and [the stock OpenRPC registry](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/apps/shared/src/gateway-contract.openrpc.json).

## Advertised methods

Stock OpenRPC declares **263** unique methods. Mercury's `rpc.discover` combines **48** gateway entries and **36** profile-level entries. Of those **84** names, **61** appear in stock OpenRPC and **23** fall outside that registry. **202** stock names are absent from the advertised set. Counts measure declared dispatch coverage. A shared name still needs checks for request fields, replies, events, and the client roles that can call it. [Gateway table and discovery](crates/gateway/src/lib.rs) · [Profile-level table and role checks](crates/gateway/src/methods.rs)

| Stock namespace | Advertised by Mercury | Stock methods | Example to verify or add |
| --- | ---: | ---: | --- |
| `session` | 17 | 33 | Verify create, list, and history. Cover stock mutation workflows including rewind. |
| `prompt` | 1 | 3 | Verify `prompt.submit` admission and add coverage for `prompt.background` and `prompt.btw`. |
| `approval` | 3 | 3 | Verify offered decisions, retry, and pending state after reconnect. |
| `profiles` | 3 | 6 | Verify `profiles.list` and profile selection through the expected client roles. |
| `projects` | 6 | 15 | Verify project selection, workspace routing, and session assignment. |
| `groups` | 0 | 23 | Exercise group lifecycle, messaging, and session recovery through stock clients. |
| `mcp` | 1 | 11 | Verify `mcp.servers.list`, then cover management and OAuth workflows. |
| `plugins` | 1 | 3 | Verify `plugins.list` and stock management operations. |
| `cron` | 1 | 1 | Verify the actions and replies behind `cron.manage`. |
| `process`, `file`, and `shell` | 5 | 5 | Verify process control, attachment retrieval, and `shell.exec` through stock clients. |

The current source advertises `profiles.list` and `mcp.servers.list` on native connections. The profile-level handler requires a native session and restricts selected operations to the private profile client. Exercise discovery and both connection modes when checking these methods. [Profile-level handlers](crates/gateway/src/methods.rs)

## Storage and profile compatibility

This inventory describes the source revisions above. Paths are relative to the selected profile unless noted. Stock resolves its home through `HERMES_HOME`. Mercury's native data directory can be set with `HERMES_RUST_DATA_DIR`. [Stock profile paths](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/hermes_constants.py) · [Mercury library root](crates/library/README.md#storage-and-concurrency)

| Data | Stock Hermes | Mercury |
| --- | --- | --- |
| Chats and runtime state | SQLite `state.db` stores sessions, message rows, stored system prompts, FTS indexes, and runtime records such as admissions and worker receipts. [SQLite schema](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/persistence/sqlite/_schema_ddl.py) | `library.json` holds the catalog, projects, and selection. `sessions/<native-id>/conversation.json` holds history and durable gateway state. `conversation.running.json` checkpoints a streaming turn. The catalog and sessions use locks and atomic file replacement. [Native library](crates/library/README.md#storage-and-concurrency) · [History storage](crates/engine/src/history.rs) |
| Configuration and credentials | Profile settings are in `config.yaml`, with credentials from sources such as `.env` and `auth.json`. TOML appears in integrations such as Codex's `config.toml`. [Settings](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/hermes_cli/config.py) · [Auth store](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/hermes_cli/auth.py) · [Codex integration](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/hermes_cli/codex_runtime_plugin_migration.py) | Native settings include `agent-settings.json` and desktop preferences. The gateway reads selected `config.yaml` settings for dashboard auth and the terminal backend. [Agent settings](crates/engine/src/settings.rs) · [Gateway auth](crates/gateway/src/auth.rs) · [Terminal backend](crates/gateway/src/shell_exec.rs) |
| Persona and memory | `SOUL.md` and workspace context contribute to the system prompt. `memories/MEMORY.md` and `memories/USER.md` store agent and user notes. [Prompt builder](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/agent/prompt_builder.py) · [Memory](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/website/docs/user-guide/features/memory.md) | The engine also reads `SOUL.md` and supports those Markdown note files. [Native prompt assembly](crates/engine/src/lib.rs) · [Native notes](crates/engine/src/memory.rs) |
| Scheduled jobs | Job definitions use `cron/jobs.json`. [Cron store](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/cron/jobs.py) | The native scheduler also stores definitions in `cron/jobs.json`. Test schedule parsing, ownership, and next-run state before moving jobs between the two. [Native cron store](crates/engine/src/cron/store.rs) |

Mercury's manual history importer copies the SQLite database and any WAL into a private read-only snapshot. It imports a selected conversation as a new native chat, converts text and recorded tool turns, and retains stock IDs and source rows in `hermes-source.json`. Reimporting returns the existing native copy. Attachment bytes, provider settings, and stock prompt context each need a migration check of their own. [Import behavior](crates/library/README.md#manual-hermes-import) · [Importer](crates/library/src/import.rs)

The native backup uses a versioned JSON archive. Restore creates detached chats with new IDs and keeps settings, memory, and queued work as recovery copies. That is a Mercury restore path. A stock-readable rollback that retains chats created in Mercury still needs a compatibility store or tested export. [Native backup and restore](crates/library/src/native_backup.rs)

For a cutover, use a disposable populated profile with a live WAL fixture, branched history, media, custom settings, prompt files, and a pending turn. Verify the ID map, resume and search results, attachment bytes, effective configuration and prompt behavior, and recovery from an interrupted or repeated migration. If both applications need to operate on one profile, also verify the on-disk contract and writer ownership. The existing SQLite import fixtures are a starting point for that coverage. [Import tests](crates/library/src/tests.rs)

## Windows observations

The runtime probes used stock v0.23.3 and Mercury's Windows proof build labeled `3d770ee` in the integration plan. They launched stock `hermes serve` and Mercury `gateway run` on loopback with fresh isolated profiles, recorded HTTP status codes and top-level JSON fields, and sent representative JSON-RPC requests. Two paired captures produced byte-identical summaries. [Mercury Windows proof build](ONE-GATEWAY-HERDR-PLAN.md#windows-x64-release-verification)

| Probe | Stock Hermes | Mercury proof build |
| --- | --- | --- |
| `GET /api/health` | HTTP 200 | HTTP 404. Mercury's `GET /health` returned HTTP 200. |
| `GET /api/status` | HTTP 200 with 40 top-level fields | HTTP 200 with 9 top-level fields |
| `GET /api/config` | HTTP 200 | HTTP 404 |
| `GET /api/sessions` after creating a session | HTTP 200 with `limit`, `offset`, `sessions`, `storage`, and `total` | HTTP 200 with the same top-level fields |
| Legacy WebSocket `session.list` and `session.history` | List returned `sessions`. History returned `count` and `messages` for a new chat. | Same top-level fields for the tested cases |
| Legacy WebSocket `session.create` | `info`, `message_count`, `messages`, `session_id`, `stored_session_id` | All five stock fields plus `messages_omitted`, `open_requests`, `pending_approval`, `running`, and `turn_started_at` |
| Legacy WebSocket `profiles.list` and `mcp.servers.list` | Both returned results | Both returned `-32601` on this build. The current source has native handlers. |

The unchanged stock Python gateway client also passed the approval, disconnect, reconnect, retry, and history fixture against this Windows build. It discovered the native owner, exercised a deterministic tool turn, and recovered a four-message history. [Stock client fixture](scripts/verify_stock_gateway_client.py)

Windows Application Control blocked Rust Anything v4.1.0 and a fresh local Mercury build. These runtime results apply to the checksum-verified proof build. A runtime replay against the audited Mercury revision is pending.

## Drop-in acceptance checks

1. **Launch and connect.** Run unchanged stock clients against the Mercury package. Check `hermes serve` invocation, readiness and shutdown, `/api/health`, `/api/status`, and configuration routes. Compare the required response fields and errors. Exercise both private local discovery and deployed HTTPS sign-in.
2. **Open and migrate sessions.** Run the [storage and profile checks](#storage-and-profile-compatibility) on populated stock profiles. Verify history order and pagination across restart with the stock response schema and unchanged client decoders.
3. **Run, approve, and recover turns.** Replay simultaneous submissions, implicit queueing, first receipts, approval after disconnect, cancellation, and restart boundaries. Verify that a retried request yields one committed outcome and the expected ordered events. [Gateway integration plan](ONE-GATEWAY-HERDR-PLAN.md#what-follows-the-first-demonstration)
4. **Exercise terminals and management APIs.** Run the existing stock browser-terminal and composer fixtures. Cover profile, project, group, MCP, plugin, and routine workflows using the stock method names and the appropriate client roles. [Browser-terminal fixture](scripts/verify_stock_browser_terminal.cjs) · [Composer fixture](scripts/verify_stock_composer_shell.py)
5. **Exercise headless callers.** Stock's opt-in OpenAI-compatible adapter serves `/v1/models`, Chat Completions, and Responses. Those routes are absent from the audited Mercury gateway router. For deployments that use this adapter, include the HTTP, SSE, response retrieval, and retry behaviors in the acceptance suite. [Stock API adapter](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/gateway/platforms/api_server.py) · [Mercury router](crates/gateway/src/lib.rs)

Use the [Rust Anything release binary](https://github.com/erkamyaman/rust-anything/releases/tag/v4.1.0) on an approved runner to record deterministic CLI and HTTP cases against pinned stock Hermes, replay the reference against itself, and then replay against the matching Mercury package with `app replay --build`. Use the existing stock-client fixtures for WebSocket flows, approvals, and reconnection. [Rust Anything whole-app workflow](https://github.com/erkamyaman/rust-anything/blob/v4.1.0/docs/whole-apps.md)
