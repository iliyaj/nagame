// SPDX-FileCopyrightText: 2026 iliyaj
// SPDX-License-Identifier: GPL-3.0-or-later

//! Pure state transitions for temporary display previews.

use tokio::time::Instant;

pub struct PendingPreview<T> {
    pub id: String,
    pub client_id: u64,
    pub deadline: Instant,
    pub payload: T,
}

pub enum RevertRequest<T> {
    Restore(PendingPreview<T>),
    AlreadyCompleted,
    NoPending,
    Mismatch,
}

pub struct PreviewState<T> {
    pending: Option<PendingPreview<T>>,
    last_completed_id: Option<String>,
    next_transaction: u64,
}

impl<T> Default for PreviewState<T> {
    fn default() -> Self {
        Self {
            pending: None,
            last_completed_id: None,
            next_transaction: 1,
        }
    }
}

impl<T> PreviewState<T> {
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.pending.as_ref().map(|preview| preview.deadline)
    }

    pub fn start(&mut self, client_id: u64, deadline: Instant, payload: T) -> Option<String> {
        if self.pending.is_some() {
            return None;
        }

        let transaction = self.next_transaction;
        self.next_transaction = self
            .next_transaction
            .checked_add(1)
            .expect("display preview transaction counter exhausted");
        let id = format!("preview-{transaction}");
        self.pending = Some(PendingPreview {
            id: id.clone(),
            client_id,
            deadline,
            payload,
        });
        Some(id)
    }

    pub fn request_revert(&mut self, transaction_id: &str) -> RevertRequest<T> {
        if self
            .pending
            .as_ref()
            .is_some_and(|preview| preview.id == transaction_id)
        {
            return RevertRequest::Restore(
                self.pending
                    .take()
                    .expect("matching display preview must still be pending"),
            );
        }

        if self.last_completed_id.as_deref() == Some(transaction_id) {
            return RevertRequest::AlreadyCompleted;
        }

        if self.pending.is_some() {
            RevertRequest::Mismatch
        } else {
            RevertRequest::NoPending
        }
    }

    pub fn take_for_client(&mut self, client_id: u64) -> Option<PendingPreview<T>> {
        if self
            .pending
            .as_ref()
            .is_some_and(|preview| preview.client_id == client_id)
        {
            self.pending.take()
        } else {
            None
        }
    }

    pub fn take_if_expired(&mut self, now: Instant) -> Option<PendingPreview<T>> {
        if self
            .pending
            .as_ref()
            .is_some_and(|preview| preview.deadline <= now)
        {
            self.pending.take()
        } else {
            None
        }
    }

    pub fn take_pending(&mut self) -> Option<PendingPreview<T>> {
        self.pending.take()
    }

    pub fn retry(&mut self, preview: PendingPreview<T>) {
        debug_assert!(self.pending.is_none());
        self.pending = Some(preview);
    }

    pub fn complete(&mut self, transaction_id: String) {
        self.last_completed_id = Some(transaction_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    fn deadline(seconds: u64) -> Instant {
        Instant::now() + Duration::from_secs(seconds)
    }

    #[test]
    fn refuses_a_second_pending_preview() {
        let mut state = PreviewState::default();
        assert_eq!(state.start(1, deadline(15), ()), Some("preview-1".into()));
        assert_eq!(state.start(2, deadline(15), ()), None);
    }

    #[test]
    fn matching_revert_takes_pending_preview() {
        let mut state = PreviewState::default();
        let id = state.start(1, deadline(15), "before").unwrap();

        match state.request_revert(&id) {
            RevertRequest::Restore(preview) => assert_eq!(preview.payload, "before"),
            _ => panic!("matching transaction was not selected for restore"),
        }
        assert!(!state.is_pending());
    }

    #[test]
    fn mismatched_revert_preserves_pending_preview() {
        let mut state = PreviewState::default();
        state.start(1, deadline(15), ()).unwrap();

        assert!(matches!(
            state.request_revert("preview-999"),
            RevertRequest::Mismatch
        ));
        assert!(state.is_pending());
    }

    #[test]
    fn disconnect_only_takes_the_owners_preview() {
        let mut state = PreviewState::default();
        state.start(7, deadline(15), ()).unwrap();

        assert!(state.take_for_client(8).is_none());
        assert!(state.is_pending());
        assert!(state.take_for_client(7).is_some());
        assert!(!state.is_pending());
    }

    #[test]
    fn timeout_only_takes_an_expired_preview() {
        let now = Instant::now();
        let mut state = PreviewState::default();
        state.start(1, now + Duration::from_secs(1), ()).unwrap();

        assert!(state.take_if_expired(now).is_none());
        assert!(state
            .take_if_expired(now + Duration::from_secs(1))
            .is_some());
    }

    #[test]
    fn completed_revert_is_idempotent_without_touching_a_new_preview() {
        let mut state = PreviewState::default();
        let completed = state.start(1, deadline(15), ()).unwrap();
        let old = match state.request_revert(&completed) {
            RevertRequest::Restore(preview) => preview,
            _ => panic!("preview was not selected for restore"),
        };
        state.complete(old.id);
        let current = state.start(2, deadline(15), ()).unwrap();

        assert!(matches!(
            state.request_revert(&completed),
            RevertRequest::AlreadyCompleted
        ));
        assert!(state.is_pending());
        assert!(matches!(
            state.request_revert(&current),
            RevertRequest::Restore(_)
        ));
    }

    #[test]
    fn no_pending_preview_is_distinct_from_a_mismatch() {
        let mut state = PreviewState::<()>::default();
        assert!(matches!(
            state.request_revert("preview-1"),
            RevertRequest::NoPending
        ));
    }
}
