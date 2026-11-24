# Hermes Rust · One Gateway and Herdr

The implementation plan and Windows test results are recorded here as of 23 November 2026. The target is one installed Hermes executable. The profile gateway owns chats, tool admission and access to terminals. Iced is a client of that gateway. Herdr Core runs local interactive terminals, and its holder keeps the PTYs alive while clients disconnect or the core restarts.

The first integrated demonstration is a Windows desktop that can completely exit and reopen with both the same chat and the same running shell. The implementation can begin on several fronts before that demonstration. Existing-user cutover follows the durability and compatibility work.

## Baselines and ownership

| Component | Implementation agreement |
|---|---|
| Reference builds | Use the local rewrite at `9f30f70`, stock Python Hermes v0.23.3 at `caafe53` for public behavior, and Herdr Core v0.1.22 at `6e29846` for terminal behavior. Use the existing deterministic model fixture for the chat tests and a real local foreground child for the terminal tests. |
| Gateway | One headless owner per selected profile, one authoritative writer per chat. Own discovery, authentication, project and session routing, FIFO admission, approvals, execution generations, receipts and client-visible history. Preserve upstream local and remote access modes. |
| Terminal service | The gateway issues an opaque Hermes terminal ID and keeps a durable association with the authorized profile, workspace and Core terminal. Record conversation and tool-call identity for agent launches. Resolve ownership for listing, output, attach, input, resize and termination. Keep Core IPC private and the terminal host environment free of provider and MCP credentials. |
| Desktop | Use a persistent gateway client for chat and the authenticated terminal stream for the selected terminal. Keep drafts, window state, tabs, selection, copy/paste and Add to Chat in Iced. A chat can have several terminals. Closing a window releases its views. Explicit chat Stop targets a turn generation. Explicit terminal end targets a terminal ID. |
| Memory | The gateway owns the execution queue. Embedded Reliquary owns memory capture receipts and processing. Bind General or dedicated REL and the authorized private PHY to the committed source. |

## Work that starts together

Agree on the identity fields and lifecycle operations above first. Then run the following work concurrently on one integration branch with short-lived branches as useful. Each row has an independent first result and a named point where it joins another row.

| Work | Independent first result | Integration dependency |
|---|---|---|
| Gateway startup and discovery | Dispatch headless startup before Iced. Two simultaneous launches on one profile converge on one owner. A second profile remains isolated. Discover an existing compatible stock owner through the upstream local pipe or socket. | Supplies a real owner and profile-bound tickets to both Desktop paths. Coordinate the executable dispatch with the Core packaging work once. |
| Chat authority and storage | With the deterministic provider, extend the native record with durable admissions, FIFO position, generation, transcript outcome and final receipt. Exercise exact retry, conflicting payload, overlapping approvals, restart and replay. Prepare populated legacy fixtures for queued prompts, projects, attachments and model settings. | Ordinary Desktop chat can be demonstrated as soon as the owner can create, open and resume it. Existing-user cutover needs the durable record and migration cases. |
| Windows Herdr adapter | Run pinned Core privately and explicitly pass the selected shell and workspace. Launch a shell and foreground child, attach, type, resize, detach and reattach with a fresh first paint. Check both PIDs, current directory and shell-local state. This is the first native smoke test to run. | Backend work can proceed immediately. Exposing it to clients needs gateway profile authorization, terminal-ID mapping and the stock terminal ticket. |
| Desktop and stock Webapp | Migrate the Iced chat client and terminal panel as distinct changes. Exercise chat snapshots and approval IDs using the existing gateway fixture. Exercise terminal repaint, input, resize and detach using the terminal adapter fixture. Match stock Webapp ticket, metadata and byte-stream behavior at `/api/host-terminal`. | One integration owner wires profile selection, open/reopen, pop-out and window close in `desktop/src/main.rs`. Chat replay and terminal repaint use their respective transports. |
| Agent tools and compatibility | Compare stock foreground execution, PTY jobs, pipe-backed background jobs, `process_manage`, `shell.exec` and process controls. Build stock client comparisons for sign-in, profiles, paged history, attachments, tickets, streaming and resume, including the Android-style handshake. | Connect agent launches to durable tool admission and the process registry. Scripted client comparisons run on PC. The official Android build supplies the later device result. |
| Embedded memory and packaging preparation | Prepare owner-routing and idempotent-capture fixtures. Exercise the pinned Core build, Ghostty VT dependency, Windows runtime helpers and executable role selection early. | Connect live Reliquary capture when stable session IDs and atomic turn commits land. Final packaging uses the integrated executable and reconciles the running terminal holder on restart. |

Start at `crates/desktop/src/main.rs`, `local_gateway.rs`, `remote.rs`, `terminal.rs` and `accounts.rs` in the desktop crate. The current panel owns its PTY and kills the child on drop. Account sign-in can send a command immediately after opening a terminal, so the new panel must wait for readiness before delivering it. Gateway routing and the existing two-client tests live in `crates/gateway/src/lib.rs` and `crates/gateway/tests/websocket.rs`. Native history and migration work starts in `crates/engine/src/history.rs` and `crates/desktop/src/queue.rs`.

## First integrated demonstration

Run both halves as soon as each is ready. Their first joint run uses the same profile, project and independently running gateway.

1. A and B open the same saved chat in two desktop windows. A opens a project terminal, sets a shell-local variable and starts an interactive child that prints its PID and waits for input. Record the shell PID, child PID, working directory and terminal ID.
2. A submits a deterministic tool turn. Close A with approval pending. B sees the request, approves it, and receives the result. Check the session ID, one tool effect and one committed result. Close B so every desktop window has exited.
3. Reopen Desktop. Restore the same chat and terminal. Send the waiting child a line, finish the child and inspect the original shell. Verify both process identities, working directory, shell-local state and single chat result. A screen restored from history alone cannot establish process continuity.
4. In separate cases, explicitly stop an active turn, explicitly end a terminal and verify the child exits, and attempt access from a different profile. Check the stock expired-terminal result, foreign-profile denial and continued operation of unrelated work. Repeat terminal reattach through the stock Webapp.

When this plan was written, the Windows Core smoke test had stopped before launch. The verification table below records the subsequent native and integrated runs, including which client surfaces were exercised.

## What follows the first demonstration

**Durability and existing users.** Persist admission before acknowledgement. Commit outcome, receipt and pending memory capture together before broadcasting completion. Reconcile an uncertain external effect before retrying or releasing dependent work. Apply the same principle if terminal launch succeeded but the acknowledgement was lost. Verify legacy IDs, parked prompts, attachment bytes, provider/model choices and project routing across interrupted migration. Check failed provider turns and simultaneous child approvals on real client paths. Retire the direct Desktop runner and remote transcript mirror as their callers move.

**Agent terminals and processes.** Use Core PTYs for interactive local jobs. Provide foreground non-PTY execution with stock shell state, timeout and result semantics. Provide a tracked pipe-backed background process with writable stdin, EOF, bounded incremental output, exit status, ownership and release policy. Exercise raw input versus submitted lines, polling without duplicated output, and separate stdout and stderr where stock provides them. Verify `shell.exec` retains the redacted output tails under its limits. Have an agent launch a PTY and an authorized Desktop viewer attach to that exact process. Recheck a lost launch acknowledgement for duplicate children.

**Remaining gateway producers and memory.** Move groups, routines, scheduler claims, executable webhooks, relevant messaging polling and remaining session mutations onto the gateway. Run a group round and scheduled turn with all windows closed. Drive capture from the durable pending record, retry by source identity, and run semantics in the background. Verify one capture across restart, ordinary chat without automatic retrieval, General and dedicated routing, and denial of foreign private PHY access.

**One executable and recovery.** Ship desktop, headless gateway, terminal-server and PTY-holder roles from the same Hermes executable. Start the proper role before GUI initialization, including during self-relaunch and recovery. Package the required Windows ConPTY DLL and OpenConsole helper. Test gateway restart and Core restart independently with a live child, then distinguish actual process loss from a successful reattach. Exercise Windows console input, Unicode and resize, plus the existing Linux and macOS build matrix. The standalone Core binary is a development fixture for the first proof.

**Client and release evidence.** Run `cargo test --locked --workspace` and the packaged GUI and headless paths. Compare unmodified stock Desktop, CLI/TUI and Webapp with stock Python on their supported workflows, including remote/SSH connections and public error behavior. Report the first PC demonstration, durable gateway, terminal/tool parity, packaged platforms and embedded memory as each passes. On the official Android build, verify sign-in, history, attachments, cross-client approval, background/resume, restart and project routing. Handset access gates the Android result only.

## Verification on Windows, 23 November 2026

The short revisions in the verification sections identify local build checkpoints before the upstream PR was squashed. These results describe the original Windows release checkpoint. A later change removed automatic pre-turn retrieval. The current gateway captures and processes eligible committed turns.

| Area | Observed result |
|---|---|
| Profile owner and chat | Two simultaneous local starts selected one gateway. Separate profiles stayed isolated. Clients shared a durable turn, approval and result. Recovery preserved legacy transcript IDs, attachments, model and project, and fenced uncertain work for explicit resolution. |
| Existing clients | The unmodified Python v0.23.3 gateway client discovered the Rust owner through the private Windows pipe. A second client resumed a pending approval and retried the submission. The deterministic provider produced one tool result in a four-message history. Unmodified Desktop client modules signed in over HTTPS, submitted a turn and resumed the same message IDs. Interactive stock attachment uses `session-authority-v1`. Worker adoption, forwarded-subject tickets, stock implicit-queue and first-receipt parity remain open. |
| Desktop and terminal | Two Iced clients ran their real update handlers against a headless gateway. Both closed. Reopening restored the original chat and terminal with the original shell state, shell PID and foreground child PID. The pinned Webapp browser-terminal module also wrote to a real PTY and replayed it on resume using synthetic browser globals and an injected ticket callback. Its Node loader adds a `.ts` import extension. An expired-bookmark retry after disposal was rejected. Explicit terminal close ended the child. A separate pinned-Core test exercised a lost launch response, gateway restart, Unicode input, a Windows arrow key, resize and Ctrl+C. The desktop handlers ran without rendering. Windows rejected creation of the isolated desktop needed for a rendered-window run. |
| Tools and background work | A tool-approved Core PTY accepted input, replayed output after detach and kept its PID. The gateway denied a foreign chat access. Tracked pipe processes supported incremental output, stdin, EOF and explicit termination. A cancelled turn stopped its ordinary background process, preserved a process launched with `persist_on_release`, and still allowed explicit stop. Group-member jobs remained controllable after a viewer detached. Pipe reattachment after gateway exit, restored PTY release metadata, a distinct agent-close hook, notifications, watches, heartbeat and subagent process handoff remain open. The gateway ran a multi-persona group and a scheduled routine with all clients disconnected. Webhook delivery recovered its status from the durable receipt. Telegram verification used a local synthetic transport. |
| Linked Windows executable | The debug build at `4c1ba54` ran the gateway, Core and holder roles from `Hermes.exe`. The staged executable passed `--licenses`, reattached to the same shell and foreground child through a gateway process restart, and ended the child on explicit close. The two-viewer Iced handler fixture then passed against that packaged executable. The ZIP includes ConPTY, OpenConsole and notices. Its path is `target/windows-proof/Hermes-windows-x86_64-debug-proof.zip` and its SHA-256 is `fdef70c669f6fc2ae767f7af33acd975e52447734069a7d70c7897b0d1db90a3`. The later Windows release results are recorded below. |
| Embedded memory at the original checkpoint | Using pinned Reliquary and local deterministic inference, the gateway captured a project turn once and recalled it from a second chat after restart. General did not retrieve that fact. The test also exercised foreign private-PHY denial and deletion before capture. |
| Composer at `4c1ba54` | The first private `shell.exec` implementation supported a few built-ins, stderr routing and bounded, redacted output. See the later composer comparison below for the expanded command set. The approved agent `terminal` tool has its own execution path. |

The locked workspace suite at runtime revision `4c1ba54` passed 191 tests with 21 ignored. Separate opt-in runs exercised the native Core PTY and live Windows child. The gateway restart test succeeded eight times consecutively after shutdown began waiting for detached WebSocket handlers to finish releasing the profile. The three stock-client drivers are `scripts/verify_stock_gateway_client.py`, `scripts/verify_stock_node_gateway.mjs` and `scripts/verify_stock_browser_terminal.cjs`. Their file headers give the required runtime and arguments.

The official Android app and a handset were unavailable. Real Electron UI, SSH, Linux and macOS execution still need acceptance on their respective clients or hosts.

## Windows composer comparison

At `542ec75`, all 36 live checks matched stock Hermes v0.23.3 (`caafe5389ae5`) on both the standalone gateway and the staged, embedded-Herdr debug `Hermes.exe`. They used stock's private Python client and exercised stdout, stderr, exit status, timeout, session state, authorization and backend selection.

An explicit WebSocket keepalive during an active 30-second capture reproduced a disconnect against `6bd2f37`. The Rust process was still alive. At `542ec75` each gateway answered the keepalive before the command timed out. A separate loopback regression confirmed that the JSON heartbeat also received a reply during capture and that a burst of ordinary requests completed in order.

The locked offline workspace suite passed 223 tests with 21 ignored on rerun. On the first run, the private catalog test completed its assertions and saw Windows pipe error 232 during owner shutdown. It passed 12 consecutive isolated runs and the full rerun. The shutdown race remains unconfirmed.

The local proof executable is `target/composer-herdr-acceptance-542ec7517f84/Hermes/Hermes.exe` (SHA-256 `cc2f6663ee261b43f7016618ec3d47d1914a8f1da72729ed7d04334a0f249ffc`). Its 36-case report is beside the staged directory. The standalone RPC, classifier and Ink reports are under `target/composer-package` with suffix `542ec75.json`.

The native Rust classifier matched all 82 Windows inputs, including 78 commands that both classifiers accepted and four that both refused. This comparison evaluated decisions without executing the inputs. Separate Windows execution tests cover scoped Git reports, file utilities and isolated pip version queries. The [gateway README](crates/gateway/README.md) describes their supported forms and limits.

The local chat directory is the starting directory for ordinary reads, listings and input redirection. Both relative and absolute paths to a file one directory above the chat returned its exact contents. A missing file returned the stock Windows diagnostic and exit code. Six checks used the pinned stock Ink composer against Rust. A direct `!type` displayed the parent file without contacting the model. The same read inside `{!…}` reached the fixture model in the submitted request. A refused interpolation preserved the draft.

The opt-in build workflow runs the live shell and classifier comparisons on Windows, Linux and macOS. The local run above covers Windows. Reproduce it with `scripts/verify_stock_composer_shell.py`, `scripts/compare_composer_classifiers.py` and `scripts/verify_stock_ink_composer.mjs`.

## Windows x64 release verification

Built `3d770ee` with `cargo build --locked --release --features embedded-herdr -p hermes-desktop`. Local PowerShell policy blocked the packaging script. The archive was staged with the same file layout from the optimized executable and the checksum-verified Herdr 0.1.22 cache. All 13 files survived ZIP extraction with matching hashes. The extracted executable passed `--licenses`.

The gateway restart and Core restart were tested separately on this release. In the first run the packaged gateway restarted and reattached to the original shell and foreground child. In the second run only the Core PID changed. A fresh authenticated attachment recovered the same terminal ID, holder, shell and foreground child. New input reached the original child, the shell retained its environment and working directory, and explicit close reaped the shell.

Two Iced desktop clients exercised their normal update handlers against the packaged gateway, including tool approval, closing both clients, reopening and recovering the chat and live terminal. The pinned Python client independently completed an approval, reconnected and recovered a four-message history. The six stock Ink composer checks also passed. These fixtures exercised the desktop handlers and client modules without rendering a GUI.

The first 36-case composer comparison ran alongside other tests and matched on 35 observations. The Rust timeout case returned a Windows Ctrl+C exit and partial output. Two consecutive isolated runs each matched all 36 stock results, including `command_timeout` and a WebSocket pong while the command was running. The cause of the first result remains open.

The ZIP is `target/release-package-3d770ee17f50-offline/Hermes-windows-AMD64.zip` with SHA-256 `73e5acf37b7a2086c0adb5171e3845d8072e0b6a6b8ea239a3c73f55dfa14063`. The extracted `Hermes.exe` SHA-256 is `fc751669a709f2f70b44608018afe9efb9d1c99c4d4a6538ad3e18ad7b03d38a`.

## Source baselines

- [Rust rewrite at the cloned revision](https://github.com/itsflownium/hermes-rust-rewrite/tree/9f30f70f6b0c5b17b51960a365c0302f94992ba4)
- [Stock Hermes gateway contract](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/apps/shared/src/gateway-contract.openrpc.json) · [Stock host-terminal persistence test](https://github.com/NousResearch/hermes-agent/blob/caafe5389ae585ef77387a962aee05b1bcf17029/tests/hermes_cli/test_web_host_terminal_persistence.py)
- [Herdr Core at the selected revision](https://github.com/herdrdev/herdr-core/tree/6e2984629fcd600b0165f69b2e5b0bda5985ec1c) · [Terminal protocol](https://github.com/herdrdev/herdr-core/blob/6e2984629fcd600b0165f69b2e5b0bda5985ec1c/schema/README.md)
