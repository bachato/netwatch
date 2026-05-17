//! SSH classifier — server / client banner line.

use super::{AppProtocol, Classifier};

pub struct SshClassifier;

impl Classifier for SshClassifier {
    fn classify(&self, _payload: &[u8], _is_tcp: bool) -> Option<AppProtocol> {
        // Phase 8 — placeholder.
        None
    }
}
