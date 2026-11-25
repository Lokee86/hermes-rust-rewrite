# Hermes gateway and storage parity report

This report compares Mercury, the Rust rewrite in this repository, with stock Hermes Agent v0.23.3 for existing profiles and Desktop, Webapp, TUI, and headless clients. The source baseline is Mercury `1ac307f5427405a6ae571f188d6a7ea5c6bec4d0` and stock Hermes `caafe5389ae585ef77387a962aee05b1bcf17029`. See [the full method inventory](FEATURE_PARITY.md#gateway-contract-inventory) and [the stock OpenRPC registry](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/apps/shared/src/gateway-contract.openrpc.json).

## Advertised methods

The stock OpenRPC file has two method lists. `methods` has **263** names, served by standalone `hermes serve`. `x-canonical-methods` has **54**, the wire of native sessions. **25** names are in both, so the file declares **292** unique names.

Mercury's `rpc.discover` combines **48** gateway entries and **36** profile-level entries. Of those **84** names, **67** appear in the stock file and **17** are Mercury's own. Counts measure declared dispatch coverage. A shared name still needs checks for request fields, replies, events, and the client roles that can call it. [Gateway table and discovery](crates/gateway/src/lib.rs) · [Profile-level table and role checks](crates/gateway/src/methods.rs)

| Stock list | Stock names | Advertised by Mercury | Absent |
| --- | ---: | ---: | ---: |
| `methods` | 263 | 61 | 202 |
| `x-canonical-methods` | 54 | 15 | 39 |
| Either list | 292 | 67 | 225 |

Mercury advertises these native names: `approval.respond`, `profiles.list`, `prompt.cancel`, `prompt.receipt`, `prompt.resolve_unknown`, `prompt.submit`, `runtime.describe`, `session.create`, `session.events.since`, `session.info`, `session.interrupt`, `session.list`, `session.mutate`, `session.resume`, `shell.exec`.

These native names are absent: `a2a.forward`, `bot_relay.cancel`, `bot_relay.deliver`, `bot_relay.settle`, `clarify.lock`, `clarify.respond`, `command.observe`, `config.get`, `config.set`, `cron.cancel`, `cron.recover`, `cron.status`, `cron.submit`, `groups.approve`, `groups.attachment.download`, `groups.attachment.upload`, `groups.create`, `groups.discard`, `groups.peer.invite`, `groups.peer.register`, `groups.peer.revoke`, `groups.retry`, `image.attach_bytes`, `kanban.run`, `prompt.replace_queued`, `prompt.steer_queued`, `rollback.session_diff`, `session.account_usage`, `session.automation`, `session.detach`, `session.export`, `session.prune`, `session.redirect`, `session.stats`, `session.steer`, `session.transcript`, `worker.adopt`, `worker.persist`, `worker.register`. The file also declares one native method prefix, `plugin.{plugin_id}.{name}`, which Mercury does not serve.

This table counts the 263 names under `methods`.

| Stock namespace | Advertised by Mercury | Stock methods | Example to verify or add |
| --- | ---: | ---: | --- |
| `session` | 17 | 33 | Verify create, list, and history. Stock rewinds through the native `session.mutate`; Mercury's accepts `rename` and `model` only. |
| `prompt` | 1 | 3 | Verify `prompt.submit` admission and add coverage for `prompt.background` and `prompt.btw`. |
| `approval` | 3 | 3 | Verify offered decisions, retry, and pending state after reconnect. |
| `profiles` | 3 | 6 | Verify `profiles.list` and profile selection through the expected client roles. |
| `projects` | 6 | 15 | Verify project selection, workspace routing, and session assignment. |
| `groups` | 0 | 23 | Exercise group lifecycle, messaging, and session recovery through stock clients. |
| `mcp` | 1 | 11 | Verify `mcp.servers.list`, then cover management and OAuth workflows. |
| `plugins` | 1 | 3 | Verify `plugins.list` and stock management operations. |
| `cron` | 1 | 1 | Verify the actions and replies behind `cron.manage`. |
| `process`, `file`, and `shell` | 5 | 5 | Verify process control, attachment retrieval, and `shell.exec` through stock clients. |

`rpc.discover` returns the same list on every connection, so a name in it can still be refused. The profile-level methods, `cron.chat`, and `cron.trigger` need a native session. Selected operations also need the private profile client. On a legacy connection they answer `4001` (`unsupported_control`). At `1ac307f` an unknown method also got `4001` there. Since `7386ebb` it gets the `-32601` that the gateway README documents and stock clients expect. Check discovery and both connection modes for each method. [Profile-level handlers](crates/gateway/src/methods.rs) · [Documented errors](crates/gateway/README.md)

## Storage and profile compatibility

**Direction.** Mercury keeps its own data folder and does not write to a Hermes profile. Its chats are moving from JSON files to Mercury's own SQLite database. Opening an existing Hermes profile under `HERMES_HOME` and writing the stock stores directly is an [option for later](#option-share-the-stock-stores), not a requirement. It would be off unless a user turned it on.

This comparison describes the source revisions above. Paths are relative to the selected profile unless noted. Stock resolves its home through `HERMES_HOME`. Mercury's native data directory can be set with `HERMES_RUST_DATA_DIR`. [Stock profile paths](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/hermes_constants.py) · [Mercury library root](crates/library/README.md#storage-and-concurrency)

| Data | Stock Hermes | Mercury |
| --- | --- | --- |
| Chats and runtime state | SQLite `state.db` stores sessions, message rows, stored system prompts, FTS indexes, and runtime records such as admissions and worker receipts. [SQLite schema](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/persistence/sqlite/_schema_ddl.py) | `library.json` holds the catalog, projects, and selection. `sessions/<native-id>/conversation.json` holds history and durable gateway state. `conversation.running.json` checkpoints a streaming turn. The same folder can hold `gateway.json`, `queued-prompts.json`, `draft-attachments.json`, `remote-session.json`, `hermes-source.json`, process receipts, and `attachments/`. The catalog and sessions use locks and atomic file replacement. [Native library](crates/library/README.md#storage-and-concurrency) · [History storage](crates/engine/src/history.rs) |
| Projects | Per-profile SQLite `projects.db` holds projects, folders, and project metadata. Stock relates sessions to project workspaces. [Stock projects database](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/hermes_cli/projects_db.py) · [Stock project routing](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/tui_gateway/methods_projects.py) | `library.json` holds project entries and selection alongside the chat catalog. [Native library](crates/library/README.md#storage-and-concurrency) |
| Configuration and credentials | Profile settings are in `config.yaml`, with credentials from sources such as `.env` and `auth.json`. TOML appears in integrations such as Codex's `config.toml`. [Settings](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/hermes_cli/config.py) · [Auth store](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/hermes_cli/auth.py) · [Codex integration](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/hermes_cli/codex_runtime_plugin_migration.py) | Native settings include `agent-settings.json` and desktop preferences. Provider keys are in `providers/endpoints.json` and sign-ins are in `accounts-oauth.json` and `portal-oauth.json` in the main data directory. They are sealed with a key from the system credential store where one is available, but not when `HERMES_RUST_DATA_DIR` is set. Messaging, remote gateway, and MCP credentials have their own files. Some sign-ins can be borrowed read-only from stock's `auth.json`. Copied sealed values cannot be opened on another computer. The gateway reads selected `config.yaml` settings for dashboard auth and the terminal backend. [Agent settings](crates/engine/src/settings.rs) · [Credentials at rest](crates/engine/src/secret.rs) · [Gateway auth](crates/gateway/src/auth.rs) · [Terminal backend](crates/gateway/src/shell_exec.rs) |
| Persona and memory | `SOUL.md` and workspace context contribute to the system prompt. `memories/MEMORY.md` and `memories/USER.md` store agent and user notes. [Prompt builder](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/agent/prompt_builder.py) · [Memory](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/website/docs/user-guide/features/memory.md) | The engine also reads `SOUL.md` and supports those Markdown note files. [Native prompt assembly](crates/engine/src/lib.rs) · [Native notes](crates/engine/src/memory.rs) |
| Scheduled jobs | Job definitions use `cron/jobs.json`. [Cron store](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/cron/jobs.py) | The native scheduler also uses `cron/jobs.json`, in its own format with run history in the same file. Test schedule parsing, ownership, and next-run state before moving jobs between the two. [Native cron store](crates/engine/src/cron/store.rs) |
| Bots, groups, and plugins | Not compared here. | `bots.json`, `groups.json`, `webhooks.json`, `plugins/installed.json`, and `plugin-data/<plugin>/config.json`. [Bots and groups](crates/library/src/bots.rs) · [Plugins](crates/engine/src/plugins.rs) |

Mercury's manual history importer copies the SQLite database and any WAL into a private read-only snapshot. It imports a selected conversation as a new native chat, converts text and recorded tool turns, and retains stock IDs and source rows in `hermes-source.json`. Reimporting returns the existing native copy. Attachment bytes, provider settings, and stock prompt context each need a migration check of their own. [Import behavior](crates/library/README.md#manual-hermes-import) · [Importer](crates/library/src/import.rs)

The native backup uses a versioned JSON archive. Restore creates detached chats with new IDs and keeps settings, memory, and queued work as recovery copies. Credentials and workspace files are not in the archive. [Native backup and restore](crates/library/src/native_backup.rs)

### Option: share the stock stores

This describes what sharing one profile with stock Hermes would take. A fault in Mercury could then damage the user's Hermes data, and a stock schema change could break Mercury, so it needs its own decision and opt-in. It would mean using the stock SQLite schemas, migrations, indexes, triggers, and persistence semantics, and preserving the profile files and their precedence. Stock Hermes would have to be able to reopen the same profile after Mercury has added history.

- Move the session catalog and transcripts from `library.json` and `conversation.json` to `state.db`. Preserve the existing stock IDs, lineage, message identities, archive/pin state, tool results, stored prompts, attachment references, and full-text search. Use stock runtime tables and transaction rules for admissions, execution receipts, interruption, and recovery. This replaces the separate running checkpoint and queue as active sources of truth.
- Move projects and workspace selection out of `library.json` into `projects.db`. Preserve stock project IDs, folders, and session associations through the stock workspace rules.
- Move profile settings from `agent-settings.json` to the stock `config.yaml` keys. Reuse the stock environment and credential locations, `SOUL.md`, and `memories/` Markdown files. Match the stock `cron/jobs.json` schema and behavior. Map the remaining Mercury JSON state for stock-backed features to the corresponding stock stores.

### Checks for that option

Before converting existing Mercury profiles, back up the original files and pause active writers before converting the JSON state. Match previously imported chats to their stock origins. Preserve Mercury-added turns and divergent branches, record ID collisions and unmapped fields, and verify the result before activating the stock stores. After cutover, retire the replaced JSON files as active state.

**Acceptance.** Open a disposable populated Hermes profile in Mercury, resume an existing session, complete a turn, and reopen that same profile in stock Hermes. Stock must find the new turn under the original session ID, including through search. Check attachments, projects, effective settings, prompt files, and ownership after restart. Separately migrate a populated Mercury profile twice and interrupt a migration before completion. Confirm recovery, stable mappings, and no duplicated messages, tool effects, or scheduled jobs. [Existing SQLite fixtures](crates/library/src/tests.rs)

## Windows observations

The runtime probes used stock v0.23.3 and Mercury's Windows proof build labeled `3d770ee` in the integration plan. That checkpoint was squashed and is not in this repository's history. The probes launched stock `hermes serve` and Mercury `gateway run` on loopback with fresh isolated profiles, recorded HTTP status codes and top-level JSON fields, and sent representative JSON-RPC requests. Two paired captures produced byte-identical summaries. The probe script and its captures are not checked in, so this table cannot be reproduced from the repository. [Mercury Windows proof build](ONE-GATEWAY-HERDR-PLAN.md#windows-x64-release-verification)

| Probe | Stock Hermes | Mercury proof build |
| --- | --- | --- |
| `GET /api/health` | HTTP 200 | HTTP 404. Mercury's `GET /health` returned HTTP 200. |
| `GET /api/status` | HTTP 200 with 40 top-level fields | HTTP 200 with 9 top-level fields |
| `GET /api/config` | HTTP 200 | HTTP 404 |
| `GET /api/sessions` after creating a session | HTTP 200 with `limit`, `offset`, `sessions`, `storage`, and `total` | HTTP 200 with the same top-level fields |
| Legacy WebSocket `session.list` and `session.history` | List returned `sessions`. History returned `count` and `messages` for a new chat. | Same top-level fields for the tested cases |
| Legacy WebSocket `session.create` | `info`, `message_count`, `messages`, `session_id`, `stored_session_id` | All five stock fields plus `messages_omitted`, `open_requests`, `pending_approval`, `running`, and `turn_started_at` |
| Legacy WebSocket `profiles.list` and `mcp.servers.list` | Both returned results | Both returned `-32601` on this build. The current source serves them to native sessions and answers `4001` to a legacy WebSocket. |

The unchanged stock Python gateway client also passed the approval, disconnect, reconnect, retry, and history fixture against this Windows build. It discovered the native owner, exercised a deterministic tool turn, and recovered a four-message history. [Stock client fixture](scripts/verify_stock_gateway_client.py)

Windows Application Control blocked Rust Anything v4.1.0 and a fresh local Mercury build. These runtime results apply to the checksum-verified proof build. A runtime replay against the audited Mercury revision is pending.

## Drop-in acceptance checks

1. **Launch and connect.** Run unchanged stock clients against the Mercury package. Mercury starts its gateway with `gateway run` and has no `serve` command. Check `hermes serve` invocation, readiness and shutdown, `/api/health`, `/api/status`, and configuration routes. Compare the required response fields and errors. Exercise both private local discovery and deployed HTTPS sign-in.
2. **Open and migrate sessions.** Run the import and storage checks on populated stock profiles without changing them, and migrate existing Mercury JSON chats once. Verify history order and pagination across restart with unchanged stock client decoders. The [shared-store checks](#checks-for-that-option) apply only if that option is built.
3. **Run, approve, and recover turns.** Replay simultaneous submissions, implicit queueing, first receipts, approval after disconnect, cancellation, and restart boundaries. Verify that a retried request yields one committed outcome and the expected ordered events. [Gateway integration plan](ONE-GATEWAY-HERDR-PLAN.md#what-follows-the-first-demonstration)
4. **Exercise terminals and management APIs.** Run the existing stock browser-terminal and composer fixtures. Cover profile, project, group, MCP, plugin, and routine workflows using the stock method names and the appropriate client roles. [Browser-terminal fixture](scripts/verify_stock_browser_terminal.cjs) · [Composer fixture](scripts/verify_stock_composer_shell.py)
5. **Exercise headless callers.** Stock's opt-in OpenAI-compatible adapter serves `/v1/models`, Chat Completions, and Responses. Those routes are absent from the audited Mercury gateway router. For deployments that use this adapter, include the HTTP, SSE, response retrieval, and retry behaviors in the acceptance suite. [Stock API adapter](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/gateway/platforms/api_server.py) · [Mercury router](crates/gateway/src/lib.rs)

[Rust Anything v4.1.0](https://github.com/erkamyaman/rust-anything/releases/tag/v4.1.0) is one option for recording and replay. It is a third-party binary published on 24 November 2026. Check the archive against the release's `SHA256SUMS` and run it on an approved, isolated runner without credentials. Use it to record deterministic CLI and HTTP cases against pinned stock Hermes, replay the reference against itself, and then replay against the matching Mercury package with `app replay --build`. Use the existing stock-client fixtures for WebSocket flows, approvals, and reconnection. [Rust Anything whole-app workflow](https://github.com/erkamyaman/rust-anything/blob/v4.1.0/docs/whole-apps.md)
