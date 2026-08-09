//! Deterministic triggers for the background self-review fork.
//!
//! No LLM judges "was this task worth learning from" — plain counters do,
//! keeping learning cost predictable. A counter accumulates while the agent
//! works and resets whenever the corresponding durable state is written
//! (either by the foreground agent or by a completed review), so reviews fire
//! only after a stretch of activity that produced no persisted learning.

use serde::{Deserialize, Serialize};

/// Tools whose successful use counts as a foreground memory write.
pub const MEMORY_WRITE_TOOLS: &[&str] = &["memory_save", "memory_delete", "memory_forget"];

/// Tools whose successful use counts as a foreground skill write.
pub const SKILL_WRITE_TOOLS: &[&str] = &[
    "create_skill",
    "update_skill",
    "patch_skill",
    "write_skill_files",
    "delete_skill",
];

#[must_use]
pub fn is_memory_write_tool(name: &str) -> bool {
    MEMORY_WRITE_TOOLS.contains(&name)
}

#[must_use]
pub fn is_skill_write_tool(name: &str) -> bool {
    SKILL_WRITE_TOOLS.contains(&name)
}

/// Which review the fork should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewKind {
    Memory,
    Skills,
    Combined,
}

/// Per-session review counters, persisted across turns (and process restarts)
/// by the caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LearningCounters {
    /// Completed turns since a memory write (foreground or review).
    pub turns_since_memory_write: u32,
    /// Tool-loop iterations since a skill write (foreground or review).
    pub iters_since_skill_write: u32,
}

impl LearningCounters {
    /// Record one completed turn.
    ///
    /// `tool_iterations` is the number of tool-loop iterations the turn ran;
    /// `wrote_memory` / `wrote_skill` report successful foreground writes,
    /// which reset the corresponding counter (the agent already persisted its
    /// learning — no review needed yet).
    pub fn record_turn(&mut self, tool_iterations: u32, wrote_memory: bool, wrote_skill: bool) {
        if wrote_memory {
            self.turns_since_memory_write = 0;
        } else {
            self.turns_since_memory_write = self.turns_since_memory_write.saturating_add(1);
        }
        if wrote_skill {
            self.iters_since_skill_write = 0;
        } else {
            self.iters_since_skill_write =
                self.iters_since_skill_write.saturating_add(tool_iterations);
        }
    }

    /// Which review (if any) is due. An interval of `0` disables that review.
    #[must_use]
    pub fn due(&self, memory_interval: u32, skill_interval: u32) -> Option<ReviewKind> {
        let memory_due = memory_interval > 0 && self.turns_since_memory_write >= memory_interval;
        let skills_due = skill_interval > 0 && self.iters_since_skill_write >= skill_interval;
        match (memory_due, skills_due) {
            (true, true) => Some(ReviewKind::Combined),
            (true, false) => Some(ReviewKind::Memory),
            (false, true) => Some(ReviewKind::Skills),
            (false, false) => None,
        }
    }

    /// Reset the counters covered by a completed (spawned) review.
    pub fn reset_for(&mut self, kind: ReviewKind) {
        match kind {
            ReviewKind::Memory => self.turns_since_memory_write = 0,
            ReviewKind::Skills => self.iters_since_skill_write = 0,
            ReviewKind::Combined => *self = Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_and_reset_on_foreground_writes() {
        let mut counters = LearningCounters::default();
        counters.record_turn(3, false, false);
        counters.record_turn(2, false, false);
        assert_eq!(counters.turns_since_memory_write, 2);
        assert_eq!(counters.iters_since_skill_write, 5);

        counters.record_turn(4, true, false);
        assert_eq!(counters.turns_since_memory_write, 0);
        assert_eq!(counters.iters_since_skill_write, 9);

        counters.record_turn(1, false, true);
        assert_eq!(counters.turns_since_memory_write, 1);
        assert_eq!(counters.iters_since_skill_write, 0);
    }

    #[test]
    fn due_maps_to_review_kind() {
        let counters = LearningCounters {
            turns_since_memory_write: 10,
            iters_since_skill_write: 3,
        };
        assert_eq!(counters.due(10, 10), Some(ReviewKind::Memory));
        assert_eq!(counters.due(10, 3), Some(ReviewKind::Combined));
        assert_eq!(counters.due(20, 3), Some(ReviewKind::Skills));
        assert_eq!(counters.due(20, 20), None);
    }

    #[test]
    fn zero_interval_disables_review() {
        let counters = LearningCounters {
            turns_since_memory_write: 100,
            iters_since_skill_write: 100,
        };
        assert_eq!(counters.due(0, 0), None);
        assert_eq!(counters.due(0, 10), Some(ReviewKind::Skills));
        assert_eq!(counters.due(10, 0), Some(ReviewKind::Memory));
    }

    #[test]
    fn reset_for_clears_only_covered_counters() {
        let mut counters = LearningCounters {
            turns_since_memory_write: 7,
            iters_since_skill_write: 9,
        };
        counters.reset_for(ReviewKind::Memory);
        assert_eq!(counters.turns_since_memory_write, 0);
        assert_eq!(counters.iters_since_skill_write, 9);

        counters.reset_for(ReviewKind::Combined);
        assert_eq!(counters, LearningCounters::default());
    }

    #[test]
    fn tool_classification() {
        assert!(is_memory_write_tool("memory_save"));
        assert!(!is_memory_write_tool("memory_search"));
        assert!(is_skill_write_tool("patch_skill"));
        assert!(!is_skill_write_tool("read_skill"));
    }

    #[test]
    fn counters_survive_serde_roundtrip() {
        let counters = LearningCounters {
            turns_since_memory_write: 4,
            iters_since_skill_write: 11,
        };
        let json = serde_json::to_value(counters).unwrap_or_default();
        let back: LearningCounters = serde_json::from_value(json).unwrap_or_default();
        assert_eq!(back, counters);
    }
}
