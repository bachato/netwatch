//! Deep packet inspection — application-layer protocol classification.
//!
//! Each classifier looks at the first N bytes of an application-layer
//! payload (post-TCP/UDP) and decides whether the flow is its protocol.
//! Classifiers run once per stream — the result is cached on the
//! `Stream` record in `collectors::packets` for the flow's lifetime.
//!
//! Adding a new classifier:
//! 1. Drop a new file under `src/dpi/` with a struct implementing
//!    `Classifier::classify`.
//! 2. Add it to `classify_once` in priority order — cheap pattern-match
//!    classifiers first, parser-based ones later.

pub mod dns;
pub mod http;
pub mod quic;
pub mod ssh;
pub mod tls;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppProtocol {
    /// TLS handshake observed; SNI / ALPN extracted when present.
    Tls {
        sni: Option<String>,
        alpn: Option<String>,
    },
    /// HTTP/1.x request line + `Host:` header.
    Http {
        method: String,
        host: Option<String>,
    },
    /// DNS query — first question's qname + qtype.
    Dns { qname: String, qtype: u16 },
    /// SSH server / client banner line, e.g. `SSH-2.0-OpenSSH_9.0`.
    Ssh { version: String },
    /// QUIC Initial packet detected. SNI extraction is deferred (header
    /// protection makes it non-trivial); `None` for now.
    Quic { sni: Option<String> },
}

pub trait Classifier {
    /// `Some(protocol)` when this classifier recognizes the payload,
    /// `None` to fall through to the next classifier.
    fn classify(&self, payload: &[u8], is_tcp: bool) -> Option<AppProtocol>;
}

/// Run all classifiers in priority order, returning the first match.
/// Cheap pattern-match classifiers go first; parser-based ones last.
pub fn classify_once(payload: &[u8], is_tcp: bool) -> Option<AppProtocol> {
    if payload.len() < 16 {
        return None;
    }
    // Cheapest first: starts-with byte patterns.
    if let Some(p) = ssh::SshClassifier.classify(payload, is_tcp) {
        return Some(p);
    }
    // HTTP: ASCII method prefix + httparse for confirmation.
    if let Some(p) = http::HttpClassifier.classify(payload, is_tcp) {
        return Some(p);
    }
    // TLS: 0x16 0x03 prefix gate then full handshake parse.
    if let Some(p) = tls::TlsClassifier.classify(payload, is_tcp) {
        return Some(p);
    }
    // UDP-only classifiers: QUIC first (cheap byte check), then DNS.
    if !is_tcp {
        if let Some(p) = quic::QuicClassifier.classify(payload, is_tcp) {
            return Some(p);
        }
        if let Some(p) = dns::DnsClassifier.classify(payload, is_tcp) {
            return Some(p);
        }
    }
    None
}
