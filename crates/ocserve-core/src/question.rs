//! Question rendezvous — port of upstream `Question` service
//! (`opencode/src/question/index.ts`): pending map, `question.asked/replied/
//! rejected` events, reply/reject resolution. Bounded: wait cap replaces
//! upstream's infinite Deferred await (AGENTS §2.3 — divergence documented
//! in the runner: timeout resolves as a reject).

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Runner-side bound for an unanswered question (upstream waits forever).
pub const QUESTION_WAIT: Duration = Duration::from_secs(1800);

/// v1 RejectedError message (verbatim): the tool surfaces this to the model.
pub const REJECTED_MESSAGE: &str = "The user dismissed this question";

#[derive(Debug, Clone)]
pub enum Outcome {
    /// reply: answers in question order (each = selected labels).
    Answers(Vec<Vec<String>>),
    /// reject / wait timeout / channel closed.
    Rejected,
}

struct Pending {
    request: Value,
    tx: tokio::sync::oneshot::Sender<Outcome>,
}

/// Pending question requests across sessions (GET /question list + resolve).
#[derive(Default)]
pub struct QuestionGate {
    pending: parking_lot::Mutex<HashMap<String, Pending>>,
}

/// Dropping the runner's guard removes its pending entry (upstream's
/// Effect.ensuring — covers abort of the background prompt task).
pub struct PendingGuard {
    gate: Arc<QuestionGate>,
    id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.gate.pending.lock().remove(&self.id);
    }
}

impl QuestionGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a pending ask; returns the outcome receiver + cleanup guard.
    pub fn register(
        self: &Arc<Self>,
        id: &str,
        request: Value,
    ) -> (tokio::sync::oneshot::Receiver<Outcome>, PendingGuard) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending
            .lock()
            .insert(id.to_string(), Pending { request, tx });
        let guard = PendingGuard {
            gate: Arc::clone(self),
            id: id.to_string(),
        };
        (rx, guard)
    }

    /// `POST /question/{id}/reply` — answers must be an array of string
    /// arrays (v1 Question.Reply). Unknown id → false (404 upstream).
    pub fn reply(&self, id: &str, answers: Vec<Vec<String>>) -> bool {
        match self.pending.lock().remove(id) {
            Some(p) => p.tx.send(Outcome::Answers(answers)).is_ok(),
            None => false,
        }
    }

    /// `POST /question/{id}/reject` (no body). Unknown id → false.
    pub fn reject(&self, id: &str) -> bool {
        match self.pending.lock().remove(id) {
            Some(p) => p.tx.send(Outcome::Rejected).is_ok(),
            None => false,
        }
    }

    /// Runner-side timeout cleanup (entry gone → list no longer shows it).
    pub fn remove(&self, id: &str) {
        self.pending.lock().remove(id);
    }

    /// `GET /question` — all pending Request objects (v1 Question.Request).
    pub fn list(&self) -> Vec<Value> {
        self.pending
            .lock()
            .values()
            .map(|p| p.request.clone())
            .collect()
    }

    /// Runner wait with the bounded cap; timeout → Rejected + cleanup.
    pub async fn wait(&self, id: &str, rx: tokio::sync::oneshot::Receiver<Outcome>) -> Outcome {
        match tokio::time::timeout(QUESTION_WAIT, rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => Outcome::Rejected, // sender dropped
            Err(_) => {
                self.remove(id);
                Outcome::Rejected
            }
        }
    }
}

/// v1 QuestionTool output formatting (verbatim template).
pub fn format_output(questions: &Value, answers: &[Vec<String>]) -> String {
    let items: Vec<String> = questions
        .as_array()
        .map(|qs| {
            qs.iter()
                .enumerate()
                .map(|(i, q)| {
                    let answer = answers
                        .get(i)
                        .map(|a| a.join(", "))
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "Unanswered".to_string());
                    format!(
                        "\"{}\"=\"{}\"",
                        q["question"].as_str().unwrap_or_default(),
                        answer
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    format!(
        "User has answered your questions: {}. You can now continue with the user's answers in mind.",
        items.join(", ")
    )
}

/// v1 QuestionTool title.
pub fn format_title(questions: &Value) -> String {
    let n = questions.as_array().map(|a| a.len()).unwrap_or(0);
    format!("Asked {} question{}", n, if n > 1 { "s" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn output_matches_v1_template() {
        let qs = json!([
            {"question": "Pick a color", "header": "Color",
             "options": [{"label": "Red", "description": "r"}]},
            {"question": "Pick speed", "header": "Speed",
             "options": [{"label": "Fast", "description": "f"}]}
        ]);
        let answers = vec![vec!["Red".to_string()], vec![]];
        assert_eq!(
            format_output(&qs, &answers),
            "User has answered your questions: \"Pick a color\"=\"Red\", \
             \"Pick speed\"=\"Unanswered\". You can now continue with the user's answers in mind."
        );
        assert_eq!(format_title(&qs), "Asked 2 questions");
        assert_eq!(format_title(&json!([])), "Asked 0 question"); // v1: s only when n>1
    }

    #[tokio::test]
    async fn register_reply_list_roundtrip() {
        let gate = QuestionGate::new();
        let req = json!({"id": "que_1", "sessionID": "ses_1", "questions": []});
        let (rx, _guard) = gate.register("que_1", req.clone());
        assert_eq!(gate.list(), vec![req.clone()]);
        assert!(gate.reply("que_1", vec![vec!["a".into()]]));
        assert!(gate.list().is_empty(), "entry removed on reply");
        match rx.await.unwrap() {
            Outcome::Answers(a) => assert_eq!(a, vec![vec!["a".to_string()]]),
            _ => panic!("expected answers"),
        }
        // double reply → false (404)
        assert!(!gate.reply("que_1", vec![]));
    }

    #[tokio::test]
    async fn reject_resolves_and_unknown_is_false() {
        let gate = QuestionGate::new();
        let (rx, _guard) = gate.register("que_2", json!({"id": "que_2"}));
        assert!(gate.reject("que_2"));
        assert!(matches!(rx.await.unwrap(), Outcome::Rejected));
        assert!(!gate.reject("que_missing"));
        assert!(!gate.reply("que_missing", vec![]));
    }

    #[tokio::test(start_paused = true)]
    async fn wait_timeout_rejects_and_cleans_pending() {
        let gate = QuestionGate::new();
        let (rx, _guard) = gate.register("que_3", json!({"id": "que_3"}));
        assert_eq!(gate.list().len(), 1);
        // no reply ever — advance past QUESTION_WAIT under paused time
        tokio::time::advance(QUESTION_WAIT + Duration::from_secs(1)).await;
        let outcome = gate.wait("que_3", rx).await;
        assert!(matches!(outcome, Outcome::Rejected));
        assert!(gate.list().is_empty(), "timeout cleans the pending map");
    }

    #[tokio::test]
    async fn guard_drop_cleans_pending_on_abort() {
        let gate = QuestionGate::new();
        {
            let (_rx, _guard) = gate.register("que_4", json!({"id": "que_4"}));
            assert_eq!(gate.list().len(), 1);
        } // guard dropped → task aborted equivalent
        assert!(
            gate.list().is_empty(),
            "drop removes pending (upstream ensuring)"
        );
    }
}
