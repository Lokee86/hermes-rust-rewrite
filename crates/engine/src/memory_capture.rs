//! Evidence for replayable memory capture from an authoritative, committed turn.
use crate::{Entry, Status, Turn, privacy};
use anyhow::{Result, ensure};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeSet;

// Reserve space for normalized source and receipt metadata in Core's batch.
const MAX_EVIDENCE_BYTES: usize = 224 * 1024;

/// Scrub text before it leaves the authoritative transcript for a memory provider.
pub fn redact_source_text(text: &str, protected: &[&str]) -> String {
    privacy::redact_capture(text, protected)
}

/// Frozen by the gateway when an enrolled, ordinary chat is created. These are
/// owner identities, never paths or request-supplied destinations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureBinding {
    pub profile_id: String,
    pub principal_id: String,
    pub project_id: Option<String>,
    pub rel_id: String,
    pub phy_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureReceipt {
    pub source_sha256: [u8; 32],
    pub import_id: String,
    pub receipt_digest: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingMemoryCapture {
    pub binding: CaptureBinding,
    pub session_id: String,
    pub admission_id: String,
    pub generation: u64,
    pub turn_index: usize,
    pub admitted_ns: i64,
    pub committed_ns: i64,
    pub evidence_sha256: [u8; 32],
    /// False identifies receipts written before assistant messages were copied by entry.
    #[serde(default)]
    pub archive_by_entry: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    reasoning_summaries: Vec<CapturedReasoningSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<CaptureReceipt>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub settled: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct CapturedText {
    pub message_uid: String,
    pub role: &'static str,
    pub timestamp_ns: i64,
    pub content: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub commentary: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct CapturedEcho {
    pub message_uid: String,
    pub sequence: usize,
    pub model_round: usize,
    pub kind: CapturedEchoKind,
    pub correlation_id: String,
    pub name: String,
    pub content: String,
    pub succeeded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum CapturedEchoKind {
    ModelRoundStarted,
    ModelRoundCompleted,
    ToolCall,
    ToolResult,
    ReasoningSummary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturedReasoningSummary {
    pub entry_index: usize,
    pub text: String,
}

impl PendingMemoryCapture {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        binding: &CaptureBinding,
        session_id: &str,
        admission_id: &str,
        generation: u64,
        turn_index: usize,
        admitted_ns: i64,
        committed_ns: i64,
        turn: &Turn,
    ) -> Result<Self> {
        ensure!(
            generation > 0 && !admission_id.is_empty() && !session_id.is_empty(),
            "memory capture requires a durable turn identity"
        );
        ensure!(
            admitted_ns > 0 && committed_ns >= admitted_ns,
            "invalid capture timestamps"
        );
        ensure!(
            !matches!(turn.status, Status::Running),
            "capture requires a committed turn"
        );
        let mut capture = Self {
            binding: binding.clone(),
            session_id: session_id.into(),
            admission_id: admission_id.into(),
            generation,
            turn_index,
            admitted_ns,
            committed_ns,
            evidence_sha256: [0; 32],
            archive_by_entry: true,
            reasoning_summaries: Vec::new(),
            receipt: None,
            settled: false,
        };
        let rows = capture.rows(turn);
        Self::validate_rows(&rows)?;
        ensure!(
            serde_json::to_vec(&(&rows, capture.echo(turn)))?.len() <= MAX_EVIDENCE_BYTES,
            "memory capture exceeds the combined Archive and Echo byte budget"
        );
        capture.evidence_sha256 = capture.fingerprint(turn, &rows)?;
        Ok(capture)
    }

    pub fn with_reasoning_summaries(
        mut self,
        turn: &Turn,
        summaries: Vec<CapturedReasoningSummary>,
    ) -> Result<Self> {
        self.reasoning_summaries = summaries;
        let rows = self.rows(turn);
        if serde_json::to_vec(&(&rows, self.echo(turn)))?.len() > MAX_EVIDENCE_BYTES {
            self.reasoning_summaries.clear();
        }
        self.evidence_sha256 = self.fingerprint(turn, &rows)?;
        Ok(self)
    }

    /// Rebuild source from the authoritative native record. A changed row or
    /// changed owner binding can never be acknowledged as the original capture.
    pub fn verified_rows(&self, turn: &Turn) -> Result<Vec<CapturedText>> {
        let rows = self.rows(turn);
        ensure!(
            self.evidence_sha256 == self.fingerprint(turn, &rows)?,
            "memory source no longer matches the committed capture"
        );
        Self::validate_rows(&rows)?;
        Ok(rows)
    }

    pub fn verified_echo(&self, turn: &Turn) -> Result<Vec<CapturedEcho>> {
        self.verified_rows(turn)?;
        Ok(self.echo(turn))
    }

    fn echo(&self, turn: &Turn) -> Vec<CapturedEcho> {
        if !self.archive_by_entry {
            return Vec::new();
        }
        let mut next_text = vec![None; turn.entries.len()];
        let mut next = None;
        for (index, entry) in turn.entries.iter().enumerate().rev() {
            if matches!(entry, Entry::Assistant { text, .. } if !text.trim().is_empty()) {
                next = Some(index);
            }
            next_text[index] = next;
        }
        let mut round = 0;
        let mut anchor = None;
        let mut events = Vec::new();
        let mut previously_archived = None;
        let mut captured_calls = BTreeSet::new();
        let mut completed = false;
        let mut remaining_bytes = 96 * 1024;
        let mut add = |message: Option<usize>,
                       model_round,
                       kind,
                       id: &str,
                       name: &str,
                       content: &str,
                       succeeded| {
            if events.len() >= 96 {
                return false;
            }
            let mut content = if contains_inline_data(content) {
                "[inline data omitted]".into()
            } else {
                privacy::redact_capture(content, &[])
            };
            let limit = remaining_bytes.min(8 * 1024);
            if content.len() > limit {
                content.truncate(content.floor_char_boundary(limit));
            }
            remaining_bytes -= content.len();
            events.push(CapturedEcho {
                message_uid: match message {
                    Some(index) => format!("generation-{}-assistant-{index}", self.generation),
                    None => format!("generation-{}-user", self.generation),
                },
                sequence: events.len(),
                model_round,
                kind,
                correlation_id: id.into(),
                name: name.into(),
                content,
                succeeded,
            });
            true
        };
        for (index, entry) in turn.entries.iter().enumerate() {
            match entry {
                Entry::Assistant { text, calls } => {
                    if round > 0 {
                        add(
                            anchor,
                            round,
                            CapturedEchoKind::ModelRoundCompleted,
                            "",
                            "",
                            "",
                            true,
                        );
                    }
                    round += 1;
                    captured_calls.clear();
                    completed = !calls.is_empty() || turn.status == Status::Complete;
                    if !text.trim().is_empty() {
                        previously_archived = Some(index);
                    }
                    anchor = next_text[index].or(previously_archived);
                    add(
                        anchor,
                        round,
                        CapturedEchoKind::ModelRoundStarted,
                        "",
                        "",
                        "",
                        true,
                    );
                    if let Some(summary) = self
                        .reasoning_summaries
                        .iter()
                        .find(|summary| summary.entry_index == index)
                    {
                        add(
                            anchor,
                            round,
                            CapturedEchoKind::ReasoningSummary,
                            "",
                            "",
                            &summary.text,
                            true,
                        );
                    }
                    for call in calls {
                        if valid_identifier(&call.id)
                            && valid_identifier(&call.name)
                            && !captured_calls.contains(call.id.as_str())
                            && add(
                                anchor,
                                round,
                                CapturedEchoKind::ToolCall,
                                &call.id,
                                &call.name,
                                &call.arguments,
                                false,
                            )
                        {
                            captured_calls.insert(call.id.as_str());
                        }
                    }
                }
                Entry::Tool {
                    id,
                    name,
                    result,
                    approved,
                } => {
                    if captured_calls.remove(id.as_str()) {
                        add(
                            anchor,
                            round,
                            CapturedEchoKind::ToolResult,
                            id,
                            name,
                            result,
                            *approved,
                        );
                    }
                }
            }
        }
        if completed {
            add(
                anchor,
                round,
                CapturedEchoKind::ModelRoundCompleted,
                "",
                "",
                "",
                true,
            );
        }
        events
    }

    fn validate_rows(rows: &[CapturedText]) -> Result<()> {
        ensure!(
            !rows.is_empty()
                && rows.len() <= 128
                && rows.iter().all(|row| {
                    !row.content.trim().is_empty()
                        && row.content.len() <= 32_768
                        && !contains_inline_data(&row.content)
                }),
            "memory source exceeds the supported text-only copy format"
        );
        Ok(())
    }

    fn rows(&self, turn: &Turn) -> Vec<CapturedText> {
        // Old receipts must replay the exact original source projection.
        let mut messages = vec![json!({"role": "user", "content": turn.prompt})];
        let mut entry_indices = Vec::new();
        if self.archive_by_entry {
            for (index, entry) in turn.entries.iter().enumerate() {
                if let Entry::Assistant { text, .. } = entry
                    && !text.trim().is_empty()
                {
                    messages.push(json!({"role": "assistant", "content": text}));
                    entry_indices.push(index);
                }
            }
        } else if let [Entry::Assistant { text, calls }] = turn.entries.as_slice()
            && turn.status == Status::Complete
            && calls.is_empty()
            && !text.is_empty()
        {
            messages.push(json!({"role": "assistant", "content": text}));
        }
        privacy::redact_messages(&mut messages);
        messages
            .into_iter()
            .enumerate()
            .map(|(i, msg)| {
                let (role, timestamp_ns) = if i == 0 {
                    ("user", self.admitted_ns)
                } else {
                    ("assistant", self.committed_ns)
                };
                let entry_index = i.checked_sub(1).and_then(|i| entry_indices.get(i));
                CapturedText {
                    message_uid: match entry_index {
                        Some(index) => format!("generation-{}-assistant-{index}", self.generation),
                        None => format!("generation-{}-{role}", self.generation),
                    },
                    role,
                    timestamp_ns,
                    content: msg["content"].as_str().unwrap_or_default().to_owned(),
                    commentary: entry_index.is_some_and(|index| {
                        matches!(&turn.entries[*index], Entry::Assistant { calls, .. } if !calls.is_empty())
                    }),
                }
            })
            .collect()
    }

    fn fingerprint(&self, turn: &Turn, rows: &[CapturedText]) -> Result<[u8; 32]> {
        let status = match &turn.status {
            Status::Complete => "complete",
            Status::Failed(_) => "failed",
            Status::Interrupted => "interrupted",
            Status::Running => "running",
        };
        let evidence = (
            if self.archive_by_entry {
                "hermes.capture.v2"
            } else {
                "hermes.capture.v1"
            },
            &self.binding,
            &self.session_id,
            &self.admission_id,
            self.generation,
            self.turn_index,
            status,
            rows,
        );
        let bytes = if self.archive_by_entry {
            serde_json::to_vec(&(evidence, self.echo(turn)))?
        } else {
            serde_json::to_vec(&evidence)?
        };
        Ok(digest(&SHA256, &bytes)
            .as_ref()
            .try_into()
            .expect("SHA-256 is 32 bytes"))
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}

/// Binary data URLs and embedded base64 are outside the text-only memory boundary.
pub fn contains_inline_data(text: &str) -> bool {
    text.trim_start()
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
        || text
            .as_bytes()
            .windows(8)
            .any(|part| part.eq_ignore_ascii_case(b";base64,"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retained_capture_checks_redacted_text_and_frozen_route() {
        let route = CaptureBinding {
            profile_id: "p".into(),
            principal_id: "user".into(),
            project_id: Some("project".into()),
            rel_id: "rel".into(),
            phy_id: "phy".into(),
        };
        let mut turn = Turn {
            attachments: vec![],
            prompt: "password=example".into(),
            context: "private file content".into(),
            model: "test".into(),
            status: Status::Complete,
            entries: vec![Entry::Assistant {
                text: "answer".into(),
                calls: vec![],
            }],
        };
        let marker =
            PendingMemoryCapture::new(&route, "session", "admission", 2, 0, 100, 105, &turn)
                .unwrap();
        let rows = marker.verified_rows(&turn).unwrap();
        assert_eq!(rows[0].content, "password=[REDACTED]");
        assert_eq!(rows[1].timestamp_ns, 105);
        let mut tool_turn = turn.clone();
        tool_turn.entries.insert(
            0,
            Entry::Assistant {
                text: "Checking the source.".into(),
                calls: vec![crate::ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                }],
            },
        );
        tool_turn.entries.insert(
            1,
            Entry::Tool {
                id: "call-1".into(),
                name: "read_file".into(),
                approved: true,
                result: "evidence".into(),
            },
        );
        let next = PendingMemoryCapture::new(&route, "session", "next", 3, 1, 110, 120, &tool_turn)
            .unwrap()
            .with_reasoning_summaries(
                &tool_turn,
                vec![CapturedReasoningSummary {
                    entry_index: 0,
                    text: "Reviewed the source.".into(),
                }],
            )
            .unwrap();
        let rows = next.verified_rows(&tool_turn).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.content.as_str())
                .collect::<Vec<_>>(),
            ["password=[REDACTED]", "Checking the source.", "answer"]
        );
        assert_eq!(rows[1].message_uid, "generation-3-assistant-0");
        assert_eq!(rows[2].message_uid, "generation-3-assistant-2");
        let echo = next.verified_echo(&tool_turn).unwrap();
        assert_eq!(
            echo.iter().map(|event| event.kind).collect::<Vec<_>>(),
            [
                CapturedEchoKind::ModelRoundStarted,
                CapturedEchoKind::ReasoningSummary,
                CapturedEchoKind::ToolCall,
                CapturedEchoKind::ToolResult,
                CapturedEchoKind::ModelRoundCompleted,
                CapturedEchoKind::ModelRoundStarted,
                CapturedEchoKind::ModelRoundCompleted,
            ]
        );
        assert_eq!(echo[2].correlation_id, echo[3].correlation_id);
        assert_eq!(echo[3].message_uid, rows[1].message_uid);
        assert_eq!(echo[3].content, "evidence");

        let mut old = marker.clone();
        old.archive_by_entry = false;
        old.evidence_sha256 = old.fingerprint(&turn, &old.rows(&turn)).unwrap();
        let mut old_record = serde_json::to_value(&old).unwrap();
        old_record
            .as_object_mut()
            .unwrap()
            .remove("archive_by_entry");
        let restored: PendingMemoryCapture = serde_json::from_value(old_record).unwrap();
        assert_eq!(
            restored.verified_rows(&turn).unwrap()[1].message_uid,
            "generation-2-assistant"
        );
        let mut redirected = marker.clone();
        redirected.binding.rel_id = "other".into();
        assert!(redirected.verified_rows(&turn).is_err());
        turn.prompt = "different".into();
        assert!(marker.verified_rows(&turn).is_err());
    }
}
