//! Observe-mode egress profiling (Horizon 3, phase 0).
//!
//! Builds per-process profiles of *what each program talks to* by joining
//! three signals that otherwise live apart:
//!   - the process name + remote endpoint (from the connection table),
//!   - the destination hostname (TLS/QUIC SNI from the ClientHello, or the
//!     cleartext HTTP `Host`), and
//!   - the destination's autonomous-system org (from the geo/ASN database).
//!
//! This is the learned baseline the egress policy linter is authored from
//! ("observe → promote → warn on drift"). It is **observe only** — it
//! records what egress looks like, it never blocks. SNI is cleartext in the
//! ClientHello, so meaningful profiles form on the vast majority of traffic
//! with no decryption and no keylog.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::connections::Connection;
use super::geo::{is_private_ip, GeoCache};
use crate::dpi::AppProtocol;

/// Cap on distinct processes profiled. A fork-storm of short-lived
/// process names can't grow the map without bound.
pub(crate) const MAX_PROCESSES: usize = 256;
/// Cap on distinct destinations tracked per process.
const MAX_DESTS_PER_PROCESS: usize = 128;
/// Re-warn at most this often for the same (process, destination, port)
/// policy violation, so a continuously-violating flow doesn't alert-storm.
const VIOLATION_COOLDOWN_SECS: u64 = 300;

/// One observed destination for a process.
#[derive(Clone, Debug)]
pub struct EgressDest {
    /// Destination hostname — TLS/QUIC SNI, or cleartext HTTP `Host`. `None`
    /// when the flow carried no name we could read (e.g. raw-IP traffic).
    pub sni: Option<String>,
    /// Autonomous-system organization of the remote IP (e.g. `Google LLC`),
    /// when the ASN database resolved it.
    pub asn_org: Option<String>,
    pub port: u16,
    pub first_seen: Instant,
    pub last_seen: Instant,
    /// Number of observations (connection-refresh ticks this dest appeared).
    pub count: u64,
}

/// Identity of a destination within a process profile: the most specific
/// name available (SNI → ASN org → raw IP) paired with the port. Mirrors the
/// roadmap's rule granularity — prefer SNI, fall back to ASN, avoid raw IP.
type DestKey = (String, u16);

/// Per-process egress profile: the set of distinct destinations observed.
#[derive(Clone, Debug)]
pub struct EgressProfile {
    pub process: String,
    pub dests: HashMap<DestKey, EgressDest>,
    pub last_seen: Instant,
}

/// A flow that violated a declared egress rule. Surfaced as a warning —
/// the linter never blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    pub process: String,
    /// The destination identity that violated — SNI, ASN org, or IP.
    pub dest: String,
    pub port: u16,
    pub reason: String,
}

/// Accumulates per-process egress profiles across connection refreshes, and
/// (when a policy is loaded) flags flows that drift from the declared rules.
#[derive(Default)]
pub struct EgressProfiler {
    profiles: HashMap<String, EgressProfile>,
    /// Declared egress policy, if any. `None` ⇒ pure observe mode.
    policy: Option<EgressPolicy>,
    /// Cooldown per violating (process, dest, port) so a steady violation
    /// warns periodically, not every tick.
    violation_cooldown: HashMap<(String, String, u16), Instant>,
    /// Newly-detected violations awaiting drain by the caller.
    pending: Vec<Violation>,
}

impl EgressProfiler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct and load the policy from the default path, if one exists.
    pub fn with_default_policy() -> Self {
        let mut profiler = Self::new();
        if let Some(path) = default_policy_path() {
            profiler.set_policy(load_policy_file(&path));
        }
        profiler
    }

    /// Fold the current connection table into the profiles. Call once per
    /// connection refresh. Only external (non-private) destinations that have
    /// an owning process are recorded — egress to the public internet is the
    /// thing we baseline; LAN/loopback chatter isn't.
    pub fn observe(&mut self, connections: &[Connection], geo: &GeoCache) {
        let now = Instant::now();
        for conn in connections {
            let Some(process) = conn.process_name.as_deref() else {
                continue;
            };
            if process.is_empty() {
                continue;
            }
            let (Some(ip), Some(port_str)) = crate::app::parse_addr_parts(&conn.remote_addr) else {
                continue;
            };
            if is_private_ip(&ip) {
                continue;
            }
            let Ok(port) = port_str.parse::<u16>() else {
                continue;
            };

            let sni = dest_hostname(&conn.app_protocol);
            let asn_org = geo.lookup(&ip).map(|g| g.org).filter(|o| !o.is_empty());
            self.record(process, &ip, port, sni.clone(), asn_org.clone(), now);
            self.check_policy(process, &ip, port, &sni, &asn_org, now);
        }
        self.evict_processes_if_needed();
        // Drop expired cooldown entries so the map stays bounded.
        self.violation_cooldown.retain(|_, &mut t| {
            now.duration_since(t) < Duration::from_secs(VIOLATION_COOLDOWN_SECS)
        });
    }

    /// Compare one observed flow against the loaded policy. Only processes
    /// that *have* a declared rule are checked — an unlisted process has no
    /// rule to violate, so it never warns (deterministic, low-noise). A new
    /// violation is queued (subject to the per-flow cooldown).
    fn check_policy(
        &mut self,
        process: &str,
        ip: &str,
        port: u16,
        sni: &Option<String>,
        asn_org: &Option<String>,
        now: Instant,
    ) {
        let Some(policy) = &self.policy else {
            return;
        };
        let Some(rule) = policy.process.get(process) else {
            return;
        };
        let Some(reason) = rule.violation(sni.as_deref(), asn_org.as_deref(), port) else {
            return;
        };
        let dest = sni
            .clone()
            .or_else(|| asn_org.clone())
            .unwrap_or_else(|| ip.to_string());

        let key = (process.to_string(), dest.clone(), port);
        if let Some(&last) = self.violation_cooldown.get(&key) {
            if now.duration_since(last) < Duration::from_secs(VIOLATION_COOLDOWN_SECS) {
                return;
            }
        }
        self.violation_cooldown.insert(key, now);
        self.pending.push(Violation {
            process: process.to_string(),
            dest,
            port,
            reason,
        });
    }

    /// Install (or clear) the declared egress policy.
    pub fn set_policy(&mut self, policy: Option<EgressPolicy>) {
        self.policy = policy;
    }

    /// Whether a policy is currently loaded.
    pub fn has_policy(&self) -> bool {
        self.policy.is_some()
    }

    /// Drain the violations detected since the last call.
    pub fn take_violations(&mut self) -> Vec<Violation> {
        std::mem::take(&mut self.pending)
    }

    /// Ratify the current observed baseline into a declared policy — the
    /// "promote" step. The human reviews/edits the result before trusting
    /// it; that review is what defeats baseline poisoning (a profile learned
    /// on an already-compromised host).
    pub fn promote(&self) -> EgressPolicy {
        let mut policy = EgressPolicy::default();
        for profile in self.profiles.values() {
            let mut allow_sni = BTreeSet::new();
            let mut allow_asn = BTreeSet::new();
            let mut allow_ports = BTreeSet::new();
            for dest in profile.dests.values() {
                match (&dest.sni, &dest.asn_org) {
                    (Some(s), _) => {
                        allow_sni.insert(s.clone());
                    }
                    (None, Some(a)) => {
                        allow_asn.insert(a.clone());
                    }
                    (None, None) => {}
                }
                allow_ports.insert(dest.port);
            }
            policy.process.insert(
                profile.process.clone(),
                ProcessRule {
                    allow_sni: allow_sni.into_iter().collect(),
                    allow_asn: allow_asn.into_iter().collect(),
                    allow_ports: allow_ports.into_iter().collect(),
                },
            );
        }
        policy
    }

    /// Record one observed (process, destination) pair. Split out from
    /// `observe` so the join/key/eviction logic is testable without the geo
    /// resolver. The destination identity prefers the most specific name we
    /// have: SNI, then ASN org, then the raw IP.
    fn record(
        &mut self,
        process: &str,
        ip: &str,
        port: u16,
        sni: Option<String>,
        asn_org: Option<String>,
        now: Instant,
    ) {
        let label = sni
            .clone()
            .or_else(|| asn_org.clone())
            .unwrap_or_else(|| ip.to_string());
        self.upsert(process, (label, port), sni, asn_org, port, now);
    }

    fn upsert(
        &mut self,
        process: &str,
        key: DestKey,
        sni: Option<String>,
        asn_org: Option<String>,
        port: u16,
        now: Instant,
    ) {
        let profile = self
            .profiles
            .entry(process.to_string())
            .or_insert_with(|| EgressProfile {
                process: process.to_string(),
                dests: HashMap::new(),
                last_seen: now,
            });
        profile.last_seen = now;

        if let Some(dest) = profile.dests.get_mut(&key) {
            dest.last_seen = now;
            dest.count += 1;
            // Backfill a name/ASN that wasn't resolved on first sight (SNI
            // appears once the ClientHello is parsed; ASN once geo resolves).
            if dest.sni.is_none() {
                dest.sni = sni;
            }
            if dest.asn_org.is_none() {
                dest.asn_org = asn_org;
            }
            return;
        }

        if profile.dests.len() >= MAX_DESTS_PER_PROCESS {
            if let Some(oldest) = profile
                .dests
                .iter()
                .min_by_key(|(_, d)| d.last_seen)
                .map(|(k, _)| k.clone())
            {
                profile.dests.remove(&oldest);
            }
        }
        profile.dests.insert(
            key,
            EgressDest {
                sni,
                asn_org,
                port,
                first_seen: now,
                last_seen: now,
                count: 1,
            },
        );
    }

    fn evict_processes_if_needed(&mut self) {
        while self.profiles.len() > MAX_PROCESSES {
            if let Some(oldest) = self
                .profiles
                .iter()
                .min_by_key(|(_, p)| p.last_seen)
                .map(|(k, _)| k.clone())
            {
                self.profiles.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Number of processes with a profile (drives the debug overlay).
    pub fn process_count(&self) -> usize {
        self.profiles.len()
    }

    /// Total distinct destinations across all profiles.
    pub fn dest_count(&self) -> usize {
        self.profiles.values().map(|p| p.dests.len()).sum()
    }

    /// Snapshot of all profiles, sorted by process name. Each profile's
    /// destinations can be sorted by the caller; the map is returned as-is.
    pub fn snapshot(&self) -> Vec<EgressProfile> {
        let mut out: Vec<EgressProfile> = self.profiles.values().cloned().collect();
        out.sort_by(|a, b| a.process.cmp(&b.process));
        out
    }
}

// ── Declared egress policy ─────────────────────────────────────────────

/// A declarative egress allowlist: `process → {allowed SNIs, ASNs, ports}`.
/// The linter warns when a process with a rule talks to something outside
/// it — a sentence no firewall ruleset can express. It never blocks.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct EgressPolicy {
    #[serde(default)]
    pub process: HashMap<String, ProcessRule>,
}

/// The allowed egress for one process. An empty list means "unrestricted on
/// that dimension" — e.g. empty `allow_ports` permits any port.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProcessRule {
    /// Allowed destination hostnames. Exact (`api.example.com`) or a leading
    /// wildcard (`*.example.com`, which also matches the apex).
    #[serde(default)]
    pub allow_sni: Vec<String>,
    /// Allowed autonomous-system orgs (matched case-insensitively), used when
    /// a flow has no readable SNI.
    #[serde(default)]
    pub allow_asn: Vec<String>,
    /// Allowed destination ports. Empty ⇒ any port.
    #[serde(default)]
    pub allow_ports: Vec<u16>,
}

impl ProcessRule {
    /// Returns `Some(reason)` if the destination is outside this rule.
    fn violation(&self, sni: Option<&str>, asn_org: Option<&str>, port: u16) -> Option<String> {
        if !self.allow_ports.is_empty() && !self.allow_ports.contains(&port) {
            return Some(format!("port {port} not in allowlist"));
        }
        let name_restricted = !self.allow_sni.is_empty() || !self.allow_asn.is_empty();
        if name_restricted {
            let sni_ok = sni.is_some_and(|s| self.allow_sni.iter().any(|p| sni_matches(p, s)));
            let asn_ok =
                asn_org.is_some_and(|a| self.allow_asn.iter().any(|x| x.eq_ignore_ascii_case(a)));
            if !sni_ok && !asn_ok {
                let dest = sni.or(asn_org).unwrap_or("unknown destination");
                return Some(format!("{dest} not in allowlist"));
            }
        }
        None
    }
}

/// Match an `allow_sni` pattern against a host. Supports an exact match or a
/// leading `*.` wildcard (`*.example.com` matches `a.example.com` and the
/// apex `example.com`). Case-insensitive.
fn sni_matches(pattern: &str, host: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("*.") {
        host.eq_ignore_ascii_case(suffix)
            || host
                .to_ascii_lowercase()
                .ends_with(&format!(".{}", suffix.to_ascii_lowercase()))
    } else {
        host.eq_ignore_ascii_case(pattern)
    }
}

/// Default policy location: `<config_dir>/netwatch/egress-policy.toml`.
pub fn default_policy_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("netwatch").join("egress-policy.toml"))
}

/// Load a policy from disk. `None` if the file is absent or unparseable.
pub fn load_policy_file(path: &Path) -> Option<EgressPolicy> {
    let contents = std::fs::read_to_string(path).ok()?;
    match toml::from_str(&contents) {
        Ok(policy) => Some(policy),
        Err(e) => {
            tracing::warn!(target: "netwatch::egress", path = %path.display(), error = %e, "egress policy parse failed");
            None
        }
    }
}

/// Serialize a policy to disk (creating parent dirs). Used by "promote".
pub fn save_policy_file(policy: &EgressPolicy, path: &Path) -> std::io::Result<()> {
    let body = toml::to_string_pretty(policy)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let header = "# netwatch egress policy (observe → promote → warn).\n\
                  # Generated from the observed baseline; review before trusting.\n\
                  # The linter WARNS on drift — it never blocks.\n\n";
    std::fs::write(path, format!("{header}{body}"))
}

/// Extract the destination hostname from a flow's app-protocol: TLS/QUIC SNI
/// (cleartext ClientHello) or the cleartext HTTP `Host`.
fn dest_hostname(p: &Option<AppProtocol>) -> Option<String> {
    match p {
        Some(AppProtocol::Tls { sni: Some(s), .. }) => Some(s.clone()),
        Some(AppProtocol::Quic { sni: Some(s), .. }) => Some(s.clone()),
        Some(AppProtocol::Http { host: Some(h), .. }) => Some(h.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sni(host: &str) -> Option<String> {
        Some(host.to_string())
    }

    #[test]
    fn record_builds_per_process_profile_keyed_by_sni() {
        let mut p = EgressProfiler::new();
        let now = Instant::now();
        p.record(
            "chrome",
            "142.250.1.1",
            443,
            sni("www.google.com"),
            Some("Google LLC".into()),
            now,
        );
        p.record(
            "chrome",
            "142.250.1.2",
            443,
            sni("mail.google.com"),
            Some("Google LLC".into()),
            now,
        );
        // Same SNI again → same destination, count increments (not a new dest).
        p.record(
            "chrome",
            "142.250.9.9",
            443,
            sni("www.google.com"),
            Some("Google LLC".into()),
            now,
        );

        assert_eq!(p.process_count(), 1);
        let snap = p.snapshot();
        let chrome = &snap[0];
        assert_eq!(chrome.process, "chrome");
        assert_eq!(
            chrome.dests.len(),
            2,
            "two distinct SNIs → two destinations"
        );
        let www = chrome
            .dests
            .get(&("www.google.com".to_string(), 443))
            .unwrap();
        assert_eq!(www.count, 2);
        assert_eq!(www.asn_org.as_deref(), Some("Google LLC"));
    }

    #[test]
    fn falls_back_to_asn_then_ip_when_no_sni() {
        let mut p = EgressProfiler::new();
        let now = Instant::now();
        p.record("curl", "9.9.9.9", 443, None, Some("Quad9".into()), now);
        p.record("nc", "203.0.113.7", 4444, None, None, now);

        let snap = p.snapshot();
        // Sorted by process name: curl, nc.
        assert!(snap
            .iter()
            .any(|pr| pr.dests.contains_key(&("Quad9".to_string(), 443))));
        assert!(snap
            .iter()
            .any(|pr| pr.dests.contains_key(&("203.0.113.7".to_string(), 4444))));
    }

    #[test]
    fn backfills_sni_and_asn_discovered_later() {
        let mut p = EgressProfiler::new();
        let now = Instant::now();
        // First sight: no name resolved yet → keyed on IP.
        p.record("app", "198.51.100.5", 443, None, None, now);
        // The dest exists under the IP label; a later sighting that DOES
        // carry an SNI lands on a *new* key (the identity is now the SNI).
        p.record(
            "app",
            "198.51.100.5",
            443,
            sni("api.example.com"),
            Some("Example Org".into()),
            now,
        );

        let snap = p.snapshot();
        let app = &snap[0];
        // IP-keyed dest plus SNI-keyed dest.
        let by_ip = app.dests.get(&("198.51.100.5".to_string(), 443)).unwrap();
        assert!(by_ip.sni.is_none());
        let by_sni = app
            .dests
            .get(&("api.example.com".to_string(), 443))
            .unwrap();
        assert_eq!(by_sni.asn_org.as_deref(), Some("Example Org"));
    }

    #[test]
    fn observe_skips_private_and_processless_connections() {
        use crate::collectors::connections::Connection;
        let mk = |proc: Option<&str>, remote: &str| Connection {
            protocol: "TCP".into(),
            local_addr: "192.168.1.10:5000".into(),
            remote_addr: remote.into(),
            state: "ESTABLISHED".into(),
            pid: Some(1),
            process_name: proc.map(|s| s.to_string()),
            handshake_rtt_us: None,
            rx_rate: None,
            tx_rate: None,
            attribution: Default::default(),
            app_protocol: None,
            retransmits: 0,
            out_of_order: 0,
        };
        let mut p = EgressProfiler::new();
        let geo = crate::collectors::geo::GeoCache::new();
        let conns = vec![
            mk(Some("ssh"), "192.168.1.1:22"),  // private dst → skipped
            mk(None, "8.8.8.8:53"),             // no process → skipped
            mk(Some("ssh"), "203.0.113.50:22"), // recorded
        ];
        p.observe(&conns, &geo);
        assert_eq!(p.process_count(), 1);
        assert_eq!(p.dest_count(), 1);
    }

    #[test]
    fn per_process_destinations_are_capped() {
        let mut p = EgressProfiler::new();
        let now = Instant::now();
        for i in 0..(MAX_DESTS_PER_PROCESS + 50) {
            p.record(
                "noisy",
                &format!("203.0.113.{}", i % 256),
                1000 + i as u16,
                None,
                None,
                now,
            );
        }
        let snap = p.snapshot();
        assert!(snap[0].dests.len() <= MAX_DESTS_PER_PROCESS);
    }

    // ── Policy ──

    #[test]
    fn sni_wildcard_and_exact_matching() {
        assert!(sni_matches("api.example.com", "api.example.com"));
        assert!(sni_matches("API.example.com", "api.example.com")); // case-insensitive
        assert!(!sni_matches("api.example.com", "other.example.com"));
        assert!(sni_matches("*.example.com", "a.example.com"));
        assert!(sni_matches("*.example.com", "deep.sub.example.com"));
        assert!(sni_matches("*.example.com", "example.com")); // apex
        assert!(!sni_matches("*.example.com", "example.org"));
        assert!(!sni_matches("*.example.com", "notexample.com"));
    }

    #[test]
    fn rule_violation_semantics() {
        let rule = ProcessRule {
            allow_sni: vec!["*.google.com".into()],
            allow_asn: vec!["Cloudflare, Inc.".into()],
            allow_ports: vec![443],
        };
        // Allowed: matching SNI on an allowed port.
        assert!(rule.violation(Some("www.google.com"), None, 443).is_none());
        // Allowed via ASN fallback when no SNI.
        assert!(rule
            .violation(None, Some("Cloudflare, Inc."), 443)
            .is_none());
        // Wrong port.
        assert!(rule.violation(Some("www.google.com"), None, 8080).is_some());
        // Unlisted SNI.
        assert!(rule
            .violation(Some("evil.example.com"), None, 443)
            .is_some());
        // No name at all on a name-restricted rule → violation.
        assert!(rule.violation(None, None, 443).is_some());

        // A rule with no name restrictions only constrains ports.
        let port_only = ProcessRule {
            allow_ports: vec![443],
            ..Default::default()
        };
        assert!(port_only
            .violation(Some("anything.com"), None, 443)
            .is_none());
        assert!(port_only
            .violation(Some("anything.com"), None, 80)
            .is_some());
    }

    #[test]
    fn promote_then_policy_admits_the_observed_baseline() {
        let mut p = EgressProfiler::new();
        let now = Instant::now();
        p.record(
            "chrome",
            "142.250.1.1",
            443,
            sni("www.google.com"),
            Some("Google LLC".into()),
            now,
        );
        p.record("curl", "9.9.9.9", 443, None, Some("Quad9".into()), now);

        let policy = p.promote();
        let chrome = policy.process.get("chrome").unwrap();
        assert!(chrome.allow_sni.contains(&"www.google.com".to_string()));
        assert_eq!(chrome.allow_ports, vec![443]);
        let curl = policy.process.get("curl").unwrap();
        assert!(curl.allow_asn.contains(&"Quad9".to_string()));

        // The promoted policy must not flag the very baseline it came from.
        assert!(chrome
            .violation(Some("www.google.com"), Some("Google LLC"), 443)
            .is_none());
    }

    #[test]
    fn policy_toml_roundtrips() {
        let mut policy = EgressPolicy::default();
        policy.process.insert(
            "chrome".into(),
            ProcessRule {
                allow_sni: vec!["*.google.com".into()],
                allow_asn: vec![],
                allow_ports: vec![443],
            },
        );
        let s = toml::to_string_pretty(&policy).unwrap();
        let parsed: EgressPolicy = toml::from_str(&s).unwrap();
        let rule = parsed.process.get("chrome").unwrap();
        assert_eq!(rule.allow_sni, vec!["*.google.com".to_string()]);
        assert_eq!(rule.allow_ports, vec![443]);
    }

    #[test]
    fn observe_warns_on_drift_with_cooldown() {
        let mut p = EgressProfiler::new();
        let mut policy = EgressPolicy::default();
        policy.process.insert(
            "app".into(),
            ProcessRule {
                allow_sni: vec!["api.example.com".into()],
                allow_asn: vec![],
                allow_ports: vec![443],
            },
        );
        p.set_policy(Some(policy));
        let now = Instant::now();

        // Drift: app talks to an undeclared host.
        p.check_policy(
            "app",
            "203.0.113.9",
            443,
            &sni("evil.example.com"),
            &None,
            now,
        );
        // An unlisted process is never flagged (no rule to violate).
        p.check_policy(
            "other",
            "203.0.113.9",
            443,
            &sni("whatever.com"),
            &None,
            now,
        );

        let v = p.take_violations();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].process, "app");
        assert!(v[0].reason.contains("not in allowlist"));

        // Same violation again immediately → suppressed by cooldown.
        p.check_policy(
            "app",
            "203.0.113.9",
            443,
            &sni("evil.example.com"),
            &None,
            now,
        );
        assert_eq!(p.take_violations().len(), 0);
    }
}
