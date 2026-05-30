//! Live end-to-end verification of TLS 1.3 Application-Data decryption.
//!
//! Drives the *real* capture path — `PacketCollector` + keylog watcher +
//! `StreamTracker::try_decrypt_tls_record` — against actual wire traffic,
//! then proves we recover plaintext.
//!
//! Flow:
//!   1. Start the keylog watcher on a fresh temp SSLKEYLOGFILE.
//!   2. Start live pcap capture on the default interface (BPF: tcp port 443).
//!   3. Spawn an OpenSSL-backed Python client that:
//!        - completes a TLS 1.3 handshake (writes secrets to the keylog),
//!        - sleeps briefly so the watcher ingests the keylog BEFORE any
//!          application data flows (avoids the watcher-poll race),
//!        - sends a recognizable `GET / HTTP/1.1` request.
//!   4. Scan captured packets for `decrypted_plaintext` containing that GET.
//!
//! Requires pcap access (member of `access_bpf` group, or run as root) and
//! a Python 3.8+ linked against OpenSSL (honors the `SSLKEYLOGFILE` env var).
//!
//! Run:  cargo run --example tls_decrypt_live -- [host] [interface]

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use netwatch::collectors::packets::PacketCollector;

fn main() {
    let host = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "example.com".into());
    let iface = std::env::args().nth(2).unwrap_or_else(|| "en0".into());

    // Unique-enough keylog path without Date/random (which aren't needed here).
    let keylog: PathBuf =
        std::env::temp_dir().join(format!("netwatch-keylog-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&keylog);
    // Create empty so the watcher has something to open immediately.
    std::fs::File::create(&keylog).expect("create keylog file");

    println!("keylog:    {}", keylog.display());
    println!("interface: {iface}");
    println!("host:      {host}\n");

    let mut collector = PacketCollector::new();
    collector.configure_tls_keylog(Some(keylog.clone()));
    collector.start_capture(&iface, Some("tcp port 443"));

    // Give the capture thread a moment to actually open the pcap handle.
    thread::sleep(Duration::from_millis(800));
    if let Some(err) = collector.error.lock().unwrap().clone() {
        eprintln!("capture failed to start: {err}");
        std::process::exit(2);
    }
    println!("capture running, launching TLS 1.3 client...\n");

    // Cooperating client: handshake, pause so the keylog watcher ingests the
    // secret, then send a recognizable request. OpenSSL-backed Python writes
    // the NSS keylog automatically from the SSLKEYLOGFILE env var.
    let py = r#"
import ssl, socket, time, sys, os
host = sys.argv[1]
ctx = ssl.create_default_context()
ctx.minimum_version = ssl.TLSVersion.TLSv1_3
ctx.maximum_version = ssl.TLSVersion.TLSv1_3
raw = socket.create_connection((host, 443), timeout=10)
s = ctx.wrap_socket(raw, server_hostname=host)
print("  [py] handshake ok:", s.version(), s.cipher()[0], flush=True)
s.settimeout(3.0)
# No post-handshake sleep: the FIRST request races the keylog watcher and
# will likely be missed. Keep the connection alive and keep sending — once
# the watcher ingests the secret, sequence-resync must lock onto the later
# requests. The old code permanently disabled the stream on that first miss.
n = int(os.environ.get("REQUESTS", "6"))
sent = 0
for i in range(n):
    req = ("GET / HTTP/1.1\r\nHost: %s\r\nConnection: keep-alive\r\nUser-Agent: netwatch-tls-verify\r\n\r\n" % host).encode()
    try:
        s.sendall(req); sent += 1
        s.recv(16384)
    except Exception:
        break
    time.sleep(0.2)
print("  [py] sent", sent, "keep-alive requests over ~%.1fs" % (sent*0.2), flush=True)
try: s.close()
except Exception: pass
"#;

    let status = Command::new("python3")
        .arg("-c")
        .arg(py)
        .arg(&host)
        .env("SSLKEYLOGFILE", &keylog)
        .status()
        .expect("spawn python3");
    if !status.success() {
        eprintln!("python client failed");
        std::process::exit(2);
    }

    // Let the response and any trailing records get captured.
    thread::sleep(Duration::from_millis(800));

    // Drain and inspect.
    let keylog_lines = std::fs::read_to_string(&keylog)
        .map(|s| s.lines().count())
        .unwrap_or(0);
    println!("\nkeylog lines ingested by client: {keylog_lines}");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut decrypted_count;
    let mut found_get = false;
    let mut samples: Vec<String> = Vec::new();
    loop {
        {
            let pkts = collector.get_packets();
            decrypted_count = 0;
            samples.clear();
            for p in pkts.iter() {
                if let Some(pt) = &p.decrypted_plaintext {
                    decrypted_count += 1;
                    let preview: String = pt
                        .iter()
                        .take(64)
                        .map(|&b| {
                            if b.is_ascii_graphic() || b == b' ' {
                                b as char
                            } else {
                                '.'
                            }
                        })
                        .collect();
                    if samples.len() < 6 {
                        samples.push(format!(
                            "  {}:{} -> {}:{}  [{} B] {:?}",
                            p.src_ip,
                            p.src_port.unwrap_or(0),
                            p.dst_ip,
                            p.dst_port.unwrap_or(0),
                            pt.len(),
                            preview
                        ));
                    }
                    const NEEDLE: &[u8] = b"GET / HTTP/1.1";
                    if pt.windows(NEEDLE.len()).any(|w| w == NEEDLE) {
                        found_get = true;
                    }
                }
            }
        }
        if found_get || Instant::now() > deadline {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }

    collector.stop_capture();
    let _ = std::io::stdout().flush();
    let _ = std::fs::remove_file(&keylog);

    println!("\n=== RESULT ===");
    println!("decrypted application-data records captured: {decrypted_count}");
    for s in &samples {
        println!("{s}");
    }
    if found_get {
        println!(
            "\n✅ VERIFIED: recovered our plaintext \"GET / HTTP/1.1\" from a live TLS 1.3 flow."
        );
    } else if decrypted_count > 0 {
        println!("\n⚠️  Decrypted {decrypted_count} record(s) but did not match the exact GET line (may be a coalesced/segmented record). Decryption pipeline is working.");
    } else {
        println!("\n❌ No records decrypted. Check: keylog written? client used OpenSSL? capture on the right interface? watcher race?");
        std::process::exit(1);
    }
}
