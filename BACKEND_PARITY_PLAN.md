# Mercury backend parity plan

Mercury is a native Rust implementation of the workflows Hermes is moving toward. The immediate goal is a gateway and agent runtime that can sustain interactive work, messaging, and unattended jobs across client disconnects and gateway restarts. Keeping that behavior in the backend lets contributors make progress before the next Hermes Desktop UI ships.

Mercury already has durable turns and approvals, local terminals, routines, native storage, and an embedded plugin host. Its Telegram adapter polls and sends messages today. Routing an inbound Telegram message through the existing gateway and delivering the resulting reply is the first implementation slice. It exercises the same ownership and recovery rules that later clients and integrations will need. [Gateway implementation](crates/gateway/README.md) · [Telegram host](crates/gateway/src/telegram_host.rs)

## Scope

- Compare user-visible behavior with current, supported Hermes workflows. Hermes v0.23.4 is the starting reference. Use upstream migration documentation to identify the supported successor when both old and new paths still exist. [Hermes v0.23.4](https://github.com/NousResearch/hermes-agent/releases/tag/v0.23.4) · [Hermes gateway internals](https://github.com/NousResearch/hermes-agent/blob/93dd2a4e/website/docs/developer-guide/gateway-internals.md)
- Leave legacy compatibility outside the work. This includes standalone `hermes serve` and dashboard startup compatibility, legacy socket and attachment fallbacks, deprecated plugin and storage hooks, the standalone Kanban daemon, and shared writes to Hermes databases. The parity inventory records those differences for reference. [Mercury parity inventory](API_PARITY_REPORT.md) · [Upstream backend migration](https://github.com/NousResearch/hermes-agent/blob/93dd2a4e/website/docs/user-guide/features/web-dashboard.md#L552)
- Defer Desktop UI changes until the new Hermes UI ships. The current Desktop can exercise existing backend flows. Programmatic clients and the gateway-hosted browser workflow remain part of the project.

## Backend ownership

The gateway authorizes requests, resolves their profile and session, admits turns, coordinates approvals, and publishes results and events. The engine runs the model, tools, and provider calls. Mercury's library persists profiles, projects, history, and execution state in its own stores. Clients and adapters call through the gateway and can reconnect to the same work. [Gateway](crates/gateway/README.md) · [Library](crates/library/README.md)

Use a single authoritative writer per session and scope credentials, settings, tools, and workspaces to the owning profile. An admitted request has a stable identity. A retry of the same request returns the recorded outcome or the current execution state. A conflicting retry is rejected. If a restart makes an external effect uncertain, preserve that uncertainty for reconciliation before repeating the effect. Use the existing admission and recovery code as the starting point. [Turn admission](crates/gateway/src/authority.rs) · [Gateway tests](crates/gateway/tests/websocket.rs)

## Workstreams and dependencies

| Workstream | Prerequisite | Completion check |
| --- | --- | --- |
| **Gateway foundation** | Start here. Use the existing native owner, admission records, and event replay. | A headless fixture creates and resumes a session, retries submission after a lost response, recovers queued work after restart, and rejects controls for the wrong profile or execution generation. |
| **Telegram agent workflow** | Gateway foundation for profile routing, admission, and recovery. | One authorized inbound message reaches one conversation, runs the agent, and produces a delivered or explicitly unresolved reply. Tests exercise redelivery and restart. |
| **Agent runtime and tools** | Start with existing engine fixtures. Gateway foundation for controls that touch live sessions. | A model can complete representative coding and document tasks with the appropriate tools. A user can clarify, steer, or stop active work through the gateway. |
| **Profiles and remote access** | Gateway foundation for authorization, discovery, and durable session identity. | A client can select multiple profiles through a host connection and resume work over an authenticated remote connection. Cross-profile requests are refused. |
| **Durable unattended work** | Gateway foundation and session controls. Channel delivery uses the Telegram workflow's delivery path. | Workers, scheduled runs, and persistent goals expose their outcomes and recover safely after restart. Local automation can land independently of messaging adapters. |
| **Additional messaging platforms** | Telegram agent workflow. Profiles and remote access for shared-host deployments. | A second adapter proves the common message and delivery behavior. Subsequent adapters use the same fixtures for the capabilities they expose. |
| **MCP and plugin expansion** | Existing plugin and MCP hosts. Agent runtime and profile controls for capabilities that execute work or ask users for input. | A plugin or server can exercise its supported capability under the correct profile policy, with explicit errors for unavailable capabilities. |
| **CLI, editor, and API clients** | Gateway foundation. Profiles and remote access for deployed clients. | Current terminal, editor, and API workflows use the same durable session behavior. A local CLI driver can move ahead while remote access is being built. |
| **Gateway-hosted Webapp** | Profiles and remote access, plus the shared session and settings contracts used by programmatic clients. | A signed-in browser user selects a profile, resumes a conversation, and sees live work and pending approvals through a reconnect. |
| **Profile portability and release operations** | Stable storage contracts for import and restore. Gateway foundation for service lifecycle. Work on packaging and migration fixtures can start early. | A clean install can start the gateway, import a current profile, run work, and upgrade with its saved state intact. |
| **New Desktop UI** | The new upstream UI ships. The gateway capabilities the UI needs are exercised through independent clients. | The port completes representative workflows over the native gateway, including approval, reconnect, terminal reattach, and profile switching. |

## First implementation slice

1. Extend the existing loopback fixtures around durable admission, approval, history, and replay where the Telegram workflow needs coverage. Use a deterministic provider and disposable profiles. Establish the input identity, profile and conversation mapping, and the conditions under which a retry may proceed. [Existing transport fixtures](crates/gateway/tests/websocket.rs)
2. Route authorized Telegram updates into the owning profile and session through the normal admission path. Record sufficient input state before advancing the polling cursor so a restart can recover the update. Keep the Telegram adapter responsible for polling, normalization, and sending. [Telegram polling](crates/gateway/src/telegram_host.rs) · [Existing admission](crates/gateway/src/authority.rs)
3. Persist delivery intent and record the outcome of sending the committed response. Exercise a duplicate update, a restart after admission, and a restart after turn completion. When a send has an uncertain outcome, make that state visible and reconcile it before retrying. Keep unattended runs subject to the profile's tool and approval policy.
4. Run the full path with the desktop process absent. Verify that a permitted update produces one committed turn, an unauthorized update produces no admission, and a repeated update preserves the original session and receipt. This is the first integration gate for channel work.

## Agent capabilities and session controls

Extend the engine in task-sized increments. Start with automatic project instructions, patch editing, richer search, and document extraction. Build selectable execution environments and background process notifications on top of the existing terminal and process ownership. Exercise workspace restrictions, targeted approvals, and rollback with real fixtures. [Engine tools](crates/engine/src/tools.rs) · [Process controls](crates/engine/src/processes.rs) · [Hermes tools and toolsets](https://github.com/NousResearch/hermes-agent/blob/v0.23.4/website/docs/user-guide/features/tools.md)

Give projects multiple workspace folders, repository discovery, and managed worktrees. Bind the selected workspace and its instructions to the session so a client switch or reconnect preserves the agent's execution context.

Add model-facing web and media tools and browser or computer-use capabilities through the same policy boundaries. Expand provider coverage, account routing, failover, credential rotation, and auxiliary inference incrementally. A fallback must preserve the user's routing and credential policy for the request.

Implement clarification, steering, queued-input edits, and session branching at the gateway where they apply to running or committed work. Add content search and export through Mercury's own history interfaces. Exercise live controls against the intended session and execution generation, including a reconnect during approval and an attempted stale control. [Hermes native session behavior](https://github.com/NousResearch/hermes-agent/blob/93dd2a4e/website/docs/developer-guide/gateway-local-sessions.md)

## Profiles, delegation, and messaging

Finish the shared-host profile experience before promising cross-profile operations. A connection must be able to discover and select the profiles it is allowed to use. Remote and SSH clients attach to the owner and receive scoped access. The internal process layout can follow Mercury's needs as long as a profile's data and policy stay isolated. [Hermes profile multiplexing](https://github.com/NousResearch/hermes-agent/blob/93dd2a4e/website/docs/developer-guide/multiplexing-gateway.md) · [Hermes SSH client design](https://github.com/NousResearch/hermes-agent/blob/93dd2a4e/website/docs/developer-guide/gateway-ssh-clients.md)

Build durable child runs, continuation, persistent goals and loops, skill-backed routines, and Kanban workers on the existing admission and recovery model. Each producer submits work under an identified profile and records its result independently of the submitting client. Test restart and cancellation at the boundary where a queued job becomes a running turn. Bring channel delivery and cross-host group collaboration in as the shared routing and delivery paths mature. [Mercury scheduler](crates/gateway/src/scheduler.rs) · [Hermes delegation](https://github.com/NousResearch/hermes-agent/blob/v0.23.4/website/docs/user-guide/features/delegation.md) · [Hermes gateway-owned Kanban](https://github.com/NousResearch/hermes-agent/blob/v0.23.4/website/docs/user-guide/features/kanban.md#gateway-embedded-dispatcher-default)

## Extensions, clients, and operations

Expand the published plugin SDK and Mercury's MCP management as individual capabilities become usable. Priorities include configuring multiple servers, filtering tools, exposing resources and prompts, and routing sampling or clarification through the owning session. Add provider, terminal, context, and delegation extensions at their existing engine boundaries. Skills and memory work includes agent-managed skills and reviewable curation, using the existing memory and skill stores. [Plugin host](crates/python-host) · [MCP client](crates/engine/src/mcp.rs) · [Hermes extension guide](https://github.com/NousResearch/hermes-agent/blob/93dd2a4e/website/docs/developer-guide/plugins/index.md)

Start programmatic access with a small CLI that can submit, follow, and resume a turn. Use it for repeatable backend acceptance. Expand to interactive terminal workflows, ACP editor support, and the inbound OpenAI-compatible API. The Webapp needs scoped sign-in, profile selection, history, settings, and live control over those same sessions. Use public integration protocols where they are the feature users rely on. [Hermes programmatic integration](https://github.com/NousResearch/hermes-agent/blob/v0.23.4/website/docs/developer-guide/programmatic-integration.md) · [Hermes API server](https://github.com/NousResearch/hermes-agent/blob/v0.23.4/website/docs/user-guide/features/api-server.md)

Extend the current importer to support a forward transfer from a populated Hermes profile. Bring over the history, attachment bytes, project links, supported settings, and routine definitions needed to resume work. Credential transfer uses supported mechanisms or asks users to sign in again. Keep schedules disabled during import until the operator activates them, so moving a profile cannot run the same job on both installations. [Current importer](crates/library/src/import.rs) · [Storage parity report](API_PARITY_REPORT.md#storage-and-profile-compatibility)

Ship service startup, shutdown, diagnostics, and upgrades with the same durable state guarantees as a normal gateway restart. Cover supported packages on macOS, Windows, and Linux, including a native Windows ARM64 distribution. Port the new Desktop UI when it ships and validate its workflows against the backend. [Build and packaging workflow](.github/workflows/build.yml) · [Terminal and gateway integration plan](ONE-GATEWAY-HERDR-PLAN.md)

## Contributor verification

A contribution should identify the workflow it advances, its prerequisites, and the observable behavior that proves it works. Small changes can land on their own once their prerequisites are satisfied. Reuse the loopback gateway, deterministic model, storage, and terminal fixtures for normal execution. Add a regression case when a workflow exposes a new failure boundary.

- Exercise the authorized path and the relevant refusal path. For durable work, cover retry and restart at the affected boundary and verify the saved state as well as the live response.
- Mark a capability available in the parity inventory once its workflow has been exercised. Record the deployment requirements and external services for optional integrations.
- Run the affected crate tests for each change. Use the [repository build instructions](README.md#build-from-source) and locked workspace tests at integration gates. For a release, exercise the packaged headless gateway and its supported clients through a representative turn, disconnect, restart, and upgrade on the supported platforms.
