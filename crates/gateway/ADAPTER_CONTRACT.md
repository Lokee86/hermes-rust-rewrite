# Gateway adapter contract v1

Mercury's gateway owns the lifetime of a channel conversation from the first authenticated event to the last recorded delivery. An adapter connects to a platform and translates between that platform and the gateway. Implement this contract and pass the core conformance tests before starting a production adapter. Complete one bundled adapter's platform acceptance before starting the next.

The contract covers the native Mercury host boundary. Hermes v0.23.4 supplies the reference behavior for the platform registry, normalized messages, session scoping, and delivery. [Hermes platform registry](https://github.com/NousResearch/hermes-agent/blob/v0.23.4/gateway/platform_registry.py) · [Hermes message events](https://github.com/NousResearch/hermes-agent/blob/v0.23.4/gateway/platforms/event.py) · [Hermes gateway behavior](https://github.com/NousResearch/hermes-agent/blob/v0.23.4/website/docs/developer-guide/gateway-internals.md)

## Existing code and target ownership

Mercury already has one profile gateway owner, native session admission receipts, and a conversation writer that commits turn history and authority together. Routines and signed webhooks use the gateway's internal admission and receipt paths. The current Telegram host supervises polling and replies and maintains its own inbox and cursor. Give adapters a common gateway entry point so that a second platform does not need its own admission, authorization, or recovery implementation. [Gateway authority](src/authority.rs) · [Routine producer](src/scheduler.rs) · [Webhook producer](src/webhooks.rs) · [Telegram host](src/telegram_host.rs)

| Owner | Responsibility |
| --- | --- |
| Profile gateway | Adapter registry and connection leases, verified profile binding, policy and capability checks, conversation routing, durable ingress and checkpoints, and the delivery outbox. |
| Native session authority | Queue order, execution claims and generations, interactive controls, approval and clarification state, committed history and receipts, and recovery of uncertain execution. |
| Adapter | Platform credential verification, network lifecycle, conversion of platform events and identities, formatting and delivery limits, and evidence from platform requests. |
| Engine | Provider and tool execution under the settings and permissions supplied by the gateway. |

Put the registry, ingress processing, and outbound coordinator behind a messaging module in `hermes-gateway`. Adapters call a connection-scoped ingress interface. The messaging module uses internal session admission and control methods and publishes results from committed receipts. Routines and webhooks submit outbound intents through the same coordinator. Store messaging records in the owning profile's `mercury.db`, using the existing profile database and conversation writer. Adapters use scoped host operations for staging attachments and reading reply artifacts. [Profile database](../engine/src/db.rs) · [Attachment handling](src/upload.rs)

## Interface

The gateway validates contract major version, adapter identity, configuration, and advertised capabilities before activating a connection. Version 1 requires text intake and text delivery. Additional capabilities declare support for attachments, topics or threads, interactive prompts, and progress or message editing. A capability request without support produces a typed refusal, with any supported fallback chosen by gateway policy.

| Operation | Contract |
| --- | --- |
| Register and connect | An adapter supplies its platform identifier, version, capabilities, and connection factory. The gateway binds the configured adapter to one profile, verifies the platform account, and grants a connection lease. The adapter reports readiness, retryable disconnects, or a configuration or credential failure. |
| Submit an event | The adapter presents an authenticated platform event, its replay identity, and an optional resume checkpoint. A durable result identifies acceptance, a repeat of a prior result, refusal, or conflicting reuse, and returns the stored checkpoint when one is in use. A failed commit is an explicit request to retry or resume from the last durable checkpoint. |
| Prepare a delivery | The gateway supplies a committed, authorized intent. The adapter returns an ordered, bounded plan of platform operations without performing network effects. The gateway records the plan and stable part identities before the first attempt. |
| Send a part | The adapter performs one platform operation for one leased attempt and returns classified evidence. The gateway records the result and uses it to decide whether and when another attempt can start. |
| Stop or reconnect | The gateway stops new intake and delivery claims, drains or cancels in-flight operations, and revokes the connection generation. A new connection uses the persisted checkpoint, active routes, and receipts. Stale connection generations cannot admit updates or settle attempts. |

Expose health and receipt status through the native profile owner, with the relevant profile, connection, conversation, and attempt identifiers. Credential values and message bodies stay out of operational status and diagnostics.

## Identity and policy

The gateway assigns the profile and connection scope after verifying the external account. It grants one active intake lease per platform account within an installation. Platform IDs are opaque values. Labels, usernames, and message-supplied profile names have no authority.

An inbound event carries the authenticated account's chat ID, the sender ID and supported role evidence, the thread or topic ID when present, and an event ID unique within the platform's replay scope. It also carries normalized content or a typed control action. The gateway combines profile, connection, chat, thread, and any policy-selected participant scope into a conversation route. The route has a stable session ID in the owning profile. Use a deterministic creation key to recover a session created just before an interrupted route write. Report a lost mapped session as blocked and require an explicit owner decision before creating a replacement.

Room policy determines whether a participant can submit work, whether a room shares its session, and which mentions, commands, tools, and controls are allowed. Recheck the current connection and applicable policy at intake, before claiming a turn or executing a control, and before each external delivery attempt. A credential change to another platform account creates a new connection identity. Pending work continues to refer to its original identity and destination.

Approval and clarification prompts carry gateway-issued action identities bound to the intended actor or role, conversation, pending request, and execution generation. Repeated and stale responses return their recorded decision or a typed refusal. Queue edits, steering, redirects, interruption, and prompt responses all use the same native authority as local clients. Media and remote text pass through the gateway's data and attachment policy before use by the engine.

## Intake, ordering, and execution recovery

1. Authenticate the transport and normalize the event. Record the policy revision and the minimal evidence needed to identify and replay its disposition.
2. Commit the accepted input or durable refusal, along with any safe cursor advancement. A batch checkpoint advances only through the durably handled prefix. Polling and push adapters follow their platform's acknowledgement and resume rules. A transport that cannot replay an observed gap reports it in connection status.
3. Derive the native submission key from the connection and event identities. Resolve or create the routed session and persist one native receipt for that submission. A repeated identity with equal normalized input reads the original disposition. Conflicting reuse preserves the original and returns a conflict.
4. Claim the queued turn under the current owner and policy. Persist the claim before starting the engine. Use the native turn boundary to commit history and the terminal result together.

The route orders ordinary inputs and replies. Authorized controls for a running turn can reach the authority while that turn is awaiting an answer. An uncertain execution fences further ordinary turns in its route until the owning authority resolves it. Other routes remain eligible for work. A restart reconciles durable intake, queued receipts, and committed results. A claim with an uncertain execution outcome enters the existing native unknown-resolution path. Retain deduplication identities through the adapter's supported replay window and for all unsettled work.

## Delivery and recovery

A reply intent references a committed turn result or a durable, authorized scheduler or webhook notification. The intent records the approved destination, any conversational reply anchor, and stable delivery identity. The gateway validates the rendered plan against the authorized destination and records each part with its own durable attempt and platform receipt. Parts for the same destination are sent in order. Progress updates can use negotiated display operations tied to the same intent. The committed outcome determines the final reply.

| Result | Gateway action |
| --- | --- |
| Confirmed | Commit the platform receipt and mark the part delivered. A restarted owner resumes from the next unfinished part. |
| Retryable with evidence of non-acceptance | Persist the retry deadline and its scope. Retry the same part after that deadline under current policy. An explicit platform rate limit is one such result. |
| Definitively rejected | Record the terminal failure and make it visible to the operator. |
| Uncertain | Hold the part and later deliveries to its destination. A missing response, ambiguous transport failure, or interrupted send claim enters this state. |

Persist each attempt before invoking the adapter. On restart, reconcile incomplete attempts while holding the new owner lease. Positive platform evidence may confirm a delivery. A configured owner can also mark the exact attempt delivered or discard it. Only confirmed non-acceptance permits an automatic retry. Resolving one attempt releases its destination's order fence and leaves other destinations independent.

## Adoption and completion

1. **Prove the core contract.** Register synthetic polling and push adapters through the actual gateway owner. Drive normal native session admission, provider execution, and outbound delivery with a local deterministic provider and disposable profile databases. Verify the native live controls used by adapters through the same session authority.
2. **Adopt one bundled adapter.** Move its routing, checkpoints, admission, and delivery directly onto the tested gateway path. Where Mercury already persists native platform data, migrate it once under its current owner lock before handing over the lease. Preserve valid settings, inbox history, and the acknowledgement position. Historical inbox entries stay visible without enqueueing agent turns. At integration, the gateway has one live write path for that adapter's state.
3. **Complete platform acceptance.** Verify the selected adapter's supported workflow matrix with transport fixtures and a configured bot. Complete that adapter's live acceptance before starting another. [Discord](DISCORD_ADAPTER_SPEC.md) is the first production adapter. The [backend roadmap](../../BACKEND_PARITY_PLAN.md) records the platform targets.

Use the loopback gateway, native owner checks, and scheduler and webhook tests as prior art. The conformance suite must reproduce these cases through a running gateway and reopened profile storage. [Native session tests](tests/websocket.rs) · [Owner tests](tests/owner.rs)

| Acceptance case | Required evidence |
| --- | --- |
| Profile and route isolation | A DM, a thread, and a conversation under a second profile resolve to their intended sessions. Stale connection generations and forged cross-profile controls are refused. |
| Intake and retry | Send an accepted event, an exact duplicate, a conflicting duplicate, and a forbidden input. Verify the durable dispositions, one admission for the accepted event, a resume checkpoint that never passes uncommitted input, and no admission for the forbidden input. |
| Native execution | Reopen after intake, queueing, execution claim, and result commit. Verify recovery of safe work, an explicit uncertain execution fence, and a reply based on the committed result. Confirm approvals and clarification actions require the matching request and generation. |
| External delivery | Reopen before and after claim or confirmation. Test an explicit retryable rate limit, definitive rejection, a lost acknowledgement, and operator resolution. Verify ordering of multi-part replies and continued progress in an unrelated conversation. |
| Revocation and producer routing | Remove a user's access or change an account during queued work and before delivery. Verify the current policy applies. Send a scheduled and a webhook notification through the authorized destination using the same delivery receipts. |
| Capabilities and lifecycle | Verify registration, health reporting, lease take-over, and structured refusals for missing capabilities. Exercise at least two intake transport shapes without adapter-specific rules in the gateway authority. |

The core gate requires the conformance suite and `cargo test --locked --workspace` to pass. Each production adapter also has its own transport and live acceptance gate.
