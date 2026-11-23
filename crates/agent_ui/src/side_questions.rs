//! Side questions ("btw"): a question asked about a running session that is
//! answered by a fork of it, so the session doing the work is never
//! interrupted and never sees the question.

use acp_thread::{AcpThread, AgentThreadEntry, AssistantMessageChunk};
use agent_client_protocol::v2::schema as acp_v2;
use gpui::{App, Entity, SharedString, Subscription, Task};

pub const BTW_COMMAND: &str = "btw";

pub const BTW_COMMAND_DESCRIPTION: &str =
    "Ask a side question in a fork of this session without interrupting it";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SideQuestionId(pub usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideQuestionState {
    Forking,
    Ready,
    Failed(SharedString),
}

pub struct SideQuestion {
    pub id: SideQuestionId,
    pub parent_session_id: acp_v2::SessionId,
    pub question: SharedString,
    pub state: SideQuestionState,
    pub thread: Option<Entity<AcpThread>>,
    pub _subscription: Option<Subscription>,
    pub _task: Task<()>,
}

impl SideQuestion {
    pub fn session_id(&self, cx: &App) -> Option<acp_v2::SessionId> {
        self.thread
            .as_ref()
            .map(|thread| thread.read(cx).session_id().clone())
    }

    pub fn is_generating(&self, cx: &App) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| thread.read(cx).status() == acp_thread::ThreadStatus::Generating)
    }

    pub fn answer(&self, cx: &App) -> Option<String> {
        latest_answer(self.thread.as_ref()?.read(cx), cx)
    }
}

/// Appended to the fork's system prompt.
pub fn system_prompt() -> String {
    "You are a fork of a coding session, started for a side question the user \
     asked while the original session keeps working in parallel. The original \
     may already have moved past the point you were forked at.\n\n\
     Rules:\n\
     - Do what the user asks here, not the original task. A question gets a \
     direct, concise answer; a request to build or change something gets done.\n\
     - The original session is editing the same working tree right now. Before \
     you change a file, say which, and prefer files it is not working on."
        .to_string()
}

/// The question in a `/btw` message, or `None` when the text is not one.
/// A bare `/btw` yields an empty question.
pub fn parse_btw_command(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix('/')?;
    let rest = rest.strip_prefix(BTW_COMMAND)?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim())
}

pub fn quote(text: &str) -> String {
    text.trim()
        .lines()
        .map(|line| {
            if line.is_empty() {
                ">".to_string()
            } else {
                format!("> {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn explain_selection_question(selection: &str) -> String {
    format!(
        "Explain this part of the conversation:\n\n{}",
        quote(selection)
    )
}

/// What "Ask About This…" puts in the composer, and where the cursor goes so
/// the user types the question before the quoted selection.
pub fn ask_about_selection_draft(selection: &str) -> (String, usize) {
    let prefix = format!("/{BTW_COMMAND} ");
    let cursor = prefix.len();
    (format!("{prefix}\n\n{}", quote(selection)), cursor)
}

/// A short title for the fork's thread.
pub fn title(question: &str) -> SharedString {
    const MAX_CHARS: usize = 60;
    let first_line = question
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('>'))
        .unwrap_or("Side question");
    let mut title: String = first_line.chars().take(MAX_CHARS).collect();
    if first_line.chars().count() > MAX_CHARS {
        title.push('…');
    }
    format!("btw: {title}").into()
}

/// What "Use Answer" puts in the parent's composer.
pub fn answer_for_parent(question: &str, answer: &str) -> String {
    format!(
        "From a side question I asked a fork of this session:\n\n{}\n\n{}\n",
        quote(question),
        answer.trim()
    )
}

/// The text of the fork's most recent assistant message, without thinking.
pub fn latest_answer(thread: &AcpThread, cx: &App) -> Option<String> {
    let message = thread
        .entries()
        .iter()
        .rev()
        .take_while(|entry| !matches!(entry, AgentThreadEntry::UserMessage(_)))
        .find_map(|entry| match entry {
            AgentThreadEntry::AssistantMessage(message) => Some(message),
            _ => None,
        })?;
    let text = message
        .chunks
        .iter()
        .filter_map(|chunk| match chunk {
            AssistantMessageChunk::Message { block, .. } => Some(block.to_markdown(cx)),
            AssistantMessageChunk::Thought { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_btw_command() {
        assert_eq!(parse_btw_command("/btw why?"), Some("why?"));
        assert_eq!(parse_btw_command("  /btw  why?  "), Some("why?"));
        assert_eq!(parse_btw_command("/btw\n\n> quoted"), Some("> quoted"));
        assert_eq!(parse_btw_command("/btw"), Some(""));
        assert_eq!(parse_btw_command("/btwx why?"), None);
        assert_eq!(parse_btw_command("btw why?"), None);
        assert_eq!(parse_btw_command("/compact"), None);
    }

    #[test]
    fn quotes_every_line() {
        assert_eq!(quote("a\n\nb\n"), "> a\n>\n> b");
    }

    #[test]
    fn ask_about_selection_puts_cursor_before_the_quote() {
        let (draft, cursor) = ask_about_selection_draft("some code");
        assert_eq!(draft, "/btw \n\n> some code");
        assert_eq!(&draft[..cursor], "/btw ");
    }

    #[test]
    fn title_skips_quotes_and_truncates() {
        assert_eq!(title("> quoted\nwhy this?").as_ref(), "btw: why this?");
        assert_eq!(title("> only a quote").as_ref(), "btw: Side question");
        let long = "x".repeat(80);
        assert_eq!(title(&long).as_ref(), format!("btw: {}…", "x".repeat(60)));
    }
}
