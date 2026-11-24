//! Evidence for replayable memory capture from an authoritative, committed turn.
use crate::{Entry, Status, Turn, privacy};
use anyhow::{Result, ensure};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use serde_json::json;

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
            receipt: None,
            settled: false,
        };
        let rows = capture.rows(turn);
        Self::validate_rows(&rows)?;
        capture.evidence_sha256 = capture.fingerprint(turn, &rows)?;
        Ok(capture)
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

    fn validate_rows(rows: &[CapturedText]) -> Result<()> {
        ensure!(
            !rows.is_empty()
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
        // The native record timestamps the admission and completion. Tool rounds
        // have no per-message timestamps. Only copy the two attributable texts.
        let mut messages = vec![json!({"role": "user", "content": turn.prompt})];
        if let [Entry::Assistant { text, calls }] = turn.entries.as_slice()
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
                CapturedText {
                    message_uid: format!("generation-{}-{role}", self.generation),
                    role,
                    timestamp_ns,
                    content: msg["content"].as_str().unwrap_or_default().to_owned(),
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
        let bytes = serde_json::to_vec(&(
            "hermes.capture.v1",
            &self.binding,
            &self.session_id,
            &self.admission_id,
            self.generation,
            self.turn_index,
            status,
            rows,
        ))?;
        Ok(digest(&SHA256, &bytes)
            .as_ref()
            .try_into()
            .expect("SHA-256 is 32 bytes"))
    }
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
        let mut redirected = marker.clone();
        redirected.binding.rel_id = "other".into();
        assert!(redirected.verified_rows(&turn).is_err());
        turn.prompt = "different".into();
        assert!(marker.verified_rows(&turn).is_err());
    }
}
