//! Context management: prune, summarise, and the threshold that triggers both.
//!
//! Kept separate from `engine.rs` because the policy is worth reading on its
//! own: what gets dropped, what is kept verbatim, and what the model is told.
//!
//! Three rules shape this, and they are the ones both Claude Code and
//! OpenCode converged on:
//! 1. **Prune before summarising.** Old tool output is the cheapest thing to
//!    drop and usually the least useful, so it goes first.
//! 2. **Keep a verbatim tail.** A summary alone loses the exact values a task
//!    depends on ("the port was 8123"), so recent turns stay intact.
//! 3. **Compaction is a log record, not a rewrite.** The stored session keeps
//!    everything, so post-compaction questions ("what was in that file?") can
//!    still be answered by searching history rather than by luck.

use turya_protocol::{Part, Transcript, Turn, TurnId};

/// Turns at the tail that are never pruned or summarised. Everything older
/// loses its tool output; the tail keeps exact values because a summary
/// cannot be trusted with an identifier or a port number.
const KEEP_TAIL_TURNS: usize = 2;
/// How a pruned result is represented. Short and honest: the model is told
/// the value existed and is gone, not handed a fake one.
const PRUNED_MARKER: &str =
    "[pruned] earlier tool output was dropped to free context; re-run the tool if you need it.";

/// Replace old tool results with a marker. Pure, so the policy is testable
/// without a model. Returns how many results were pruned.
pub fn prune_transcript(transcript: &mut Transcript) -> usize {
    let Some(keep_from) = turn_cutoff(transcript) else {
        return 0;
    };
    let mut pruned = 0;
    for turn in transcript.turns.iter_mut().take(keep_from) {
        for part in turn.parts.iter_mut() {
            if let Part::ToolResult {
                output, truncated, ..
            } = part
            {
                if output != PRUNED_MARKER {
                    *output = PRUNED_MARKER.to_string();
                    *truncated = true;
                    pruned += 1;
                }
            }
        }
    }
    pruned
}

/// Index of the first turn to prune: everything except the verbatim tail.
fn turn_cutoff(transcript: &Transcript) -> Option<usize> {
    let eligible = transcript.turns.len().saturating_sub(KEEP_TAIL_TURNS);
    (eligible > 0).then_some(eligible)
}

/// The instruction sent to the model to produce a compaction summary.
/// Tool calls in this pass are structurally dropped by the caller, so the
/// ending holds even if the model ignores the instruction.
pub fn compaction_prompt(focus: Option<&str>) -> String {
    let focus_line = match focus {
        Some(f) if !f.trim().is_empty() => {
            format!("The user asked you to focus the summary on: {}\n", f.trim())
        }
        _ => String::new(),
    };
    format!(
        "Context is being compacted. Write a summary that lets a fresh context continue this \
         work without the history. Use these headings exactly:\n\
         ## Objective\n## Work done\n## Files and commands touched\n## Key facts and decisions\n\
         ## Remaining tasks\n\n\
         {focus_line}\
         Be specific about names, paths, identifiers, and values. Do not call any tools."
    )
}

/// Rebuild a compacted transcript: the summary turn plus the verbatim tail.
///
/// The summary is a `Part::Text` in its own turn so it renders like any other
/// assistant message and, on a later compaction, gets summarised again rather
/// than compounding into nonsense.
pub fn compacted(session_id: &str, summary: &str, keep_tail: &[Turn], marker: &str) -> Transcript {
    let mut out = Transcript::new(session_id);
    out.start_turn("compaction");
    out.push(Part::Text {
        text: format!("[context compacted] {marker}\n\n{summary}"),
    });
    for turn in keep_tail {
        out.turns.push(turn.clone());
    }
    out
}

/// The verbatim tail a compaction keeps.
pub fn tail_turns(transcript: &Transcript) -> Vec<Turn> {
    let start = transcript.turns.len().saturating_sub(KEEP_TAIL_TURNS);
    transcript.turns[start..].to_vec()
}

/// Rough per-turn token weight, for `/context`. Estimates are labelled as
/// such in the UI; provider-reported usage is authoritative when we have it.
pub fn estimate_turn_tokens(turn: &Turn) -> u32 {
    let chars: usize = turn
        .parts
        .iter()
        .map(|p| match p {
            Part::Text { text }
            | Part::Reasoning { text }
            | Part::UserText { text }
            | Part::Instruction { text } => text.chars().count(),
            Part::ToolCall {
                tool_name,
                arguments,
                ..
            } => tool_name.chars().count() + arguments.to_string().chars().count(),
            Part::ToolResult { output, .. } => output.chars().count(),
            Part::Attachment(a) | Part::Image(a) => a.path.to_string_lossy().chars().count(),
        })
        .sum();
    // Floor of 1: a turn is never free.
    (chars / 4).max(1) as u32
}

/// Total estimated tokens for a transcript.
pub fn estimate_transcript_tokens(transcript: &Transcript) -> u32 {
    transcript.turns.iter().map(estimate_turn_tokens).sum()
}

/// Bytes the session log holds for this transcript, for `/context`.
pub fn transcript_chars(transcript: &Transcript) -> usize {
    transcript
        .turns
        .iter()
        .map(estimate_turn_tokens)
        .sum::<u32>() as usize
        * 4
}

/// Ids of the last `n` turns, for messages.
pub fn recent_turn_ids(transcript: &Transcript, n: usize) -> Vec<TurnId> {
    let start = transcript.turns.len().saturating_sub(n);
    transcript.turns[start..]
        .iter()
        .map(|t| t.id.clone())
        .collect()
}
