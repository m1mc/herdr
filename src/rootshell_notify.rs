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
///
/// `pane` is the public id of the local pane that raised it. rootshell-notify reads
/// `HERDR_PANE_ID` and `HERDR_SOCKET_PATH` to attach a Herdr route, which lets the
/// Rootshell app open that pane when the push is tapped.
pub(crate) fn send_push_notification(
    title: &str,
    body: Option<&str>,
    status: PushStatus,
    pane: Option<&str>,
) {
    let title = title.to_string();
    let body = body.map(str::to_string);
    let route = pane.map(|pane| (pane.to_string(), crate::api::socket_path()));

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
        set_pane_route(
            &mut cmd,
            route
                .as_ref()
                .map(|(pane, socket)| (pane.as_str(), socket.as_path())),
        );

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

/// Points rootshell-notify at the pane that raised the push, or clears the route.
fn set_pane_route(cmd: &mut std::process::Command, route: Option<(&str, &std::path::Path)>) {
    match route {
        Some((pane, socket)) => {
            cmd.env("HERDR_PANE_ID", pane)
                .env("HERDR_SOCKET_PATH", socket);
        }
        // An inherited pane id would route the push to whatever pane started this process.
        None => {
            cmd.env_remove("HERDR_PANE_ID")
                .env_remove("HERDR_SOCKET_PATH");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route_env(cmd: &std::process::Command) -> Vec<(String, Option<String>)> {
        let mut env: Vec<_> = cmd
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        env.sort();
        env
    }

    #[test]
    fn push_routes_to_the_raising_pane_and_never_an_inherited_one() {
        let mut cmd = std::process::Command::new("rootshell-notify");
        set_pane_route(
            &mut cmd,
            Some(("w1:p2", std::path::Path::new("/run/herdr.sock"))),
        );
        assert_eq!(
            route_env(&cmd),
            [
                ("HERDR_PANE_ID".to_owned(), Some("w1:p2".to_owned())),
                (
                    "HERDR_SOCKET_PATH".to_owned(),
                    Some("/run/herdr.sock".to_owned())
                ),
            ]
        );

        let mut cmd = std::process::Command::new("rootshell-notify");
        set_pane_route(&mut cmd, None);
        assert_eq!(
            route_env(&cmd),
            [
                ("HERDR_PANE_ID".to_owned(), None),
                ("HERDR_SOCKET_PATH".to_owned(), None),
            ]
        );
    }

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
