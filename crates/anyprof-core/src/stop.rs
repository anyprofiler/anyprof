use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

const PHASE_RECORDING: u8 = 0;
const PHASE_STOPPING: u8 = 1;

/// What one Ctrl+C / SIGTERM should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalAction {
    /// End sampling and still write the profile.
    ///
    /// This is every signal that arrives while samples are being collected, and
    /// the first signal that arrives during stop/save.
    StopSampling,
    /// A further signal once stop/save has started. The process should exit
    /// immediately and not wait for the profile to be written.
    ForceExit,
}

/// Cooperative cancel flag shared by the CLI and a sampler.
///
/// The first interrupt asks the sampler to stop and return a profile. A second
/// interrupt during [`StopGate::begin_stopping`] asks the CLI to exit without
/// waiting. Signals that arrive before stop/save do not force an exit, so a
/// double Ctrl+C while samples are still coming in still saves the profile.
#[derive(Debug)]
pub struct StopGate {
    stop: AtomicBool,
    phase: AtomicU8,
    signals: AtomicU32,
}

impl StopGate {
    /// A gate that has not been signaled.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            phase: AtomicU8::new(PHASE_RECORDING),
            signals: AtomicU32::new(0),
        })
    }

    /// Whether sampling should end at the next check.
    #[must_use]
    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Mark the start of `Profiler.stop` and the following save.
    ///
    /// Call this before stopping the target profiler so a second interrupt
    /// during that work force-exits instead of waiting.
    pub fn begin_stopping(&self) {
        self.phase.store(PHASE_STOPPING, Ordering::SeqCst);
    }

    /// Record one SIGINT, SIGTERM, or console Ctrl+C / Ctrl+Break.
    pub fn note_signal(&self) -> SignalAction {
        let previous = self.signals.fetch_add(1, Ordering::SeqCst);
        self.stop.store(true, Ordering::SeqCst);
        if previous >= 1 && self.phase.load(Ordering::SeqCst) == PHASE_STOPPING {
            SignalAction::ForceExit
        } else {
            SignalAction::StopSampling
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SignalAction, StopGate};

    #[test]
    fn first_signal_stops_sampling_and_a_second_during_save_force_exits() {
        let gate = StopGate::new();
        assert!(!gate.stop_requested());
        assert_eq!(gate.note_signal(), SignalAction::StopSampling);
        assert!(gate.stop_requested());
        // Still recording: another Ctrl+C keeps the "save this profile" path.
        assert_eq!(gate.note_signal(), SignalAction::StopSampling);

        let during_save = StopGate::new();
        assert_eq!(during_save.note_signal(), SignalAction::StopSampling);
        during_save.begin_stopping();
        assert_eq!(during_save.note_signal(), SignalAction::ForceExit);
    }

    #[test]
    fn a_signal_during_save_after_a_natural_end_does_not_abort_on_the_first() {
        let gate = StopGate::new();
        gate.begin_stopping();
        assert_eq!(gate.note_signal(), SignalAction::StopSampling);
        assert_eq!(gate.note_signal(), SignalAction::ForceExit);
    }
}
