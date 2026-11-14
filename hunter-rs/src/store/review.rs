//! What a reviewer reports, the same for the pre-PR review of a fix and the
//! review of a PR hunter did not open: both reviewers write the issue
//! schema of their playbook (`review.md`).

use serde::{Deserialize, Serialize};

/// An issue as the reviewer reported it: a pre-PR round's, which goes back
/// to the fixer and is kept in the fix's review files, or a PR review
/// round's ([`PrReviewRound::Ok`](super::PrReviewRound::Ok)).
///
/// It is written as the playbooks' JSON object, with the fields the
/// reviewer left out still left out, and read leniently (its `From`
/// `serde_json::Value`), so the files of an earlier daemon, which kept the
/// reviewer's entries as they were, still read.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(from = "serde_json::Value")]
pub struct ReportedIssue {
    pub kind: String,
    pub blocking: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    /// `RIGHT` or `LEFT`, in any case; anything else is the reviewer not
    /// saying.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    pub summary: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

impl ReportedIssue {
    /// Whether the reviewer judged the issue outside the change's scope: a
    /// real defect it did not introduce or worsen, or a generalization
    /// beyond the finding. A fix's review reports it to the human, and
    /// never sends it to the fixer.
    pub fn is_follow_up(&self) -> bool {
        self.kind == "follow-up"
    }
}

/// A reviewer's issue entry: the playbook's schema, read leniently. A
/// missing or mistyped optional field is none, a missing kind is the
/// reviewer's default `introduced`, a `blocking` that is not `true` is not
/// blocking, and fields outside the schema are dropped. Which entries are
/// issues at all, and a `blocking` the reviewer got wrong, are for the
/// reader of its file to decide (`parse_review`).
impl From<serde_json::Value> for ReportedIssue {
    fn from(v: serde_json::Value) -> Self {
        let s = |k: &str| {
            v.get(k)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        Self {
            kind: s("kind").unwrap_or_else(|| "introduced".to_owned()),
            blocking: v.get("blocking").and_then(serde_json::Value::as_bool) == Some(true),
            severity: s("severity"),
            confidence: v.get("confidence").and_then(serde_json::Value::as_f64),
            file: s("file"),
            line: v.get("line").and_then(serde_json::Value::as_i64),
            side: s("side"),
            summary: s("summary").unwrap_or_default(),
            detail: s("detail").unwrap_or_default(),
        }
    }
}
