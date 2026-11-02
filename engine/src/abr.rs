//! Deliberately conservative AIMD. Successful UDP sends are NOT proof that the
//! receiver has bandwidth. Local-only recovery is slow and capped at the
//! initial setting, and is explicitly marked unverified.

pub const MIN_KBPS: u32 = 1500;
pub const INITIAL_KBPS: u32 = 4000;
pub const MAX_KBPS: u32 = 8000;

/// Local send pressure is evidence of a congested sender, not proof of remote
/// packet loss. Adapt in one-second windows rather than per RTP packet.
pub struct LocalPressure {
    bad_seconds: u8,
    clean_seconds: u16,
    cooldown: u8,
}

impl LocalPressure {
    pub fn new() -> Self {
        Self {
            bad_seconds: 0,
            clean_seconds: 0,
            cooldown: 0,
        }
    }

    pub fn observe(
        &mut self,
        attempts: u64,
        failed: u64,
        controller: &mut Controller,
        fresh_feedback: bool,
    ) -> Option<(&'static str, u32)> {
        self.cooldown = self.cooldown.saturating_sub(1);
        // A few blocked datagrams are normal bursts; require both a count and
        // a rate, sustained over three consecutive windows.
        let bad = failed >= 5 && failed.saturating_mul(100) >= attempts.max(1).saturating_mul(2);
        if bad {
            self.clean_seconds = 0;
            self.bad_seconds = self.bad_seconds.saturating_add(1);
            if self.bad_seconds >= 3 && self.cooldown == 0 {
                self.bad_seconds = 0;
                self.cooldown = 10;
                return controller.reduce().map(|value| ("local_pressure", value));
            }
        } else {
            self.bad_seconds = 0;
            // Tolerate at most 1% local drops in each one-second window.
            // These remain local signals, not evidence of receiver delivery.
            if failed.saturating_mul(100) <= attempts && attempts >= 10 && !fresh_feedback {
                self.clean_seconds = self.clean_seconds.saturating_add(1);
                // Recovery is deliberately slow and capped at the initial
                // conservative setting. Successful local sends don't prove
                // delivery, so never call this remote congestion control.
                if self.clean_seconds >= 30 && self.cooldown == 0 && controller.kbps < INITIAL_KBPS
                {
                    self.clean_seconds = 0;
                    self.cooldown = 10;
                    return controller
                        .update((controller.kbps + 250).min(INITIAL_KBPS))
                        .map(|value| ("local_recovery_unverified", value));
                }
            } else {
                self.clean_seconds = 0;
            }
        }
        None
    }
}

pub struct Controller {
    pub kbps: u32,
    bad_reports: u8,
    good_reports: u8,
    pub receiver_feedback: bool,
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            kbps: INITIAL_KBPS,
            bad_reports: 0,
            good_reports: 0,
            receiver_feedback: false,
        }
    }
}

impl Controller {
    pub fn report(&mut self, loss: f64, jitter_ms: f64) -> Option<u32> {
        if !loss.is_finite() || !jitter_ms.is_finite() {
            return None;
        }
        self.receiver_feedback = true;
        if loss >= 0.03 || jitter_ms >= 100.0 {
            self.good_reports = 0;
            self.bad_reports = self.bad_reports.saturating_add(1);
            if self.bad_reports >= 2 {
                self.bad_reports = 0;
                return self.reduce();
            }
        } else {
            self.bad_reports = 0;
            if loss < 0.005 && jitter_ms < 40.0 {
                self.good_reports = self.good_reports.saturating_add(1);
                if self.good_reports >= 10 {
                    self.good_reports = 0;
                    return self.update((self.kbps + 250).min(MAX_KBPS));
                }
            } else {
                self.good_reports = 0;
            }
        }
        None
    }

    pub fn reduce(&mut self) -> Option<u32> {
        self.good_reports = 0;
        self.update((self.kbps * 3 / 4).max(MIN_KBPS))
    }

    fn update(&mut self, next: u32) -> Option<u32> {
        if self.kbps == next {
            None
        } else {
            self.kbps = next;
            Some(next)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn congestion_reduces_and_recovery_is_slow() {
        let mut c = Controller::default();
        assert_eq!(c.report(0.1, 0.0), None);
        assert_eq!(c.report(0.1, 0.0), Some(3000));
        for _ in 0..9 {
            assert_eq!(c.report(0.0, 10.0), None);
        }
        assert_eq!(c.report(0.0, 10.0), Some(3250));
    }

    #[test]
    fn limits_and_no_fabricated_feedback() {
        let mut c = Controller::default();
        assert!(!c.receiver_feedback);
        assert_eq!(c.report(f64::NAN, 0.0), None);
        assert!(!c.receiver_feedback);
        for _ in 0..100 {
            c.reduce();
        }
        assert_eq!(c.kbps, MIN_KBPS);
        for _ in 0..1000 {
            c.report(0.0, 0.0);
        }
        assert_eq!(c.kbps, MAX_KBPS);
    }

    #[test]
    fn isolated_bursts_do_not_lower_bitrate_and_sustained_pressure_does() {
        let mut c = Controller::default();
        let mut p = LocalPressure::new();
        for _ in 0..30 {
            assert!(p.observe(100, 5, &mut c, false).is_none());
            assert!(p.observe(100, 0, &mut c, false).is_none());
        }
        assert_eq!(c.kbps, 4000);
        assert!(p.observe(100, 10, &mut c, false).is_none());
        assert!(p.observe(100, 10, &mut c, false).is_none());
        assert_eq!(
            p.observe(100, 10, &mut c, false),
            Some(("local_pressure", 3000))
        );
        for _ in 0..9 {
            assert!(p.observe(100, 10, &mut c, false).is_none());
        }
        assert_eq!(c.kbps, 3000);
    }

    #[test]
    fn local_recovery_is_slow_capped_and_never_claims_receiver_feedback() {
        let mut c = Controller::default();
        let mut p = LocalPressure::new();
        c.reduce();
        for _ in 0..29 {
            assert!(p.observe(100, 1, &mut c, false).is_none());
        }
        assert_eq!(
            p.observe(100, 1, &mut c, false),
            Some(("local_recovery_unverified", 3250))
        );
        for _ in 0..1000 {
            p.observe(100, 0, &mut c, false);
        }
        assert_eq!(c.kbps, INITIAL_KBPS);
        assert!(!c.receiver_feedback);
    }

    #[test]
    fn excessive_drops_or_recent_feedback_interrupt_local_recovery() {
        let mut c = Controller::default();
        c.reduce();
        let mut p = LocalPressure::new();
        for _ in 0..29 {
            assert!(p.observe(100, 1, &mut c, false).is_none());
        }
        assert!(p.observe(100, 2, &mut c, false).is_none());
        for _ in 0..29 {
            assert!(p.observe(100, 0, &mut c, false).is_none());
        }
        assert!(p.observe(100, 0, &mut c, true).is_none());
        for _ in 0..29 {
            assert!(p.observe(100, 0, &mut c, false).is_none());
        }
        assert_eq!(
            p.observe(100, 0, &mut c, false),
            Some(("local_recovery_unverified", 3250))
        );
    }
}
