//! Counters for the Prometheus endpoint, shared by the game thread and the
//! I/O tasks. Everything is a relaxed atomic: these are statistics.

use std::sync::atomic::{AtomicU64, Ordering};

/// Upper bounds (µs) of the game-loop tick duration histogram.
pub const TICK_BUCKETS_US: [u64; 9] = [50, 100, 250, 500, 1000, 2500, 5000, 10_000, 50_000];

/// Server counters.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Game-loop ticks run.
    pub ticks: AtomicU64,
    /// Time the game thread spent working (ticks, packet handling, flushes).
    pub busy_ns: AtomicU64,
    /// Tick durations: one count per bucket of [`TICK_BUCKETS_US`], plus +Inf.
    pub tick_hist: [AtomicU64; TICK_BUCKETS_US.len() + 1],
    /// Sum of tick durations.
    pub tick_ns: AtomicU64,
    /// Longest tick so far.
    pub tick_ns_max: AtomicU64,
    /// Datagrams received on the game port.
    pub datagrams_in: AtomicU64,
    /// Bytes received on the game port.
    pub bytes_in: AtomicU64,
    /// Datagrams sent from the game port.
    pub datagrams_out: AtomicU64,
    /// Bytes sent from the game port.
    pub bytes_out: AtomicU64,
    /// Datagrams dropped because the inbound queue was full.
    pub inbound_dropped: AtomicU64,
}

impl Metrics {
    /// Records one tick that took `ns`.
    pub fn tick(&self, ns: u64) {
        self.ticks.fetch_add(1, Ordering::Relaxed);
        self.tick_ns.fetch_add(ns, Ordering::Relaxed);
        self.busy_ns.fetch_add(ns, Ordering::Relaxed);
        self.tick_ns_max.fetch_max(ns, Ordering::Relaxed);
        let us = ns / 1000;
        let i = TICK_BUCKETS_US
            .iter()
            .position(|&b| us <= b)
            .unwrap_or(TICK_BUCKETS_US.len());
        self.tick_hist[i].fetch_add(1, Ordering::Relaxed);
    }

    /// Records work outside ticks (handling inbound packets, flushing).
    pub fn busy(&self, ns: u64) {
        self.busy_ns.fetch_add(ns, Ordering::Relaxed);
    }

    /// Records a received datagram.
    pub fn received(&self, bytes: usize) {
        self.datagrams_in.fetch_add(1, Ordering::Relaxed);
        self.bytes_in.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Records a sent datagram.
    pub fn sent(&self, bytes: usize) {
        self.datagrams_out.fetch_add(1, Ordering::Relaxed);
        self.bytes_out.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// Prometheus text for the counters (gauges are added by the caller).
    pub fn render(&self, out: &mut String) {
        use std::fmt::Write;
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let mut counter = |name: &str, help: &str, v: f64| {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}"
            );
        };
        counter(
            "oss_ticks_total",
            "Game loop ticks run.",
            g(&self.ticks) as f64,
        );
        counter(
            "oss_game_busy_seconds_total",
            "Time the game thread spent working.",
            g(&self.busy_ns) as f64 / 1e9,
        );
        counter(
            "oss_datagrams_received_total",
            "Datagrams received on the game port.",
            g(&self.datagrams_in) as f64,
        );
        counter(
            "oss_received_bytes_total",
            "Bytes received on the game port.",
            g(&self.bytes_in) as f64,
        );
        counter(
            "oss_datagrams_sent_total",
            "Datagrams sent from the game port.",
            g(&self.datagrams_out) as f64,
        );
        counter(
            "oss_sent_bytes_total",
            "Bytes sent from the game port.",
            g(&self.bytes_out) as f64,
        );
        counter(
            "oss_inbound_dropped_total",
            "Datagrams dropped because the game thread fell behind.",
            g(&self.inbound_dropped) as f64,
        );
        let _ = writeln!(
            out,
            "# HELP oss_tick_duration_seconds Game loop tick duration.\n# TYPE oss_tick_duration_seconds histogram"
        );
        let mut cum = 0;
        for (i, b) in TICK_BUCKETS_US.iter().enumerate() {
            cum += g(&self.tick_hist[i]);
            let _ = writeln!(
                out,
                "oss_tick_duration_seconds_bucket{{le=\"{}\"}} {cum}",
                *b as f64 / 1e6
            );
        }
        cum += g(&self.tick_hist[TICK_BUCKETS_US.len()]);
        let _ = writeln!(out, "oss_tick_duration_seconds_bucket{{le=\"+Inf\"}} {cum}");
        let _ = writeln!(
            out,
            "oss_tick_duration_seconds_sum {}\noss_tick_duration_seconds_count {cum}",
            g(&self.tick_ns) as f64 / 1e9
        );
        let _ = writeln!(
            out,
            "# HELP oss_tick_duration_max_seconds Longest tick since start.\n# TYPE oss_tick_duration_max_seconds gauge\noss_tick_duration_max_seconds {}",
            g(&self.tick_ns_max) as f64 / 1e9
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram() {
        let m = Metrics::default();
        m.tick(40_000); // 40 µs
        m.tick(3_000_000); // 3 ms
        m.tick(90_000_000); // 90 ms
        let mut s = String::new();
        m.render(&mut s);
        assert!(s.contains("oss_tick_duration_seconds_bucket{le=\"0.00005\"} 1"));
        assert!(s.contains("oss_tick_duration_seconds_bucket{le=\"0.005\"} 2"));
        assert!(s.contains("oss_tick_duration_seconds_bucket{le=\"+Inf\"} 3"));
        assert!(s.contains("oss_ticks_total 3"));
    }
}
