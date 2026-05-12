//! macOS user credential authentication via opendirectoryd.
//!
//! Uses `/usr/bin/dscl` to verify username/password against the
//! local macOS directory service — the same backend used by the
//! login window, SSH, and other system services.

use std::process::Command;
use tracing::debug;

/// Authenticate a macOS user against the local directory node.
///
/// Returns `true` when the password matches the macOS user account.
/// Returns `false` on invalid credentials or infrastructure errors.
pub fn verify_user_password(username: &str, password: &str) -> bool {
    let output = Command::new("/usr/bin/dscl")
        .arg("/Local/Default")
        .arg("-authonly")
        .arg(username)
        .arg(password)
        .output();

    match output {
        Ok(out) => {
            let ok = out.status.success();
            if ok {
                debug!(?username, "Authentication succeeded");
            } else {
                debug!(?username, "Authentication failed");
            }
            ok
        }
        Err(e) => {
            debug!(?username, error = %e, "dscl invocation failed");
            false
        }
    }
}
