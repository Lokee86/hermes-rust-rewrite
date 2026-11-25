# Hermes Desktop feature parity

Audit of the Rust port, updated 25 November 2026. The reference is the public NousResearch source release **v0.23.3**, commit `caafe5389ae585ef77387a962aee05b1bcf17029`. The migration branch started at `0284ce17d9bf9ed2f1832ad8950dfac2394d45ab`. The published source tag and its desktop guide define the comparison. Installer release timing can differ by platform. [Release notes](https://github.com/NousResearch/hermes-agent/releases/tag/v0.23.3) · [Desktop guide](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/website/docs/user-guide/desktop.md)

## What runs

The bundle holds two Cargo-built native programs: `hermes-desktop` (Iced 0.14, a multi-window daemon, which also runs the profile gateway and the embedded Herdr terminal core) and `hermes-python-host`. The Rust engine runs turns, streams replies, asks for approval and saves results. There is no Electron, Tauri or Node runtime. HTML artifacts preview in the OS webview (wry: WKWebView, WebView2, WebKitGTK); the global ⌘⇧Space hotkey uses the `global-hotkey` crate. Python plugins run in `hermes-python-host`, which embeds the CPython runtime shipped in the package (python-build-standalone 3.13: `libpython3.13.dylib` on macOS, `libpython3.13.so` on Linux, `python313.dll` on Windows) and supplies `PluginHost` as the positional plugin registration object. Existing Python plugins can use `register(ctx)`. A plugin's `python_dependencies` install as wheels into its data folder. The profile gateway keeps embedded workers available for tools, supported hooks, plugin slash commands, and selected memory providers. The installed-agent bridge remains a legacy tool path. The "hey hermes" wake-word model still needs that agent (`desktop/assets/wake_listener.py`); without it the wake word falls back to spotting the phrase in live dictation.

Evidence levels below: **verified live** means driven against real services or the real window on Apple Silicon macOS; **tested** means automated behaviour tests (`cargo test --workspace`); **built** means compiled and reviewed but not exercised end to end. Windows and Linux are built, tested and started (the packaged app must show its window) in CI (`.github/workflows/build.yml`) and have not been driven by hand.

## Published routes

| Upstream route | Rust status | Working behavior and outstanding scope |
| --- | --- | --- |
| `/` and conversation IDs | Done | Create, select, rename, pin, archive and resume chats; drafts; streamed Markdown; approvals; cancel; @file/@folder/@diff/@git/@url references; attachments; prompt queue; group chats; subagents (`delegate_task`); "Open in new window"; cross-window sync of the chat list (tested). |
| `/settings` | Done | Accounts (Nous Portal OAuth, OpenAI Codex, xAI, MiniMax, Qwen device sign-ins, Anthropic/Claude Code), API keys, custom endpoints, model catalog and picker, reasoning/fast presets, appearance, shortcuts, voice, status bar, gateways (local, remote with native OAuth, SSH). Device codes verified live. |
| `/session-import` | Done | Import from the Python agent's SQLite store and other sources into independent native copies. |
| `/capabilities` | Done | Skills catalog with enable/disable, MCP servers (HTTP, stdio, OAuth 2.1), plugin catalog (2,490 pinned entries): install, enable, settings form, sign-in, update, remove. Agent Plugins v1 packages verified live (Aave); Python plugins tested in the built-in Python (tools, async tools, `check_fn`, config, `pre_tool_call`/`transform_tool_result`/`post_tool_call` hooks, PyPI wheel install) and through the installed agent fallback. |
| `/artifacts` | Done | Gallery, search, filters, previews including live HTML in the OS webview, origin navigation. |
| `/messaging` | Done | Channel setup and live conversations through the gateway adapters the Rust gateway implements. |
| `/webhooks` | Done | Create, enable, delete; HMAC-SHA256 verified deliveries (`/webhooks/{name}`) run the agent into a "Webhook · name" chat (tested). |
| `/cron` | Done | Routines page with 16 templates, scripts, script-only jobs, change monitors, scheduler, run history, approvals; sidebar "Cron jobs" section with countdowns and run peek; each routine has a chat that shows its run live while it streams. |
| `/profiles` | Done | Create, rename, delete, SOUL editing, export/import (tar.gz). |
| `/agents` | Done | Live subagent tree (`subagent.progress`), open a child's transcript. |
| `/command-center` | Done | Command Center screen and actions. |
| `/starmap` | Done | Canvas graph of enabled skills, related links, clusters and memory cards. |
| Contributed routes | Partial | Plugins add tools, skills and MCP servers. A plugin's desktop page runs in a sandboxed web view with React and the page part of the SDK (tested; checked in WKWebView). Docked panes, themes, palette and composer contributions are not loaded. |

## Workflows within and outside those routes

| Published behavior | Native scope |
| --- | --- |
| HUD mode (⌘⇧H) | Own frameless always-on-top window with native frost (NSVisualEffectView / Windows 11 acrylic), idle fade, click-through while faded, hover or a reply brings it back; composer, + menu, model picker and reasoning menu work inside it. Verified live on macOS with real AppKit click and key events. Linux keeps a solid band. |
| Quick entry, pet, wake pill | ⌘⇧Space quick-entry window, desktop pet (petdex gallery and AI pet generation via Nous Portal), wake indicator. Verified live. |
| Voice | Dictation, read-aloud and voice conversation on macOS (Speech + AVFoundation). Wake word: the Python agent's "hey hermes" openWakeWord model (tested, installs its packages on first use), else phrase spotting. On Windows and Linux, dictation records with the default microphone and transcribes on the computer with faster-whisper (recognition verified live on macOS; the recorder is built, not driven, there). |
| Cloud | Hermes Cloud agent discovery through the Portal (verified live: listing works). Connecting to a cloud agent needs one that is reachable. |
| Connections | Local gateway, remote gateway (token or native OAuth), SSH (remote `hermes serve` + port forward). SSH is tested against a real OpenSSH server on the same machine (keys, token upload, start, forward, cleanup); a remote Hermes has not been driven. |
| Terminal, files, Git | PTY tabs, project file tree and previews, repository status and diffs. |
| Packaging | macOS app and disk image, Windows zip and per-user installer, Linux tarball and Debian package from CI. Signed and notarized only when the workflow has certificates. Settings › About checks for and downloads updates; installing is manual. |

## Gateway contract inventory

_Counted on 25 November 2026 against Mercury `1ac307f` and stock Hermes v0.23.3, from the method tables in `crates/gateway/src`._

The published OpenRPC file declares **263** names under `methods` and **54** under `x-canonical-methods`, the native session wire. **25** names are in both lists, so it declares **292 unique method names**. The Rust gateway advertises **84** methods, **67** of which appear in that file: **61** of the 263 and **15** of the 54, with nine in both. The table below covers the 263, of which **202** are absent from the Rust method table. The [API parity report](API_PARITY_REPORT.md#advertised-methods) lists the native ones. A matching name establishes dispatch coverage only. It does not certify complete request schemas or behavior, and the count is not a desktop feature percentage. [Published OpenRPC registry](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/apps/shared/src/gateway-contract.openrpc.json) · [Native handlers](crates/gateway/src/lib.rs) · [Gateway wire and tests](crates/gateway/README.md)

| Published namespace | Advertised names in the Rust gateway | Outstanding published names |
| --- | --- | --- |
| agents | `agents.list` | — |
| approval | `approval.pending`, `approval.received`, `approval.respond` | — |
| billing | — | `billing.auto_reload`, `billing.charge`, `billing.charge_status`, `billing.keep_going_on_credits`, `billing.state`, `billing.step_up` |
| bot_delivery | — | `bot_delivery.list` |
| bot_relay | — | `bot_relay.deliver`, `bot_relay.outbox.drain`, `bot_relay.reply`, `bot_relay.roster.sync` |
| browser | — | `browser.manage` |
| clarify | — | `clarify.lock` |
| cli | — | `cli.exec` |
| client | `client.capabilities` | — |
| clipboard | — | `clipboard.paste` |
| command | `command.resolve` | `command.dispatch` |
| commands | `commands.catalog` | — |
| complete | `complete.path` | `complete.slash` |
| config | `config.show` | `config.get`, `config.set` |
| connection | — | `connection.respond` |
| connectors | — | `connectors.accounts`, `connectors.accounts.remove`, `connectors.accounts.rename`, `connectors.catalog`, `connectors.connect`, `connectors.list`, `connectors.operation.status`, `connectors.operation.wake`, `connectors.policy.get`, `connectors.policy.set`, `connectors.tools` |
| cron | `cron.manage` | — |
| delegation | — | `delegation.pause`, `delegation.status` |
| diagnostics | — | `diagnostics.share_nous` |
| display | — | `display.install`, `display.lease.acquire`, `display.lease.release`, `display.observe`, `display.start`, `display.status`, `display.stop`, `display.switchSandboxImage`, `display.thumbnail` |
| file | `file.attach` | — |
| free_tier | — | `free_tier.ack_notice`, `free_tier.challenge_result`, `free_tier.claim_nudge`, `free_tier.provision`, `free_tier.status` |
| gateway | `gateway.capabilities` | — |
| groups | — | `groups.approve`, `groups.attachment.download`, `groups.attachment.upload`, `groups.capabilities`, `groups.context`, `groups.continue`, `groups.create`, `groups.demote`, `groups.disband`, `groups.discard`, `groups.list`, `groups.log`, `groups.peer.invite`, `groups.peer.register`, `groups.peer.revoke`, `groups.promote`, `groups.rename`, `groups.replica_state`, `groups.replicate`, `groups.retry`, `groups.send`, `groups.state`, `groups.stop` |
| handoff | — | `handoff.fail`, `handoff.request`, `handoff.state` |
| i18n | `i18n.languages` | `i18n.catalog` |
| image | — | `image.attach`, `image.attach_bytes`, `image.detach`, `image.generate`, `image.render` |
| input | — | `input.detect_drop` |
| insights | `insights.get` | — |
| learning | — | `learning.delete`, `learning.detail`, `learning.edit`, `learning.frames` |
| llm | `llm.oneshot` | — |
| machine | — | `machine.facts` |
| mcp | `mcp.servers.list` | `mcp.catalog`, `mcp.servers.add`, `mcp.servers.oauth.callback`, `mcp.servers.oauth.cancel`, `mcp.servers.oauth.poll`, `mcp.servers.oauth.start`, `mcp.servers.remove`, `mcp.servers.set_api_key`, `mcp.servers.status`, `mcp.servers.test` |
| message | — | `message.react` |
| model | `model.options` | `model.disconnect`, `model.save_key` |
| onboarding | — | `onboarding.set_run`, `onboarding.skip`, `onboarding.start`, `onboarding.state` |
| paste | — | `paste.collapse` |
| pdf | — | `pdf.attach` |
| pet | — | `pet.cancel`, `pet.cells`, `pet.disable`, `pet.export`, `pet.gallery`, `pet.generate`, `pet.generate.status`, `pet.hatch`, `pet.info`, `pet.info.meta`, `pet.remove`, `pet.rename`, `pet.scale`, `pet.select`, `pet.thumb` |
| ping | `ping` | — |
| plugins | `plugins.list` | `plugins.catalog.lookup`, `plugins.manage` |
| preview | — | `preview.restart` |
| process | `process.kill`, `process.list`, `process.stop` | — |
| profiles | `profiles.create`, `profiles.describe`, `profiles.list` | `profiles.configure`, `profiles.get_asset`, `profiles.set_asset` |
| project | — | `project.facts` |
| projects | `projects.create`, `projects.for_cwd`, `projects.get`, `projects.list`, `projects.set_active`, `projects.update` | `projects.add_folder`, `projects.archive`, `projects.delete`, `projects.discover_repos`, `projects.project_sessions`, `projects.record_repos`, `projects.remove_folder`, `projects.set_primary`, `projects.tree` |
| prompt | `prompt.submit` | `prompt.background`, `prompt.btw` |
| reload | `reload.mcp` | `reload.env` |
| request | — | `request.answer` |
| rollback | `rollback.diff`, `rollback.list`, `rollback.restore` | — |
| session | `session.activate`, `session.active_list`, `session.archive`, `session.close`, `session.compress`, `session.context_breakdown`, `session.create`, `session.delete`, `session.events.since`, `session.history`, `session.interrupt`, `session.list`, `session.most_recent`, `session.resume`, `session.status`, `session.title`, `session.usage` | `session.branch`, `session.branch_stored`, `session.branch_whole`, `session.control`, `session.control.read`, `session.cwd.set`, `session.events.stats`, `session.foreign.import`, `session.foreign.list`, `session.foreign.preview`, `session.redirect`, `session.save`, `session.set_hidden`, `session.steer`, `session.undo`, `session.workspace.move` |
| setup | `setup.runtime_check`, `setup.status` | — |
| shared_metrics | — | `shared_metrics.desktop_daily`, `shared_metrics.desktop_dislike`, `shared_metrics.desktop_feature_use`, `shared_metrics.desktop_friction`, `shared_metrics.desktop_onboarding`, `shared_metrics.set`, `shared_metrics.slash_command`, `shared_metrics.startup_latency`, `shared_metrics.status`, `shared_metrics.update_run` |
| shell | `shell.exec` | — |
| skills | `skills.reload` | `skills.manage` |
| slash | `slash.exec` | — |
| spawn_tree | — | `spawn_tree.list`, `spawn_tree.load`, `spawn_tree.save` |
| subagent | — | `subagent.interrupt`, `subagent.list`, `subagent.roster`, `subagent.steer`, `subagent.tail` |
| subscription | — | `subscription.change`, `subscription.preview`, `subscription.resume`, `subscription.state`, `subscription.upgrade` |
| system | `system.battery` | — |
| terminal | — | `terminal.resize` |
| tools | `tools.list` | `tools.configure`, `tools.show` |
| toolsets | `toolsets.list` | — |
| usage | — | `usage.bars` |
| vault | — | `vault.add`, `vault.list`, `vault.lock`, `vault.preview.fill`, `vault.remove`, `vault.source.set`, `vault.sources`, `vault.unlock` |
| verification | — | `verification.status` |
| voice | — | `voice.record`, `voice.toggle`, `voice.tts` |
| wake | — | `wake.feed`, `wake.pause`, `wake.resume`, `wake.start`, `wake.status`, `wake.stop` |

Six more advertised names are declared only under `x-canonical-methods`: `prompt.cancel`, `prompt.receipt`, `prompt.resolve_unknown`, `runtime.describe`, `session.info`, `session.mutate`.

Rust also advertises `catalog.archive_inactive`, `catalog.bot.delete`, `catalog.session.delete`, `catalog.session.set`, `cron.chat`, `cron.trigger`, `gateway.ping`, `memory.provider.set`, `memory.providers`, `plugins.memory.archive_search`, `plugins.memory.sync`, `prompt.resume_parked`, `rpc.discover`, `telegram.connect`, `telegram.disconnect`, `telegram.send`, `telegram.status`. These 17 are outside the published file.

`commands.catalog`, `command.resolve` and `slash.exec` cover commands that plugins register, not the stock built-ins. Usage and context figures are estimates. `i18n.languages` reports English, the app's only language.
