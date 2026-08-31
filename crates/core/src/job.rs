//! Inputs entering the local job pipeline.

use crate::{TextContext, TriggerKind};

// Kept available from `selection_core::job` as well as the crate root. The
// definition remains in `request_gate`, which is the only construction site.
pub use crate::request_gate::PreparedRequest;

/// A job is identified by a monotonically increasing id owned by the
/// coordinator. The id is checked again immediately before provider work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobInput {
    pub id: u64,
    pub trigger: TriggerKind,
    pub text: TextContext,
    pub prompt_id: String,
}

impl JobInput {
    pub fn new(
        id: u64,
        trigger: TriggerKind,
        text: TextContext,
        prompt_id: impl Into<String>,
    ) -> Self {
        Self {
            id,
            trigger,
            text,
            prompt_id: prompt_id.into(),
        }
    }
}
