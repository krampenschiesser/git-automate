//! Machine-readable agent verdicts.
//!
//! Agents no longer mutate the project status directly. Instead, each session's
//! final message ends with a single marker line:
//!
//! ```text
//! GIT_AUTOMATE_VERDICT: {"v":1,"role":"reviewer","decision":"approve"}
//! ```
//!
//! [`parse_verdict`] scans the session messages, takes the LAST such line, and
//! deserializes its payload. The daemon owns every status transition based on
//! the parsed [`AgentVerdict`].

use serde::Deserialize;

use crate::external_agent::opencode::types::SessionMessage;

/// Exact prefix that introduces a verdict line (note the trailing space).
pub const VERDICT_PREFIX: &str = "GIT_AUTOMATE_VERDICT: ";

/// The decision an agent reached for its workflow step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentDecision {
    /// A review step accepts the work.
    Approve,
    /// A review step requests changes.
    Changes,
    /// The triage step has decomposed the issue and is ready.
    Ready,
    /// The developer step finished its implementation.
    Done,
    /// An unrecognized decision string (fail-safe: never an approval).
    Unknown(String),
}

impl AgentDecision {
    fn from_wire(value: &str) -> Self {
        match value {
            "approve" => AgentDecision::Approve,
            "changes" => AgentDecision::Changes,
            "ready" => AgentDecision::Ready,
            "done" => AgentDecision::Done,
            other => AgentDecision::Unknown(other.to_string()),
        }
    }
}

/// A sub-task proposed by the triage agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubTask {
    pub title: String,
    pub description: String,
}

/// A fully parsed agent verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentVerdict {
    pub role: String,
    pub decision: AgentDecision,
    pub notes: String,
    pub sub_tasks: Vec<SubTask>,
    pub resolved_threads: Vec<String>,
}

/// Result of scanning session messages for a verdict.
#[derive(Debug, Clone, PartialEq)]
pub enum VerdictParse {
    /// A well-formed verdict was found.
    Found(AgentVerdict),
    /// No verdict marker was present.
    Missing,
    /// A marker was present but its payload could not be used.
    Malformed(String),
}

#[derive(Deserialize)]
struct RawVerdict {
    v: i64,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    sub_tasks: Vec<RawSubTask>,
    #[serde(default)]
    resolved_threads: Vec<String>,
}

#[derive(Deserialize)]
struct RawSubTask {
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
}

/// Parse the last `GIT_AUTOMATE_VERDICT:` line from *messages*.
///
/// All text content across every message is scanned; the last matching line
/// wins. Returns [`VerdictParse::Missing`] when no marker is present and
/// [`VerdictParse::Malformed`] when the payload is not valid JSON, omits the
/// required `role`/`decision` fields, or carries a version other than `1`.
/// This function never panics.
pub fn parse_verdict(messages: &[SessionMessage]) -> VerdictParse {
    let mut last_payload: Option<String> = None;

    for message in messages {
        for line in message.text().lines() {
            if let Some(rest) = line.trim().strip_prefix(VERDICT_PREFIX) {
                last_payload = Some(rest.trim().to_string());
            }
        }
    }

    let Some(payload) = last_payload else {
        return VerdictParse::Missing;
    };

    if payload.is_empty() {
        return VerdictParse::Malformed("empty verdict payload".to_string());
    }

    let raw: RawVerdict = match serde_json::from_str(&payload) {
        Ok(raw) => raw,
        Err(e) => return VerdictParse::Malformed(format!("invalid verdict JSON: {e}")),
    };

    if raw.v != 1 {
        return VerdictParse::Malformed(format!("unsupported verdict version {}", raw.v));
    }

    let Some(role) = raw.role.filter(|r| !r.trim().is_empty()) else {
        return VerdictParse::Malformed("missing verdict role".to_string());
    };

    let Some(decision) = raw.decision.filter(|d| !d.trim().is_empty()) else {
        return VerdictParse::Malformed("missing verdict decision".to_string());
    };

    let sub_tasks = raw
        .sub_tasks
        .into_iter()
        .map(|task| SubTask {
            title: task.title,
            description: task.description,
        })
        .collect();

    VerdictParse::Found(AgentVerdict {
        role,
        decision: AgentDecision::from_wire(&decision),
        notes: raw.notes.unwrap_or_default(),
        sub_tasks,
        resolved_threads: raw.resolved_threads,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_agent::opencode::types::AssistantContent;

    fn assistant(text: &str) -> SessionMessage {
        SessionMessage::Assistant {
            id: "m1".to_string(),
            time: None,
            content: vec![AssistantContent::Text {
                text: text.to_string(),
            }],
        }
    }

    // T1: full developer verdict parses with resolved threads.
    #[test]
    fn parse_verdict_found_developer() {
        let messages = vec![assistant(
            "Implemented the fix.\nGIT_AUTOMATE_VERDICT: {\"v\":1,\"role\":\"developer\",\"decision\":\"done\",\"resolved_threads\":[\"TH_1\",\"TH_2\"]}",
        )];
        let VerdictParse::Found(verdict) = parse_verdict(&messages) else {
            panic!("expected Found, got {:?}", parse_verdict(&messages));
        };
        assert_eq!(verdict.role, "developer");
        assert_eq!(verdict.decision, AgentDecision::Done);
        assert_eq!(verdict.resolved_threads, vec!["TH_1", "TH_2"]);
    }

    // T2: triage verdict carries sub-tasks and notes.
    #[test]
    fn parse_verdict_found_triage_sub_tasks() {
        let messages = vec![assistant(
            "GIT_AUTOMATE_VERDICT: {\"v\":1,\"role\":\"triage\",\"decision\":\"ready\",\"sub_tasks\":[{\"title\":\"Auth\",\"description\":\"Add OAuth\"}]}",
        )];
        let VerdictParse::Found(verdict) = parse_verdict(&messages) else {
            panic!("expected Found");
        };
        assert_eq!(verdict.decision, AgentDecision::Ready);
        assert_eq!(
            verdict.sub_tasks,
            vec![SubTask {
                title: "Auth".to_string(),
                description: "Add OAuth".to_string(),
            }]
        );
        assert!(verdict.notes.is_empty());
    }

    // T3: absent marker -> Missing.
    #[test]
    fn parse_verdict_missing() {
        let messages = vec![assistant("No marker here.")];
        assert_eq!(parse_verdict(&messages), VerdictParse::Missing);
    }

    // T4: invalid JSON -> Malformed.
    #[test]
    fn parse_verdict_malformed_json() {
        let messages = vec![assistant("GIT_AUTOMATE_VERDICT: {not json")];
        assert!(matches!(
            parse_verdict(&messages),
            VerdictParse::Malformed(_)
        ));
    }

    // T5: unsupported version -> Malformed.
    #[test]
    fn parse_verdict_bad_version() {
        let messages = vec![assistant(
            "GIT_AUTOMATE_VERDICT: {\"v\":2,\"role\":\"reviewer\",\"decision\":\"approve\"}",
        )];
        assert!(matches!(
            parse_verdict(&messages),
            VerdictParse::Malformed(_)
        ));
    }

    // T6: unknown decision string is preserved, not rejected.
    #[test]
    fn parse_verdict_unknown_decision() {
        let messages = vec![assistant(
            "GIT_AUTOMATE_VERDICT: {\"v\":1,\"role\":\"reviewer\",\"decision\":\"maybe\"}",
        )];
        let VerdictParse::Found(verdict) = parse_verdict(&messages) else {
            panic!("expected Found");
        };
        assert_eq!(
            verdict.decision,
            AgentDecision::Unknown("maybe".to_string())
        );
    }

    // T7: the LAST marker wins when several are present.
    #[test]
    fn parse_verdict_last_marker_wins() {
        let messages = vec![
            assistant(
                "GIT_AUTOMATE_VERDICT: {\"v\":1,\"role\":\"reviewer\",\"decision\":\"changes\"}",
            ),
            assistant(
                "GIT_AUTOMATE_VERDICT: {\"v\":1,\"role\":\"reviewer\",\"decision\":\"approve\"}",
            ),
        ];
        let VerdictParse::Found(verdict) = parse_verdict(&messages) else {
            panic!("expected Found");
        };
        assert_eq!(verdict.decision, AgentDecision::Approve);
    }

    // T8: missing role/decision -> Malformed.
    #[test]
    fn parse_verdict_missing_required_fields() {
        let no_role = vec![assistant(
            "GIT_AUTOMATE_VERDICT: {\"v\":1,\"decision\":\"approve\"}",
        )];
        assert!(matches!(
            parse_verdict(&no_role),
            VerdictParse::Malformed(_)
        ));

        let no_decision = vec![assistant("GIT_AUTOMATE_VERDICT: {\"v\":1,\"role\":\"qa\"}")];
        assert!(matches!(
            parse_verdict(&no_decision),
            VerdictParse::Malformed(_)
        ));
    }
}
