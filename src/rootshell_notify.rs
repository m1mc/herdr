//! Rootshell APNs push notification integration.
//!
//! Delivers APNs push notifications via `rootshell-notify` when the input-originating
//! client has gone offline or when no client is attached.

use std::path::PathBuf;
use std::process::Stdio;
use tracing::{debug, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PushStatus {
    Done,
    Blocked,
    Info,
}

impl PushStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Info => "info",
        }
    }
}

impl From<crate::sound::Sound> for PushStatus {
    fn from(sound: crate::sound::Sound) -> Self {
        match sound {
            crate::sound::Sound::Done => Self::Done,
            crate::sound::Sound::Request => Self::Blocked,
        }
    }
}

impl From<crate::app::state::ToastKind> for PushStatus {
    fn from(kind: crate::app::state::ToastKind) -> Self {
        match kind {
            crate::app::state::ToastKind::Finished => Self::Done,
            crate::app::state::ToastKind::NeedsAttention => Self::Blocked,
            crate::app::state::ToastKind::UpdateInstalled => Self::Info,
        }
    }
}

/// Finds the `rootshell-notify` binary if available and configured.
pub(crate) fn find_rootshell_notify() -> Option<PathBuf> {
    let home = crate::integration::home_dir().ok();

    // 1. Check if ~/.config/rootshell-push/config.json (or ROOTSHELL_PUSH_CONFIG) exists.
    let config_path = std::env::var_os("ROOTSHELL_PUSH_CONFIG")
        .map(PathBuf::from)
        .or_else(|| {
            home.as_ref()
                .map(|h| h.join(".config").join("rootshell-push").join("config.json"))
        });

    if !config_path.is_some_and(|config| config.exists()) {
        return None;
    }

    // 2. Search PATH
    let binary_name = if cfg!(windows) {
        "rootshell-notify.exe"
    } else {
        "rootshell-notify"
    };

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(binary_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // 3. Check standard fallback ~/.local/bin/rootshell-notify
    if let Some(home) = home {
        let fallback = home.join(".local").join("bin").join(binary_name);
        if fallback.is_file() {
            return Some(fallback);
        }
    }

    None
}

/// Dispatches an asynchronous push notification via rootshell-notify in the background.
pub(crate) fn send_push_notification(title: &str, body: Option<&str>, status: PushStatus) {
    let title = title.to_string();
    let body = body.map(str::to_string);

    std::thread::spawn(move || {
        let Some(bin) = find_rootshell_notify() else {
            debug!("rootshell-notify not available or not configured; skipping push");
            return;
        };

        let mut cmd = crate::noninteractive_process::command(bin);
        cmd.arg("send")
            .arg("--title")
            .arg(&title)
            .arg("--status")
            .arg(status.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        if let Some(b) = &body {
            let trimmed = b.trim();
            if !trimmed.is_empty() {
                cmd.arg("--body").arg(trimmed);
            }
        }

        match cmd.spawn() {
            Ok(mut child) => {
                debug!(
                    title = %title,
                    status = status.as_str(),
                    "rootshell-notify background process spawned"
                );
                let _ = child.wait();
            }
            Err(err) => {
                warn!(err = %err, "failed to spawn rootshell-notify");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_rootshell_notify_respects_missing_config() {
        // When config doesn't exist, it should return None without error
        let old = std::env::var_os("ROOTSHELL_PUSH_CONFIG");
        std::env::set_var(
            "ROOTSHELL_PUSH_CONFIG",
            "/nonexistent/rootshell-push/config.json",
        );
        assert!(find_rootshell_notify().is_none());
        match old {
            Some(v) => std::env::set_var("ROOTSHELL_PUSH_CONFIG", v),
            None => std::env::remove_var("ROOTSHELL_PUSH_CONFIG"),
        }
    }
}
