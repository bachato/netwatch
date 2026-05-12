use serde::Serialize;
use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use super::connections::Connection;
use super::traffic::InterfaceTraffic;

#[derive(Debug, Clone, Serialize)]
pub struct ProcessBandwidth {
    pub process_name: String,
    pub pid: Option<u32>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_rate: f64,
    pub tx_rate: f64,
    pub connection_count: u32,
    /// Min RTT (ms) across this process's TCP connections — derived from
    /// `Connection.kernel_rtt_us`. None when no kernel RTT data is available.
    pub rtt_ms: Option<f64>,
    /// CPU%, populated from a background `ps` poll. None until the first
    /// poll completes (or when the platform doesn't support the sampler).
    pub cpu_percent: Option<f64>,
}

pub struct ProcessBandwidthCollector {
    ranked: Vec<ProcessBandwidth>,
    /// Baseline interface byte totals captured on the first `update()` call,
    /// so we attribute only bytes that flowed since netwatch started — not the
    /// kernel's since-interface-up counter (which can be GBs at startup).
    baseline_rx_bytes: Option<u64>,
    baseline_tx_bytes: Option<u64>,
    /// CPU% per-pid cache, populated by a background `ps` thread on a slow
    /// tick. The mutex is short-lived; reads are O(1) lookups.
    cpu_cache: Arc<Mutex<HashMap<u32, f64>>>,
    cpu_busy: Arc<AtomicBool>,
}

impl Default for ProcessBandwidthCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessBandwidthCollector {
    pub fn new() -> Self {
        Self {
            ranked: Vec::new(),
            baseline_rx_bytes: None,
            baseline_tx_bytes: None,
            cpu_cache: Arc::new(Mutex::new(HashMap::new())),
            cpu_busy: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn update(&mut self, connections: &[Connection], interfaces: &[InterfaceTraffic]) {
        // Interface bytes-since-startup form the denominator for the
        // per-process byte allocation; per-process *rates* come from the
        // connections themselves (populated by the packet-capture rate
        // tracker), so interface rates are no longer used here.
        let raw_rx_bytes: u64 = interfaces.iter().map(|i| i.rx_bytes_total).sum();
        let raw_tx_bytes: u64 = interfaces.iter().map(|i| i.tx_bytes_total).sum();

        let baseline_rx = *self.baseline_rx_bytes.get_or_insert(raw_rx_bytes);
        let baseline_tx = *self.baseline_tx_bytes.get_or_insert(raw_tx_bytes);
        let total_rx_bytes = raw_rx_bytes.saturating_sub(baseline_rx);
        let total_tx_bytes = raw_tx_bytes.saturating_sub(baseline_tx);

        // Aggregate per-(process, pid) state from the ESTABLISHED connections.
        // Rates come from the packet capture path (conn.rx_rate / tx_rate are
        // populated by RateState in connections.rs when a stream's bytes are
        // moving). RTT is the min across the process's TCP conns. We previously
        // allocated bytes by `count / total_count` which gave every process
        // with the same connection count an identical RX/TX — a fictional
        // model that misled users. Now bytes are allocated by *rate share*
        // instead, so the panel only shows differential numbers when there's
        // real per-connection rate data behind them.
        let mut process_conns: HashMap<(String, Option<u32>), u32> = HashMap::new();
        let mut process_rx_rate: HashMap<(String, Option<u32>), f64> = HashMap::new();
        let mut process_tx_rate: HashMap<(String, Option<u32>), f64> = HashMap::new();
        let mut process_rtt: HashMap<(String, Option<u32>), f64> = HashMap::new();
        let mut total_established: u32 = 0;

        for conn in connections {
            if conn.state != "ESTABLISHED" {
                continue;
            }
            let name = conn
                .process_name
                .clone()
                .unwrap_or_else(|| format!("pid:{}", conn.pid.map_or(0, |p| p)));
            let key = (name, conn.pid);
            *process_conns.entry(key.clone()).or_insert(0) += 1;
            total_established += 1;
            if let Some(rx) = conn.rx_rate {
                *process_rx_rate.entry(key.clone()).or_insert(0.0) += rx;
            }
            if let Some(tx) = conn.tx_rate {
                *process_tx_rate.entry(key.clone()).or_insert(0.0) += tx;
            }
            if let Some(rtt_us) = conn.kernel_rtt_us {
                let rtt_ms = rtt_us / 1000.0;
                process_rtt
                    .entry(key)
                    .and_modify(|v| {
                        if rtt_ms < *v {
                            *v = rtt_ms;
                        }
                    })
                    .or_insert(rtt_ms);
            }
        }

        if total_established == 0 {
            self.ranked.clear();
            return;
        }

        let total_proc_rx_rate: f64 = process_rx_rate.values().sum();
        let total_proc_tx_rate: f64 = process_tx_rate.values().sum();

        let cpu_cache = self.cpu_cache.lock().unwrap().clone();

        let mut ranked: Vec<ProcessBandwidth> = process_conns
            .into_iter()
            .map(|((process_name, pid), count)| {
                let key = (process_name.clone(), pid);
                let rx_rate = process_rx_rate.get(&key).copied().unwrap_or(0.0);
                let tx_rate = process_tx_rate.get(&key).copied().unwrap_or(0.0);
                // Allocate cumulative interface bytes by this process's
                // rate share. When no rates exist (no packet capture path),
                // every process gets 0 — honest "we don't know" rather
                // than fake equal slices of the interface total.
                let rx_bytes = if total_proc_rx_rate > 0.0 {
                    (total_rx_bytes as f64 * (rx_rate / total_proc_rx_rate)) as u64
                } else {
                    0
                };
                let tx_bytes = if total_proc_tx_rate > 0.0 {
                    (total_tx_bytes as f64 * (tx_rate / total_proc_tx_rate)) as u64
                } else {
                    0
                };
                let rtt_ms = process_rtt.get(&key).copied();
                let cpu_percent = pid.and_then(|p| cpu_cache.get(&p).copied());
                ProcessBandwidth {
                    process_name,
                    pid,
                    rx_bytes,
                    tx_bytes,
                    rx_rate,
                    tx_rate,
                    connection_count: count,
                    rtt_ms,
                    cpu_percent,
                }
            })
            .collect();

        ranked.sort_by(|a, b| {
            let bw_b = b.rx_rate + b.tx_rate;
            let bw_a = a.rx_rate + a.tx_rate;
            bw_b.partial_cmp(&bw_a).unwrap_or(std::cmp::Ordering::Equal)
        });

        self.ranked = ranked;
    }

    pub fn ranked(&self) -> &[ProcessBandwidth] {
        &self.ranked
    }

    /// Spawn a background `ps` poll to refresh the CPU% cache. Coalesces — if
    /// a previous poll is still running, this is a no-op. Call from the app
    /// loop on a slow tick (~5s).
    pub fn refresh_cpu(&self) {
        if self.cpu_busy.load(Ordering::SeqCst) {
            return;
        }
        self.cpu_busy.store(true, Ordering::SeqCst);
        let cache = Arc::clone(&self.cpu_cache);
        let busy = Arc::clone(&self.cpu_busy);
        thread::spawn(move || {
            if let Some(pid_cpu) = sample_cpu() {
                *cache.lock().unwrap() = pid_cpu;
            }
            busy.store(false, Ordering::SeqCst);
        });
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn sample_cpu() -> Option<HashMap<u32, f64>> {
    let output = Command::new("ps")
        .args(["-A", "-o", "pid=,pcpu="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut map: HashMap<u32, f64> = HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let pid: Option<u32> = parts.next().and_then(|s| s.parse().ok());
        let cpu: Option<f64> = parts.next().and_then(|s| s.parse().ok());
        if let (Some(pid), Some(cpu)) = (pid, cpu) {
            map.insert(pid, cpu);
        }
    }
    Some(map)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sample_cpu() -> Option<HashMap<u32, f64>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn make_conn(name: &str, pid: u32, state: &str) -> Connection {
        make_conn_rated(name, pid, state, None, None)
    }

    fn make_conn_rated(
        name: &str,
        pid: u32,
        state: &str,
        rx: Option<f64>,
        tx: Option<f64>,
    ) -> Connection {
        Connection {
            protocol: "TCP".into(),
            local_addr: "127.0.0.1:8080".into(),
            remote_addr: "10.0.0.1:443".into(),
            state: state.into(),
            pid: Some(pid),
            process_name: Some(name.into()),
            kernel_rtt_us: None,
            rx_rate: rx,
            tx_rate: tx,
            attribution: Default::default(),
        }
    }

    fn make_interface(rx_rate: f64, tx_rate: f64) -> InterfaceTraffic {
        InterfaceTraffic {
            name: "en0".into(),
            rx_rate,
            tx_rate,
            rx_bytes_total: 1_000_000,
            tx_bytes_total: 500_000,
            rx_packets: 0,
            tx_packets: 0,
            rx_errors: 0,
            tx_errors: 0,
            rx_drops: 0,
            tx_drops: 0,
            rx_history: VecDeque::new(),
            tx_history: VecDeque::new(),
        }
    }

    #[test]
    fn empty_connections_produces_empty_ranking() {
        let mut collector = ProcessBandwidthCollector::new();
        collector.update(&[], &[make_interface(1000.0, 500.0)]);
        assert!(collector.ranked().is_empty());
    }

    #[test]
    fn non_established_connections_are_ignored() {
        let mut collector = ProcessBandwidthCollector::new();
        let conns = vec![make_conn("firefox", 100, "TIME_WAIT")];
        collector.update(&conns, &[make_interface(1000.0, 500.0)]);
        assert!(collector.ranked().is_empty());
    }

    #[test]
    fn single_process_gets_its_own_rate() {
        let mut collector = ProcessBandwidthCollector::new();
        let conns = vec![make_conn_rated(
            "firefox",
            100,
            "ESTABLISHED",
            Some(1000.0),
            Some(500.0),
        )];
        collector.update(&conns, &[make_interface(1000.0, 500.0)]);
        assert_eq!(collector.ranked().len(), 1);
        let p = &collector.ranked()[0];
        assert_eq!(p.process_name, "firefox");
        assert!((p.rx_rate - 1000.0).abs() < 0.01);
        assert!((p.tx_rate - 500.0).abs() < 0.01);
        assert_eq!(p.connection_count, 1);
    }

    #[test]
    fn rates_aggregated_per_process_from_per_connection_rates() {
        // Three firefox connections (300/200/100 rx) + one curl (250 rx).
        // Real aggregation should give firefox 600, curl 250 — not equal
        // shares of the interface total like the old count-fraction model.
        let mut collector = ProcessBandwidthCollector::new();
        let conns = vec![
            make_conn_rated("firefox", 100, "ESTABLISHED", Some(300.0), Some(100.0)),
            make_conn_rated("firefox", 100, "ESTABLISHED", Some(200.0), Some(50.0)),
            make_conn_rated("firefox", 100, "ESTABLISHED", Some(100.0), Some(50.0)),
            make_conn_rated("curl", 200, "ESTABLISHED", Some(250.0), Some(0.0)),
        ];
        collector.update(&conns, &[make_interface(9999.0, 9999.0)]);
        let firefox = collector
            .ranked()
            .iter()
            .find(|p| p.process_name == "firefox")
            .unwrap();
        let curl = collector
            .ranked()
            .iter()
            .find(|p| p.process_name == "curl")
            .unwrap();
        assert_eq!(firefox.connection_count, 3);
        assert_eq!(curl.connection_count, 1);
        assert!((firefox.rx_rate - 600.0).abs() < 0.01);
        assert!((firefox.tx_rate - 200.0).abs() < 0.01);
        assert!((curl.rx_rate - 250.0).abs() < 0.01);
    }

    #[test]
    fn processes_without_rates_show_zero_not_fake_equal_share() {
        // Regression for the "all processes show identical RX" bug. When
        // no per-connection rates are available (no packet capture path,
        // Linux non-sudo, etc.) each process should report 0 rather than
        // an equal slice of total interface bandwidth.
        let mut collector = ProcessBandwidthCollector::new();
        let conns = vec![
            make_conn("firefox", 100, "ESTABLISHED"),
            make_conn("curl", 200, "ESTABLISHED"),
            make_conn("sshd", 300, "ESTABLISHED"),
        ];
        collector.update(&conns, &[make_interface(1000.0, 500.0)]);
        for p in collector.ranked() {
            assert_eq!(
                p.rx_rate, 0.0,
                "{} should have 0 rate when no per-connection data exists",
                p.process_name
            );
            assert_eq!(p.tx_rate, 0.0, "{} should have 0 tx rate", p.process_name);
            assert_eq!(p.rx_bytes, 0, "{} should have 0 bytes", p.process_name);
        }
    }

    #[test]
    fn ranked_sorted_by_total_bandwidth_descending() {
        let mut collector = ProcessBandwidthCollector::new();
        let conns = vec![
            make_conn_rated("curl", 200, "ESTABLISHED", Some(100.0), Some(50.0)),
            make_conn_rated("firefox", 100, "ESTABLISHED", Some(500.0), Some(200.0)),
            make_conn_rated("firefox", 100, "ESTABLISHED", Some(300.0), Some(100.0)),
        ];
        collector.update(&conns, &[make_interface(9999.0, 9999.0)]);
        assert_eq!(collector.ranked()[0].process_name, "firefox");
        assert_eq!(collector.ranked()[1].process_name, "curl");
    }
}
