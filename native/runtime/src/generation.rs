//! Shared cancellation fence, also exercised by the model-free speech fixture.
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub struct Generation {
    epoch: u64,
    token: Arc<AtomicBool>,
    blocked: bool,
    activity: u64,
}
impl Default for Generation {
    fn default() -> Self {
        Self {
            epoch: 0,
            token: Arc::new(AtomicBool::new(true)),
            blocked: false,
            activity: 0,
        }
    }
}
impl Generation {
    /// Immediate capture-thread fence. Until the controller processes that user
    /// activity and advances the epoch, even newly arriving old-turn chunks are blocked.
    #[cfg(any(test, feature = "native-audio"))]
    pub fn invalidate(&mut self, activity: u64) {
        self.blocked = true;
        self.activity = activity;
        self.token.store(true, Ordering::Release);
    }
    pub fn cancel(&mut self, epoch: u64) {
        self.token.store(true, Ordering::Release);
        self.epoch = epoch;
        self.token = Arc::new(AtomicBool::new(true));
        // Only a processed Hearing event may acknowledge the capture-thread fence.
    }
    pub fn acknowledge_user_activity(&mut self, activity: u64) {
        if self.activity == activity {
            self.blocked = false;
        }
    }
    pub fn can_drain(&self, epoch: u64) -> bool {
        self.epoch == epoch && !self.blocked
    }
    pub fn token(&mut self, epoch: u64) -> Option<Arc<AtomicBool>> {
        if self.blocked || self.epoch != epoch {
            return None;
        }
        if self.token.load(Ordering::Acquire) {
            self.token = Arc::new(AtomicBool::new(false));
        }
        Some(self.token.clone())
    }
    pub fn allows(&self, epoch: u64, token: &AtomicBool) -> bool {
        !self.blocked && self.epoch == epoch && !token.load(Ordering::Acquire)
    }
    #[cfg(feature = "native-audio")]
    pub fn busy(&self) -> bool {
        !self.token.load(Ordering::Acquire)
    }
    #[cfg(feature = "native-audio")]
    pub fn active_token(&self, epoch: u64) -> Option<Arc<AtomicBool>> {
        (self.can_drain(epoch) && self.busy()).then(|| self.token.clone())
    }
    pub fn finish(&self) {
        self.token.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_cannot_be_undone_by_an_old_queued_chunk() {
        let mut fence = Generation::default();
        fence.cancel(1);
        let old = fence.token(1).unwrap();
        fence.invalidate(1);
        assert!(fence.token(1).is_none());
        assert!(!fence.allows(1, &old));
        fence.cancel(2);
        assert!(fence.token(2).is_none());
        fence.acknowledge_user_activity(1);
        let new = fence.token(2).unwrap();
        assert!(fence.allows(2, &new));
        assert!(!fence.allows(1, &old));
    }
    #[test]
    fn older_hearing_cannot_acknowledge_newer_capture_activity() {
        let mut fence = Generation::default();
        fence.invalidate(2);
        fence.cancel(3);
        fence.acknowledge_user_activity(1);
        assert!(fence.token(3).is_none());
        fence.acknowledge_user_activity(2);
        assert!(fence.token(3).is_some());
    }
}
