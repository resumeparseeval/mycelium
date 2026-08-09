use serde::{Deserialize, Serialize};

/// Learning-loop configuration: the counter-triggered background self-review
/// that persists memory entries and skill updates after working sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LearningConfig {
    /// Master switch for the background self-review fork.
    #[serde(default = "default_learning_enabled")]
    pub enabled: bool,
    /// Completed turns without a memory write before a memory review fires.
    /// `0` disables memory reviews.
    #[serde(default = "default_memory_review_interval")]
    pub memory_review_interval: u32,
    /// Tool-loop iterations without a skill write before a skill review
    /// fires. `0` disables skill reviews.
    #[serde(default = "default_skill_review_interval")]
    pub skill_review_interval: u32,
    /// Iteration cap for one review run.
    #[serde(default = "default_max_review_iterations")]
    pub max_review_iterations: usize,
}

fn default_learning_enabled() -> bool {
    true
}

fn default_memory_review_interval() -> u32 {
    10
}

fn default_skill_review_interval() -> u32 {
    10
}

fn default_max_review_iterations() -> usize {
    16
}

impl Default for LearningConfig {
    fn default() -> Self {
        Self {
            enabled: default_learning_enabled(),
            memory_review_interval: default_memory_review_interval(),
            skill_review_interval: default_skill_review_interval(),
            max_review_iterations: default_max_review_iterations(),
        }
    }
}
