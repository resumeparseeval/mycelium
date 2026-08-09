//! The learning loop: counter-triggered background self-review.
//!
//! See `counters` for the deterministic triggers, `prompts` for the review
//! instructions, and `review` for the tool-whitelisted fork runner.

pub mod counters;
pub mod prompts;
pub mod review;

pub use {
    counters::{LearningCounters, ReviewKind, is_memory_write_tool, is_skill_write_tool},
    review::{DEFAULT_REVIEW_MAX_ITERATIONS, ReviewOutcome, run_learning_review},
};
