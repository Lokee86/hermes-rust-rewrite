# ntfy stock plugin implementation spec

Register ntfy as a bundled Rust adapter with platform key `ntfy`. Start after Twilio SMS and the shared gate. A headless profile can receive text on an approved ntfy topic, reply to its configured audience, and publish routine or manual notifications. Use the same plugin for explicitly publish-only installations. Support hosted and self-hosted servers through the documented HTTP interface. [Shared gateway contract](ADAPTER_CONTRACT.md#shared-requirements-for-stock-plugins) · [Twilio SMS spec](SMS_ADAPTER_SPEC.md) · [Hermes ntfy guide](https://github.com/NousResearch/hermes-agent/blob/v0.23.4/website/docs/user-guide/messaging/ntfy.md) · [Next adapter, Raft](RAFT_ADAPTER_SPEC.md)

## Registration and topic authorization

Put the factory and HTTP client in `hermes-gateway::messaging::ntfy`. Bind the endpoint, inbound topic, publish topic and scoped credential refs to the owner. Support bearer tokens and approved Basic credentials over TLS. Validate topic names and endpoint origin, including deployment paths and private trust. Read-only or write-only credentials serve only the configured operation. Lease intake per verified server and topic. Activate a stream only when inbound is enabled. Verify read and publish readiness separately. A new endpoint or topic cannot inherit a previous route or cursor. [Mercury adapter seam](src/messaging/types.rs) · [ntfy access control](https://docs.ntfy.sh/config/#access-control)

Treat a server/topic as a shared actor principal. Standard ntfy event data has no authenticated individual publisher field. Title, tags, click URLs and embedded data confer no identity. Use server ACLs or a verified reserved topic for trusted two-way deployments. If the owner enables a public topic for low-trust interaction, apply independent restricted tools and nonprivate content policy. Never derive private context or human approval permission from topic possession or a label. Bind a distinct publish topic only with explicit authority for its recipient audience and for moving result content there. [Hermes ntfy identity model](https://github.com/NousResearch/hermes-agent/blob/v0.23.4/website/docs/user-guide/messaging/ntfy.md#identity-model--read-this-before-deploying)

## Streaming intake and recovery

Subscribe to bounded JSON lines using the provider subscription API. Parse `open` and `keepalive` for health. Admit eligible `message` items only in the exact topic. Normalize text and optional displayed metadata, never executing untrusted click actions. Keep ntfy message ID scoped to server and topic as durable identity. Commit disposition or handoff before advancing last committed message ID and its time boundary. On first connection seed or start at an explicit baseline. On reconnect use `since` by provider ID or bounded supported time overlap. Merge cached and live messages by exact ID and retain all valid same-time items. Report observed gaps when caching is disabled, retention expired or identity changed. Back off transient disconnect and rate limit. Treat authentication denial or an unprovisioned topic as an actionable configuration state. [ntfy subscription and replay](https://docs.ntfy.sh/subscribe/api/#fetch-cached-messages)

Prefer separate approved input and output topics when available. For same-topic return traffic use confirmed server message IDs for exact self-echo suppression. Where stream events can race the response, a per-effect unguessable local marker may correlate pending candidate echoes under their verified topic and immutable payload, with bounded deferred handling. A publisher-controlled tag or title cannot prove author or accepted delivery. Prevent recursive output even after restart, and keep new eligible messages distinguishable from late echoes. Route supported scoped text commands to native authority. A topic-only principal cannot answer person-bound approvals. [Hermes ntfy adapter](https://github.com/NousResearch/hermes-agent/blob/v0.23.4/plugins/platforms/ntfy/adapter.py#L231)

## Publish and status

Publish prepared text to the verified bound output or approved home topic through the same outbox. Render plain text or authorized Markdown using the supported `X-Markdown` setting. Apply the server byte limit, using the stock conservative 4 KiB default and Unicode-safe bounded content, with a scoped indication of truncation. Maintain per-topic and per-account quota and backoff. Store the returned ID and topic as acceptance evidence. A timeout or missing response holds the original effect unless exact proof is available. Do not use an ntfy client label or update sequence as a generic idempotency guarantee. Server acceptance and phone notification visibility are separate outcomes. [ntfy publishing and formatting](https://docs.ntfy.sh/publish/) · [Hermes ntfy limitations](https://github.com/NousResearch/hermes-agent/blob/v0.23.4/website/docs/user-guide/messaging/ntfy.md#limits)

## Implementation and acceptance

| Step | Prerequisite | Evidence |
| --- | --- | --- |
| Topic setup and stream | Shared gate and Twilio SMS live acceptance | Scoped credentials and topic identity, durable stream intake and cache repair. |
| Native routing and publish | Verified topic principal | Limited authority, exact echo behavior, Markdown/size policies and publish-only outbox. |
| Live acceptance | Transport and shared conformance | Real ntfy service and phone or browser prove client interaction, restart and proactive output. |

Use scripted ntfy stream and publish endpoints through a running owner. Cover malformed or foreign topics, claimed user names, access errors, disconnect with cached overlap or missing retention, same-topic echo before response, uncertain publish and forbidden output remapping. Run shared conformance and `cargo test --locked --workspace`. Verify a scope-controlled ntfy deployment with real clients, including paired topic directions, same-topic loop protection, blocked publisher or room policy, configured Markdown, restart and publish-only home delivery. Record server cache and ACL settings with the live result. [Hermes ntfy fixtures](https://github.com/NousResearch/hermes-agent/blob/v0.23.4/tests/gateway/test_ntfy_plugin.py)

## Scope

The stock interaction is topic text and notifications. Exclude native attachments, typing or threads, standalone senders, old configuration compatibility and Desktop UI. [Backend parity scope](../../BACKEND_PARITY_PLAN.md#scope)
