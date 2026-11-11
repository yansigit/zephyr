//! Signal channel adapter: a `signal-cli jsonRpc` child of the daemon's, on a number of zephyr's
//! own. See DESIGN.md section 4.3.

use serde::Deserialize;
use std::path::PathBuf;

/// `[signal]`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Zephyr's own number, never the owner's: `+821012345678`. Not a secret.
    pub account: String,
}

/// signal-cli's data dir: the account's keys, password and sessions. Root's, 0700, in `<state>`
/// and so out of the agent's reach (DESIGN §4.4); not in the daily backup.
pub fn data_dir(cfg: &crate::config::Config) -> PathBuf {
    cfg.state_dir().join("signal")
}
