//! Review prompts for the background self-review fork.
//!
//! Adapted from hermes-agent's background-review prompts to moltis tool names.
//! The prompts are deliberately biased toward acting ("a pass that does
//! nothing is a missed learning opportunity") while carrying explicit
//! anti-capture rules so transient noise never hardens into durable state.

use super::counters::ReviewKind;

pub const MEMORY_REVIEW_PROMPT: &str = r"Review the conversation above and consider saving to memory if appropriate.

Focus on:
1. Has the user revealed things about themselves — their persona, desires, preferences, or personal details worth remembering?
2. Has the user expressed expectations about how you should behave, their work style, or ways they want you to operate?
3. Did durable facts about the environment, project, or tools emerge that would help future sessions?

If something stands out, save it with the memory_save tool (search first with memory_search to avoid duplicates; consolidate or delete stale entries rather than piling up near-duplicates).

Do NOT save: task progress, completed-work logs, transient errors, or anything trivially re-discoverable. If nothing is worth saving, reply 'Nothing to save.' and stop.";

pub const SKILL_REVIEW_PROMPT: &str = r"Review the conversation above and update the skill library. Be ACTIVE — most working sessions produce at least one skill update, even if small. A pass that does nothing is a missed learning opportunity, not a neutral outcome.

Target shape of the library: CLASS-LEVEL skills — each covering a class of task with a rich SKILL.md — not a long flat list of narrow one-session entries.

Signals worth capturing:
- The user corrected your approach, style, or workflow ('stop doing X', 'too verbose', 'always do Y first'). These are FIRST-CLASS skill signals.
- A non-trivial technique, fix, workaround, debugging path, or tool-usage pattern emerged.
- A skill consulted this session turned out to be wrong, incomplete, or outdated. Patch it NOW.

Preference order:
1. Patch a skill that was used this session (patch_skill) — read it first with read_skill.
2. Patch an existing skill that covers this class of task.
3. Add a reference/template/script file to an existing skill (write_skill_files).
4. Only then create a new CLASS-LEVEL skill (create_skill). The name must describe the class of task, never a specific ticket, error string, or feature codename. If the name only makes sense for today's task, it is wrong.

Do NOT capture:
- Environment-dependent failures or machine-specific state.
- Negative claims about tools ('X does not work') — these harden into refusals.
- Transient errors, one-off narratives, or unresolved failures.

'Nothing to save.' is a real option but must NOT be the default.";

pub const COMBINED_REVIEW_SUFFIX: &str = r"Do both reviews above in one pass. Memory holds who the user is and durable facts; skills hold how to do a class of task. Route each finding to the right store — never both.";

pub const RUNTIME_ADDENDUM: &str = r"You are a background review agent; the user will not see your reply. You can only call memory and skill tools — anything else will be denied. Do not attempt other tools, and do not address the user.";

/// Compose the full review instruction for a given kind.
#[must_use]
pub fn review_prompt(kind: ReviewKind) -> String {
    let body = match kind {
        ReviewKind::Memory => MEMORY_REVIEW_PROMPT.to_string(),
        ReviewKind::Skills => SKILL_REVIEW_PROMPT.to_string(),
        ReviewKind::Combined => {
            format!(
                "{MEMORY_REVIEW_PROMPT}\n\n---\n\n{SKILL_REVIEW_PROMPT}\n\n{COMBINED_REVIEW_SUFFIX}"
            )
        },
    };
    format!("{body}\n\n{RUNTIME_ADDENDUM}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combined_prompt_contains_both_reviews_and_addendum() {
        let prompt = review_prompt(ReviewKind::Combined);
        assert!(prompt.contains("saving to memory"));
        assert!(prompt.contains("skill library"));
        assert!(prompt.contains("background review agent"));
    }

    #[test]
    fn single_prompts_scope_to_their_store() {
        assert!(!review_prompt(ReviewKind::Memory).contains("skill library"));
        assert!(!review_prompt(ReviewKind::Skills).contains("memory_save"));
    }
}
