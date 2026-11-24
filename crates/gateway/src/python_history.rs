//! Read-only stock history projected from a profile gateway's committed transcript.

use super::{Authority, Entry, History, SessionSummary, Settings, Status, Value, fs, json};
use anyhow::{Context, Result};
use hermes_engine::memory_capture::{contains_inline_data, redact_source_text};
use hermes_library::SessionSource;

const MAX_ROWS: usize = 128;
const MAX_BYTES: usize = 96 * 1024;

fn epoch_ns(ns: i64) -> f64 {
    ns as f64 / 1_000_000_000.0
}

fn started_ns(summary: &SessionSummary) -> Result<i64> {
    chrono::DateTime::parse_from_rfc3339(&summary.created_at)?
        .timestamp_nanos_opt()
        .filter(|value| *value > 0)
        .context("Native session has no verified creation timestamp")
}

fn source_text(text: &str, protected: &[&str]) -> Option<String> {
    if text.trim().is_empty() || contains_inline_data(text) {
        return None;
    }
    let redacted = redact_source_text(text, protected);
    (redacted.len() <= 32_768).then_some(redacted)
}

/// Return a bounded, chronological suffix ending at the given committed turn.
/// The supplied authority must already be scoped to one provider selection.
pub(super) fn provider_messages(
    history: &History,
    authority: &Authority,
    through: usize,
    protected: &[&str],
) -> Vec<Value> {
    let mut turns = authority
        .receipts
        .iter()
        .filter_map(|receipt| {
            let index = receipt.turn_index.filter(|index| *index <= through)?;
            (receipt.status == "terminal" && receipt.outcome.as_deref() == Some("complete"))
                .then_some((index, receipt))
        })
        .collect::<Vec<_>>();
    turns.sort_unstable_by_key(|(index, _)| *index);
    let mut rows = Vec::new();
    let mut bytes = 2;
    for (index, receipt) in turns.into_iter().rev() {
        let Some(turn) = history.turns.get(index) else {
            continue;
        };
        let Some(committed_ns) = receipt.committed_ns else {
            continue;
        };
        if turn.status != Status::Complete
            || !turn.attachments.is_empty()
            || !turn.context.is_empty()
            || receipt.unattended
            || receipt.group.is_some()
            || turn.prompt != receipt.text
            || receipt.id.is_empty()
            || receipt.generation == 0
            || receipt.admitted_ns <= 0
            || committed_ns < receipt.admitted_ns
        {
            continue;
        }
        let Some(prompt) = source_text(&turn.prompt, protected) else {
            continue;
        };
        let mut turn_rows = vec![json!({
            "role": "user", "content": prompt, "timestamp": epoch_ns(receipt.admitted_ns),
            "message_uid": format!("generation-{}-user", receipt.generation),
        })];
        for (entry_index, entry) in turn.entries.iter().enumerate() {
            if let Entry::Assistant { text, .. } = entry
                && let Some(content) = source_text(text, protected)
            {
                turn_rows.push(json!({
                    "role": "assistant", "content": content, "timestamp": epoch_ns(committed_ns),
                    "message_uid": format!("generation-{}-assistant-{entry_index}", receipt.generation),
                }));
            }
        }
        let turn_bytes = turn_rows
            .iter()
            .map(|row| row.to_string().len() + 1)
            .sum::<usize>();
        if rows.len() + turn_rows.len() > MAX_ROWS || bytes + turn_bytes > MAX_BYTES {
            break;
        }
        rows.extend(turn_rows.into_iter().rev());
        bytes += turn_bytes;
    }
    rows.reverse();
    rows
}

/// Metadata is projected before provider initialization and rebuilt from the same native
/// records before capture delivery. A backup restored as a Native catalog row is still
/// distinguishable from an original session by the library's durable backup-source marker.
pub(super) fn project_session(
    profile_name: &str,
    summary: &SessionSummary,
    settings: &Settings,
    history: &History,
    authority: &Authority,
    protected: &[&str],
) -> Result<Value> {
    let restored = match fs::symlink_metadata(summary.directory.join("backup-source.json")) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("Could not determine native session lineage"),
    };
    let source = match summary.source {
        SessionSource::Native if restored => "recovered",
        SessionSource::Native if summary.bot.is_some() => "subagent",
        SessionSource::Native => settings.source.as_str(),
        SessionSource::LegacyRust => "recovered",
        SessionSource::Hermes { .. } => "import",
    };
    let original = matches!(summary.source, SessionSource::Native)
        && matches!(
            source,
            "gui" | "desktop" | "cli" | "tui" | "acp" | "telegram"
        );
    let birth = started_ns(summary)?;
    let messages = if original && !summary.archived {
        provider_messages(
            history,
            authority,
            history.turns.len().saturating_sub(1),
            protected,
        )
    } else {
        Vec::new()
    };
    // An older draft projection must not replace a newer committed projection.
    let revision = authority
        .receipts
        .iter()
        .filter(|receipt| receipt.status == "terminal" && receipt.committed_ns.is_some())
        .map(|receipt| receipt.generation)
        .max()
        .unwrap_or_default();
    let last_active = summary.last_active.as_deref().and_then(|date| {
        chrono::DateTime::parse_from_rfc3339(date)
            .ok()?
            .timestamp_nanos_opt()
            .map(epoch_ns)
    });
    let expected_rows = history.turns.len() + history.turns.iter().map(|turn| {
        turn.entries.iter().filter(|entry| {
            !matches!(entry, Entry::Assistant { text, calls } if text.trim().is_empty() && calls.is_empty())
        }).count()
    }).sum::<usize>();
    let complete = original && !summary.archived && messages.len() == expected_rows;
    Ok(json!({
        "session": {
            "id": summary.id,
            "profile_name": profile_name,
            "title": redact_source_text(&summary.title, protected),
            "source": source,
            "started_at": epoch_ns(birth),
            "last_active": last_active,
            "hidden": summary.bot.is_some(),
            "archived": summary.archived,
            "provenance_restored": restored,
            "provenance_created_at": original.then_some(epoch_ns(birth)),
            "provenance_root_session_id": original.then_some(&summary.id),
            "parent_session_id": null,
            "model_config": null,
            "message_count": messages.len(),
        },
        "revision": revision,
        "complete": complete,
        "messages": messages,
    }))
}
