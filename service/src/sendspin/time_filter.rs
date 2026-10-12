//! Sendspin time filter: a 2-D Kalman filter over (offset, drift) fed by
//! NTP-style `client/time` ↔ `server/time` exchanges.
//!
//! Faithful port of the reference implementation (Sendspin `time-filter`
//! C++ library, mirrored 1:1 by aiosendspin's `time_sync.py`), which the
//! spec requires clients to use. All times are microseconds; the client's
//! clock is our monotonic clock, the server's is whatever it reports.

/// Residual threshold (× max_error) that triggers adaptive forgetting.
const ADAPTIVE_FORGETTING_CUTOFF: f64 = 3.0;
/// Scale applied to max_error before using it as the measurement std-dev.
const MAX_ERROR_SCALE: f64 = 0.5;
/// Drift is applied only when drift² > threshold² · drift_covariance.
const DRIFT_SIGNIFICANCE_THRESHOLD_SQUARED: f64 = 2.0 * 2.0;
/// Samples before adaptive forgetting is enabled.
const MIN_SAMPLES_FOR_FORGETTING: u32 = 100;

#[derive(Clone, Copy, Debug, Default)]
struct TimeElement {
    last_update: i64,
    offset: f64,
    drift: f64,
    use_drift: bool,
}

#[derive(Clone, Debug)]
pub struct TimeFilter {
    last_update: i64,
    count: u32,
    offset: f64,
    drift: f64,
    offset_covariance: f64,
    offset_drift_covariance: f64,
    drift_covariance: f64,
    process_variance: f64,
    drift_process_variance: f64,
    forget_variance_factor: f64,
    current: TimeElement,
}

impl Default for TimeFilter {
    fn default() -> Self {
        Self::new(0.0, 2.0, 1e-11)
    }
}

impl TimeFilter {
    pub fn new(process_std_dev: f64, forget_factor: f64, drift_process_std_dev: f64) -> Self {
        Self {
            last_update: 0,
            count: 0,
            offset: 0.0,
            drift: 0.0,
            offset_covariance: f64::INFINITY,
            offset_drift_covariance: 0.0,
            drift_covariance: 0.0,
            process_variance: process_std_dev * process_std_dev,
            drift_process_variance: drift_process_std_dev * drift_process_std_dev,
            forget_variance_factor: forget_factor * forget_factor,
            current: TimeElement::default(),
        }
    }

    /// Feed one exchange: `measurement = ((T2-T1)+(T3-T4))/2`,
    /// `max_error = ((T4-T1)-(T3-T2))/2`, `time_added = T4` (client clock).
    pub fn update(&mut self, measurement: i64, max_error: i64, time_added: i64) {
        if time_added <= self.last_update {
            return; // non-monotonic: skip
        }
        let dt = (time_added - self.last_update) as f64;
        self.last_update = time_added;
        let update_std_dev = max_error as f64 * MAX_ERROR_SCALE;
        let measurement_variance = update_std_dev * update_std_dev;

        if self.count == 0 {
            self.count = 1;
            self.offset = measurement as f64;
            self.offset_covariance = measurement_variance;
            self.drift = 0.0;
            self.current = TimeElement {
                last_update: self.last_update,
                offset: self.offset,
                drift: 0.0,
                use_drift: false,
            };
            return;
        }
        if self.count == 1 {
            self.count = 2;
            self.drift = (measurement as f64 - self.offset) / dt;
            self.offset = measurement as f64;
            self.drift_covariance = (self.offset_covariance + measurement_variance) / (dt * dt);
            self.offset_covariance = measurement_variance;
            self.current = TimeElement {
                last_update: self.last_update,
                offset: self.offset,
                drift: self.drift,
                use_drift: false,
            };
            return;
        }

        // Predict.
        let offset = self.offset + self.drift * dt;
        let dt2 = dt * dt;
        let mut new_drift_cov = self.drift_covariance + dt * self.drift_process_variance;
        let mut new_offset_drift_cov = self.offset_drift_covariance + self.drift_covariance * dt;
        let mut new_offset_cov = self.offset_covariance
            + 2.0 * self.offset_drift_covariance * dt
            + self.drift_covariance * dt2
            + dt * self.process_variance;

        // Innovation + adaptive forgetting.
        let residual = measurement as f64 - offset;
        let cutoff = max_error as f64 * ADAPTIVE_FORGETTING_CUTOFF;
        if self.count < MIN_SAMPLES_FOR_FORGETTING {
            self.count += 1;
        } else if residual.abs() > cutoff {
            new_drift_cov *= self.forget_variance_factor;
            new_offset_drift_cov *= self.forget_variance_factor;
            new_offset_cov *= self.forget_variance_factor;
        }

        // Update.
        let uncertainty = 1.0 / (new_offset_cov + measurement_variance).max(1e-9);
        let offset_gain = new_offset_cov * uncertainty;
        let drift_gain = new_offset_drift_cov * uncertainty;
        self.offset = offset + offset_gain * residual;
        self.drift += drift_gain * residual;
        self.drift_covariance = new_drift_cov - drift_gain * new_offset_drift_cov;
        self.offset_drift_covariance = new_offset_drift_cov - drift_gain * new_offset_cov;
        self.offset_covariance = new_offset_cov - offset_gain * new_offset_cov;

        let use_drift = self.drift * self.drift > DRIFT_SIGNIFICANCE_THRESHOLD_SQUARED * self.drift_covariance;
        self.current = TimeElement {
            last_update: self.last_update,
            offset: self.offset,
            drift: self.drift,
            use_drift,
        };
    }

    /// Convenience: feed one exchange from its four timestamps.
    /// `t1` = client transmit, `t2` = server receive, `t3` = server
    /// transmit, `t4` = client receive.
    pub fn update_from_exchange(&mut self, t1: i64, t2: i64, t3: i64, t4: i64) {
        let offset = ((t2 - t1) + (t3 - t4)) as f64 / 2.0;
        let delay = ((t4 - t1) - (t3 - t2)) as f64 / 2.0;
        self.update(offset.round() as i64, delay.round() as i64, t4);
    }

    pub fn compute_server_time(&self, client_time: i64) -> i64 {
        let e = self.current;
        let drift = if e.use_drift { e.drift } else { 0.0 };
        let dt = (client_time - e.last_update) as f64;
        client_time + (e.offset + drift * dt).round() as i64
    }

    pub fn compute_client_time(&self, server_time: i64) -> i64 {
        let e = self.current;
        let drift = if e.use_drift { e.drift } else { 0.0 };
        ((server_time as f64 - e.offset + drift * e.last_update as f64) / (1.0 + drift)).round() as i64
    }

    pub fn is_synchronized(&self) -> bool {
        self.count >= 2 && self.offset_covariance.is_finite()
    }

    /// Offset standard deviation estimate, microseconds.
    pub fn error_us(&self) -> i64 {
        if self.offset_covariance.is_finite() {
            self.offset_covariance.sqrt().round() as i64
        } else {
            i64::MAX
        }
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    pub fn reset(&mut self) {
        *self = Self::new(
            self.process_variance.sqrt(),
            self.forget_variance_factor.sqrt(),
            self.drift_process_variance.sqrt(),
        );
    }

    /// The sync-interval schedule aiosendspin clients use: fast until
    /// converged, then back off as the error shrinks.
    pub fn next_interval_ms(&self) -> u64 {
        if !self.is_synchronized() {
            return 200;
        }
        match self.error_us() {
            e if e < 1_000 => 3_000,
            e if e < 2_000 => 1_000,
            e if e < 5_000 => 500,
            _ => 200,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulate a server clock = client + 1_000_000 µs offset with a fixed
    /// 2 ms symmetric network delay; the filter must converge on the offset.
    #[test]
    fn converges_on_constant_offset() {
        let mut f = TimeFilter::default();
        assert!(!f.is_synchronized());
        let offset = 1_000_000i64;
        let mut t = 10_000_000i64;
        for _ in 0..50 {
            let t1 = t;
            let t2 = t1 + 2_000 + offset;
            let t3 = t2 + 100;
            let t4 = t3 - offset + 2_000;
            f.update_from_exchange(t1, t2, t3, t4);
            t += 200_000;
        }
        assert!(f.is_synchronized());
        let now = t;
        let server = f.compute_server_time(now);
        assert!((server - (now + offset)).abs() < 50, "server={} want={}", server, now + offset);
        let back = f.compute_client_time(server);
        assert!((back - now).abs() <= 1);
    }

    /// With a drifting server clock (+50 ppm) the drift term must kick in
    /// and keep extrapolation accurate.
    #[test]
    fn tracks_drift() {
        let mut f = TimeFilter::default();
        let ppm = 50e-6;
        let mut t = 1_000_000i64;
        for _ in 0..400 {
            let server_at = |c: i64| c + 5_000 + (c as f64 * ppm) as i64;
            let t1 = t;
            let t2 = server_at(t1 + 1_000);
            let t3 = t2 + 50;
            let t4 = t1 + 2_050;
            f.update_from_exchange(t1, t2, t3, t4);
            t += 500_000;
        }
        let probe = t + 1_000_000;
        let want = probe + 5_000 + (probe as f64 * ppm) as i64;
        let got = f.compute_server_time(probe);
        assert!((got - want).abs() < 200, "got={} want={} diff={}", got, want, got - want);
    }

    #[test]
    fn non_monotonic_updates_are_ignored() {
        let mut f = TimeFilter::default();
        f.update(100, 10, 1_000);
        f.update(200, 10, 1_000);
        assert_eq!(f.count(), 1);
    }

    #[test]
    fn interval_schedule() {
        let mut f = TimeFilter::default();
        assert_eq!(f.next_interval_ms(), 200);
        f.update(0, 100, 1);
        f.update(0, 100, 2);
        assert!(f.is_synchronized());
        assert_eq!(f.next_interval_ms(), 3_000);
    }
}
