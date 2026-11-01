//! `/btw <question>`: a side question about `main`, answered from a snapshot of its messages by
//! a throwaway child with no tools and no session file, so `main`'s session, queue and run are
//! left as they are (DESIGN §4.1).

use crate::config::{Account, Config, BUDGET_NOTICE};
use crate::rpc::{message_text, Child, Tally};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub const USAGE: &str = "[zephyr] usage: /btw <question> asks a side question about the conversation; the conversation is left as is";
const TIMEOUT: Duration = Duration::from_secs(120);
const PER_MESSAGE: usize = 2000;
const TOTAL: usize = 24000;

/// `text` cut to `max` chars, `…` when cut.
fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

/// `main`'s messages (a `get_messages` answer) as a compact transcript, newest last: user and
/// assistant text, each tool call as a `[tool: name]` line. Each message is cut to
/// `PER_MESSAGE` chars, and the oldest go first past `TOTAL`.
pub fn transcript(snap: &Value) -> String {
    let mut entries = Vec::new();
    for m in snap["messages"].as_array().into_iter().flatten() {
        let text = message_text(m);
        let mut entry = match m["kind"].as_str() {
            Some("user") => {
                let body = crate::router::untag(&text).map_or(text.as_str(), |(_, _, b)| b);
                format!("user: {}", cut(body, PER_MESSAGE))
            }
            Some("assistant") if text.is_empty() => String::new(),
            Some("assistant") => format!("assistant: {}", cut(&text, PER_MESSAGE)),
            _ => continue,
        };
        let tools = m["content"].as_array().into_iter().flatten();
        for name in tools
            .filter(|b| b["type"] == "toolCall")
            .map(|b| &b["name"])
        {
            let line = format!("[tool: {}]", name.as_str().unwrap_or("?"));
            entry = if entry.is_empty() {
                line
            } else {
                entry + "\n" + &line
            };
        }
        if !entry.is_empty() {
            entries.push(entry);
        }
    }
    let (mut kept, mut size) = (Vec::new(), 0);
    for entry in entries.iter().rev() {
        size += entry.chars().count() + 2;
        if size > TOTAL {
            kept.push("(earlier messages left out)".to_string());
            break;
        }
        kept.push(entry.clone());
    }
    kept.reverse();
    kept.join("\n\n")
}

/// The side question's prompt: the instruction, `main`'s transcript (`None` when it could not
/// be read, e.g. no child is running), then the question.
pub fn prompt(transcript: Option<String>, question: &str) -> String {
    let conversation = match transcript {
        Some(t) if !t.is_empty() => t,
        Some(_) => "(the conversation is empty)".into(),
        None => "(the conversation could not be read)".into(),
    };
    format!(
        "You are answering a side question about the conversation below. Answer only the \
         question, briefly; you cannot use tools or act; the main conversation continues \
         separately.\n\n<conversation>\n{conversation}\n</conversation>\n\nQuestion: {question}"
    )
}

/// Asks on a throwaway child, books its spend like a job's, and writes the answer (or why there
/// is none) to `to`, the channel and sender that asked. Runs on a task of its own.
pub async fn run(cfg: Arc<Config>, account: Option<Account>, prompt: String, to: (String, String)) {
    let mut tally = Tally::default();
    let text = match ask(&cfg, account.as_ref(), &prompt, &mut tally).await {
        Ok(answer) => format!("[btw] {answer}"),
        Err(e) => format!("[zephyr] /btw failed: {e:#}"),
    };
    let store = match crate::store::Store::open(&cfg.db_path()) {
        Ok(store) => store,
        Err(e) => return eprintln!("zephyr: /btw could not open the store: {e}"),
    };
    let crossed = crate::scheduler::book(&store, &cfg, account.as_ref(), tally.used);
    for text in [Some(text.as_str()), crossed.then_some(BUDGET_NOTICE)]
        .into_iter()
        .flatten()
    {
        if let Err(e) = store.notify(&to.0, &to.1, text) {
            eprintln!("zephyr: /btw reply failed: {e}");
        }
    }
}

async fn ask(
    cfg: &Config,
    account: Option<&Account>,
    prompt: &str,
    tally: &mut Tally,
) -> anyhow::Result<String> {
    let mut child = Child::spawn_as(cfg, None, account).await?;
    let ran = crate::scheduler::prompt(
        cfg,
        &mut child,
        prompt,
        "not available in scheduled runs",
        tally,
        None,
    );
    let answer = match tokio::time::timeout(TIMEOUT, ran).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(e)) => Err(e),
        Err(_) => {
            let abort = child.call(json!({"type": "abort"}));
            let _ = tokio::time::timeout(Duration::from_secs(5), abort).await;
            Err(anyhow::anyhow!("no answer within {TIMEOUT:?}"))
        }
    };
    child.stop().await;
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transcript_is_compact_and_keeps_the_newest() {
        let snap = json!({"messages": [
            {"kind": "user", "content": "[discord #1] hi"},
            {"kind": "assistant", "content": [{"type": "text", "text": "looking"},
                                              {"type": "toolCall", "name": "bash"}]},
            {"kind": "toolResult", "content": [{"type": "text", "text": "secret output"}]},
            {"kind": "assistant", "content": [{"type": "toolCall", "name": "read"}]},
            {"kind": "assistant", "content": [{"type": "text", "text": "x".repeat(3000)}]},
        ]});
        let t = transcript(&snap);
        assert!(t.starts_with("user: hi\n\nassistant: looking\n[tool: bash]\n\n[tool: read]\n\n"));
        assert!(!t.contains("secret output"));
        assert!(t.ends_with(&format!("assistant: {}…", "x".repeat(PER_MESSAGE))));

        let long: Vec<Value> = (0..30)
            .map(|i| json!({"kind": "user", "content": format!("{i} {}", "y".repeat(1500))}))
            .collect();
        let t = transcript(&json!({ "messages": long }));
        assert!(t.chars().count() <= TOTAL + 40, "{}", t.len());
        assert!(t.starts_with("(earlier messages left out)\n\nuser: 15 "));
        assert!(t.ends_with(&format!("user: 29 {}", "y".repeat(1500))));
    }
}
