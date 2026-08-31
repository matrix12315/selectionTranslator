//! Local trigger coordination.
//!
//! This module deliberately stops at creating and cancelling local jobs.  It
//! does not know about extractors, a provider, or a network connection.  The
//! platform composition root can therefore use it while those later tasks are
//! still being implemented.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use crate::normalize::{normalize_optional, normalize_target};
use crate::{JobInput, ScreenRect, TextContext, TriggerKind};

pub const AUTOMATIC_DUPLICATE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum JobPriority {
    Hover = 0,
    Selection = 1,
    Manual = 2,
}

impl From<TriggerKind> for JobPriority {
    fn from(trigger: TriggerKind) -> Self {
        match trigger {
            TriggerKind::Hover => Self::Hover,
            TriggerKind::Selection => Self::Selection,
            TriggerKind::Manual => Self::Manual,
        }
    }
}

/// Cooperative cancellation shared by local extraction and streaming work.
/// The platform provider may copy the cancellation state into its own token.
#[derive(Clone, Debug, Default)]
pub struct JobCancellation(Arc<AtomicBool>);

impl JobCancellation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug)]
pub struct JobHandle {
    pub input: JobInput,
    pub cancellation: JobCancellation,
    pub priority: JobPriority,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobStartRejection {
    DuplicateAutomatic,
    LowerPriorityActive,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobStart {
    Started(u64),
    Rejected(JobStartRejection),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completion {
    Accepted,
    Stale,
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TextFingerprint {
    pub process_id: u32,
    pub target: String,
    pub context: Option<String>,
    pub screen_rect: Option<ScreenRect>,
}

impl TextFingerprint {
    pub fn new(process_id: u32, text: &TextContext) -> Self {
        Self {
            process_id,
            target: normalize_target(&text.target),
            context: normalize_optional(text.context.as_deref()),
            screen_rect: text.screen_rect,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.target.is_empty()
    }
}

#[derive(Clone, Debug)]
struct RecentAutomatic {
    fingerprint: TextFingerprint,
    completed_at: Instant,
}

/// Owns monotonic job IDs and the one active extraction/streaming generation.
#[derive(Debug)]
pub struct Coordinator {
    next_id: u64,
    active: Option<JobHandle>,
    recent_automatic: Option<RecentAutomatic>,
}

impl Coordinator {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            ..Self::default()
        }
    }

    pub fn active_job_id(&self) -> Option<u64> {
        self.active.as_ref().map(|job| job.input.id)
    }
    pub fn active(&self) -> Option<&JobHandle> {
        self.active.as_ref()
    }

    /// Starts a local job after extraction has produced its text context.
    /// Automatic duplicate suppression is intentionally applied only to
    /// Selection and Hover; a Manual hotkey always means “try again”.
    pub fn start(
        &mut self,
        trigger: TriggerKind,
        process_id: u32,
        text: TextContext,
        prompt_id: impl Into<String>,
        now: Instant,
    ) -> Result<JobHandle, JobStartRejection> {
        self.start_with_duplicate_suppression(trigger, process_id, text, prompt_id, now, true)
    }

    /// Check whether `start` would accept this candidate without cancelling
    /// or otherwise mutating the current job. Platform code uses this before
    /// allocating a result surface so replacement is transactional.
    pub fn can_start(
        &self,
        trigger: TriggerKind,
        process_id: u32,
        text: &TextContext,
        now: Instant,
    ) -> Result<(), JobStartRejection> {
        self.can_start_with_duplicate_suppression(trigger, process_id, text, now, true)
    }

    /// Starts a job explicitly requested by the user (for example Retry or
    /// Prompt). Explicit requests still go through the request gate later,
    /// but must not be rejected by the automatic duplicate fingerprint.
    pub fn start_explicit(
        &mut self,
        trigger: TriggerKind,
        process_id: u32,
        text: TextContext,
        prompt_id: impl Into<String>,
        now: Instant,
    ) -> Result<JobHandle, JobStartRejection> {
        self.start_with_duplicate_suppression(trigger, process_id, text, prompt_id, now, false)
    }

    /// Non-mutating counterpart to `start_explicit`.
    pub fn can_start_explicit(
        &self,
        trigger: TriggerKind,
        process_id: u32,
        text: &TextContext,
        now: Instant,
    ) -> Result<(), JobStartRejection> {
        self.can_start_with_duplicate_suppression(trigger, process_id, text, now, false)
    }

    fn can_start_with_duplicate_suppression(
        &self,
        trigger: TriggerKind,
        process_id: u32,
        text: &TextContext,
        now: Instant,
        suppress_automatic_duplicates: bool,
    ) -> Result<(), JobStartRejection> {
        let fingerprint = TextFingerprint::new(process_id, text);
        let automatic = trigger != TriggerKind::Manual;
        if suppress_automatic_duplicates && automatic && self.is_duplicate(&fingerprint, now) {
            return Err(JobStartRejection::DuplicateAutomatic);
        }

        let priority = JobPriority::from(trigger);
        if self
            .active
            .as_ref()
            .is_some_and(|active| priority < active.priority && !active.cancellation.is_cancelled())
        {
            return Err(JobStartRejection::LowerPriorityActive);
        }
        Ok(())
    }

    fn start_with_duplicate_suppression(
        &mut self,
        trigger: TriggerKind,
        process_id: u32,
        text: TextContext,
        prompt_id: impl Into<String>,
        now: Instant,
        suppress_automatic_duplicates: bool,
    ) -> Result<JobHandle, JobStartRejection> {
        self.can_start_with_duplicate_suppression(
            trigger,
            process_id,
            &text,
            now,
            suppress_automatic_duplicates,
        )?;

        let priority = JobPriority::from(trigger);
        if let Some(active) = &self.active {
            active.cancellation.cancel();
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let handle = JobHandle {
            input: JobInput::new(id, trigger, text, prompt_id),
            cancellation: JobCancellation::new(),
            priority,
        };
        self.active = Some(handle.clone());
        Ok(handle)
    }

    /// Records a successful automatic result.  A completion from an older
    /// generation is ignored, so late extraction/provider work cannot mutate
    /// current state or refresh duplicate suppression.
    pub fn complete(
        &mut self,
        job_id: u64,
        process_id: u32,
        text: &TextContext,
        now: Instant,
    ) -> Completion {
        let Some(active) = self.active.as_ref() else {
            return Completion::Stale;
        };
        if active.input.id != job_id {
            return Completion::Stale;
        }
        if active.cancellation.is_cancelled() {
            return Completion::Cancelled;
        }
        if active.input.trigger != TriggerKind::Manual {
            let fingerprint = TextFingerprint::new(process_id, text);
            if !fingerprint.is_empty() {
                self.recent_automatic = Some(RecentAutomatic {
                    fingerprint,
                    completed_at: now,
                });
            }
        }
        // Duplicate suppression is independent metadata. The active job is
        // terminal after a successful result and must not block later jobs.
        self.active.take();
        Completion::Accepted
    }

    /// Clears a terminal job without recording an automatic success. This is
    /// used for provider errors, empty responses, cancellation, and popup
    /// dismissal. Late worker completions then become stale.
    pub fn finish(&mut self, job_id: u64) -> Completion {
        let Some(active) = self.active.as_ref() else {
            return Completion::Stale;
        };
        if active.input.id != job_id {
            return Completion::Stale;
        }
        let was_cancelled = active.cancellation.is_cancelled();
        self.active.take();
        if was_cancelled {
            Completion::Cancelled
        } else {
            Completion::Accepted
        }
    }

    /// Marks the active job as no longer current and cancels its workers.
    pub fn cancel_active(&mut self) -> Option<u64> {
        let active = self.active.take()?;
        let id = active.input.id;
        active.cancellation.cancel();
        Some(id)
    }

    pub fn is_current(&self, job_id: u64) -> bool {
        self.active
            .as_ref()
            .is_some_and(|job| job.input.id == job_id && !job.cancellation.is_cancelled())
    }

    fn is_duplicate(&self, fingerprint: &TextFingerprint, now: Instant) -> bool {
        let Some(recent) = &self.recent_automatic else {
            return false;
        };
        if now.saturating_duration_since(recent.completed_at) > AUTOMATIC_DUPLICATE_TTL {
            return false;
        }
        !fingerprint.is_empty() && &recent.fingerprint == fingerprint
    }
}

impl Default for Coordinator {
    fn default() -> Self {
        Self {
            next_id: 1,
            active: None,
            recent_automatic: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(target: &str, rect: Option<ScreenRect>) -> TextContext {
        TextContext {
            target: target.into(),
            context: Some(" context ".into()),
            source: crate::ExtractionSource::UiaSelection,
            screen_rect: rect,
        }
    }

    #[test]
    fn priority_cancels_lower_and_rejects_lower_while_manual_is_active() {
        let mut coordinator = Coordinator::new();
        let hover = coordinator
            .start(
                TriggerKind::Hover,
                10,
                text("one", None),
                "p",
                Instant::now(),
            )
            .unwrap();
        let selection = coordinator
            .start(
                TriggerKind::Selection,
                10,
                text("two", None),
                "p",
                Instant::now(),
            )
            .unwrap();
        assert!(hover.cancellation.is_cancelled());
        assert!(coordinator
            .start(
                TriggerKind::Manual,
                10,
                text("three", None),
                "p",
                Instant::now()
            )
            .is_ok());
        assert!(selection.cancellation.is_cancelled());
        assert!(matches!(
            coordinator.start(
                TriggerKind::Hover,
                10,
                text("four", None),
                "p",
                Instant::now()
            ),
            Err(JobStartRejection::LowerPriorityActive)
        ));
    }

    #[test]
    fn unchanged_automatic_result_is_suppressed_for_ten_minutes() {
        let at = Instant::now();
        let rect = Some(ScreenRect::new(1, 2, 20, 30));
        let mut coordinator = Coordinator::new();
        let first = coordinator
            .start(
                TriggerKind::Selection,
                42,
                text("\u{200b}hello", rect),
                "p",
                at,
            )
            .unwrap();
        assert_eq!(
            coordinator.complete(first.input.id, 42, &first.input.text, at),
            Completion::Accepted
        );
        assert!(matches!(
            coordinator.start(
                TriggerKind::Selection,
                42,
                text(" hello ", rect),
                "p",
                at + Duration::from_secs(60)
            ),
            Err(JobStartRejection::DuplicateAutomatic)
        ));
        assert!(coordinator
            .start(
                TriggerKind::Selection,
                42,
                text(" hello ", rect),
                "p",
                at + AUTOMATIC_DUPLICATE_TTL + Duration::from_secs(1)
            )
            .is_ok());
    }

    #[test]
    fn manual_is_never_suppressed_and_late_completion_is_stale() {
        let at = Instant::now();
        let mut coordinator = Coordinator::new();
        let old = coordinator
            .start(TriggerKind::Selection, 1, text("a", None), "p", at)
            .unwrap();
        let new = coordinator
            .start(TriggerKind::Manual, 1, text("b", None), "p", at)
            .unwrap();
        assert_eq!(
            coordinator.complete(old.input.id, 1, &old.input.text, at),
            Completion::Stale
        );
        assert_eq!(
            coordinator.complete(new.input.id, 1, &new.input.text, at),
            Completion::Accepted
        );
        assert!(coordinator
            .start(TriggerKind::Manual, 1, text("b", None), "p", at)
            .is_ok());
    }

    #[test]
    fn successful_completion_clears_active_but_keeps_duplicate_metadata() {
        let at = Instant::now();
        let rect = Some(ScreenRect::new(1, 2, 3, 4));
        let mut coordinator = Coordinator::new();
        let job = coordinator
            .start(TriggerKind::Selection, 1, text("same", rect), "p", at)
            .unwrap();
        assert_eq!(
            coordinator.complete(job.input.id, 1, &job.input.text, at),
            Completion::Accepted
        );
        assert_eq!(coordinator.active_job_id(), None);
        assert!(matches!(
            coordinator.start(TriggerKind::Selection, 1, text("same", rect), "p", at),
            Err(JobStartRejection::DuplicateAutomatic)
        ));
        assert!(coordinator
            .start(TriggerKind::Selection, 1, text("different", rect), "p", at)
            .is_ok());
    }

    #[test]
    fn explicit_retry_bypasses_duplicate_suppression() {
        let at = Instant::now();
        let mut coordinator = Coordinator::new();
        let first = coordinator
            .start(TriggerKind::Selection, 1, text("same", None), "p", at)
            .unwrap();
        assert_eq!(
            coordinator.complete(first.input.id, 1, &first.input.text, at),
            Completion::Accepted
        );
        let retry = coordinator
            .start_explicit(TriggerKind::Selection, 1, text("same", None), "p", at)
            .expect("explicit retry must bypass automatic duplicate suppression");
        assert_eq!(retry.input.trigger, TriggerKind::Selection);
    }

    #[test]
    fn terminal_finish_clears_active_without_recording_success() {
        let mut coordinator = Coordinator::new();
        let job = coordinator
            .start(
                TriggerKind::Selection,
                1,
                text("same", None),
                "p",
                Instant::now(),
            )
            .unwrap();
        assert_eq!(coordinator.finish(job.input.id), Completion::Accepted);
        assert_eq!(coordinator.active_job_id(), None);
        assert!(coordinator
            .start(
                TriggerKind::Selection,
                1,
                text("same", None),
                "p",
                Instant::now()
            )
            .is_ok());
    }

    #[test]
    fn preflight_acceptance_does_not_replace_or_cancel_current_job() {
        let at = Instant::now();
        let mut coordinator = Coordinator::new();
        let current = coordinator
            .start(TriggerKind::Selection, 1, text("current", None), "p", at)
            .unwrap();

        assert_eq!(
            coordinator.can_start(TriggerKind::Selection, 1, &text("replacement", None), at),
            Ok(())
        );
        assert_eq!(coordinator.active_job_id(), Some(current.input.id));
        assert!(!current.cancellation.is_cancelled());
    }

    #[test]
    fn preflight_matches_duplicate_and_priority_rejections_without_mutation() {
        let at = Instant::now();
        let mut coordinator = Coordinator::new();
        let completed = coordinator
            .start(TriggerKind::Selection, 1, text("duplicate", None), "p", at)
            .unwrap();
        assert_eq!(
            coordinator.complete(completed.input.id, 1, &completed.input.text, at),
            Completion::Accepted
        );
        assert_eq!(
            coordinator.can_start(TriggerKind::Selection, 1, &text("duplicate", None), at),
            Err(JobStartRejection::DuplicateAutomatic)
        );

        let manual = coordinator
            .start(TriggerKind::Manual, 1, text("manual", None), "p", at)
            .unwrap();
        assert_eq!(
            coordinator.can_start(TriggerKind::Selection, 1, &text("other", None), at),
            Err(JobStartRejection::LowerPriorityActive)
        );
        assert_eq!(coordinator.active_job_id(), Some(manual.input.id));
        assert!(!manual.cancellation.is_cancelled());
    }
}
