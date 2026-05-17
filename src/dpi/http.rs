//! HTTP/1.x classifier — extracts method + Host header from a request line.

use super::{AppProtocol, Classifier};

pub struct HttpClassifier;

impl Classifier for HttpClassifier {
    fn classify(&self, _payload: &[u8], _is_tcp: bool) -> Option<AppProtocol> {
        // Phase 7 — placeholder.
        None
    }
}
